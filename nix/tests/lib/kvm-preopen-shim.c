// LD_PRELOAD shim for qemu: intercept open("/dev/kvm") and return a dup
// of a pre-opened fd inherited from the test driver.
//
// Mechanism: nixbuild.net builders have udev rule 99-local.rules that sets
// /dev/kvm to MODE=0660 GROUP=snix-qemu (empty group). The sandbox init2
// chmods to 666, but udev re-applies 660 whenever ANY concurrent build's
// qemu does KVM_CREATE_VM. Under high concurrency the 666 state is
// microseconds-transient.
//
// Primary path: test driver pre-opens /dev/kvm (inotify-driven, see
// kvm-preopen.nix), exports KVM_PRELOAD_FD=<fd>, sets LD_PRELOAD=<this>.so.
// Every qemu child inherits the open fd and this shim dup's it instead of
// re-opening /dev/kvm. Since the fd was opened while perms were 666, it
// stays valid regardless of later mode changes.
//
// Fallback path: if KVM_PRELOAD_FD is unset/invalid, the constructor does
// its own inotify-driven wait. This matters for late-starting qemus in
// multi-VM tests — a qemu that starts 60s into the test run might catch a
// 666 window the preamble missed.
//
// Multiple qemu processes can share the same /dev/kvm fd — each
// KVM_CREATE_VM returns a fresh VM fd. The /dev/kvm fd is just for the
// top-level KVM ioctl dispatch.
//
// See kvm-preopen.nix header for the full investigation of why chmod,
// setfacl, O_PATH, caps, and mknod all fail from inside the sandbox.

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/inotify.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int kvm_fd = -1;

// Direct syscall — avoid re-entering our own open() interceptor.
static int raw_open_kvm(void) {
    return syscall(__NR_openat, AT_FDCWD, "/dev/kvm", O_RDWR | O_CLOEXEC, 0);
}

// 1ms-poll fallback — used when inotify setup fails at any stage.
static int poll_open_kvm(int timeout_ms) {
    for (int i = 0; i < timeout_ms; i++) {
        int fd = raw_open_kvm();
        if (fd >= 0) return fd;
        if (errno != EACCES) return -1;
        usleep(1000);
    }
    return -1;
}

// inotify-driven wait for a 666 window. Watches /dev DIRECTORY (not
// /dev/kvm — inotify_add_watch requires read perm on the target, and
// /dev/kvm is 660 with us not in-group; /dev is 755). Directory watches
// fire IN_ATTRIB for contained entries; filter by name=="kvm". Every
// concurrent sandbox's init2 chmod(666) fires an event; we race open()
// before the next udev reset. timeout_ms bounds the total wait.
static int wait_and_open_kvm(int timeout_ms) {
    // Try immediate first — the test driver may have just started us
    // right after a 666 window.
    int fd = raw_open_kvm();
    if (fd >= 0) return fd;
    if (errno != EACCES) return -1;

    int ifd = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
    if (ifd < 0) return poll_open_kvm(timeout_ms);

    int wd = inotify_add_watch(ifd, "/dev", IN_ATTRIB);
    if (wd < 0) {
        close(ifd);
        return poll_open_kvm(timeout_ms);  // fallback, don't just give up
    }

    // Re-check after watch install — event may have raced setup.
    fd = raw_open_kvm();
    if (fd >= 0) { close(ifd); return fd; }

    struct pollfd pfd = { .fd = ifd, .events = POLLIN };
    int remaining = timeout_ms;
    int kvm_events = 0;
    while (remaining > 0) {
        int r = poll(&pfd, 1, remaining < 1000 ? remaining : 1000);
        int saw_kvm = 0;
        if (r > 0) {
            // Drain and parse — filter to name=="kvm" so we don't spin
            // on unrelated /dev churn (pts, tty, etc.).
            char buf[4096];
            ssize_t n;
            while ((n = read(ifd, buf, sizeof(buf))) > 0) {
                for (char *p = buf; p < buf + n; ) {
                    struct inotify_event *ev = (struct inotify_event *)p;
                    if (ev->len > 0 && strcmp(ev->name, "kvm") == 0) {
                        saw_kvm = 1;
                        kvm_events++;
                    }
                    p += sizeof(struct inotify_event) + ev->len;
                }
            }
        }
        // Try open on kvm event OR 1s tick (belt-and-suspenders).
        if (saw_kvm || r == 0) {
            fd = raw_open_kvm();
            if (fd >= 0) {
                fprintf(stderr,
                        "[kvm-preopen-shim] fallback inotify open succeeded "
                        "(kvm-events=%d, %dms remaining)\n",
                        kvm_events, remaining);
                close(ifd);
                return fd;
            }
            if (errno != EACCES) break;
        }
        remaining -= 1000;
    }
    fprintf(stderr,
            "[kvm-preopen-shim] fallback: no 666-window in %dms "
            "(%d kvm IN_ATTRIB events seen)\n", timeout_ms, kvm_events);
    close(ifd);
    return -1;
}

__attribute__((constructor)) static void init(void) {
    const char *env = getenv("KVM_PRELOAD_FD");
    if (env) {
        kvm_fd = atoi(env);
        // Validate the inherited fd — if the test driver's preopen failed,
        // KVM_PRELOAD_FD may be stale. fcntl F_GETFD → -1/EBADF if closed.
        if (fcntl(kvm_fd, F_GETFD) < 0) {
            kvm_fd = -1;
        }
    }
    // Fallback: no inherited fd → inotify-driven wait. 10s budget — qemu
    // startup is already slow, and a late-starting qemu in a multi-VM test
    // might catch a window the preamble missed 60s ago.
    if (kvm_fd < 0) {
        kvm_fd = wait_and_open_kvm(10000);
    }
}

static int is_kvm_path(const char *path) {
    return path && strcmp(path, "/dev/kvm") == 0;
}

static int dup_kvm_fd(void) {
    if (kvm_fd < 0) {
        // No fd — fall through to real open (will EACCES if 660).
        return -1;
    }
    return dup(kvm_fd);
}

int open(const char *path, int flags, ...) {
    static int (*real_open)(const char *, int, ...) = NULL;
    if (!real_open) real_open = dlsym(RTLD_NEXT, "open");

    if (is_kvm_path(path)) {
        int fd = dup_kvm_fd();
        if (fd >= 0) return fd;
    }

    va_list ap;
    va_start(ap, flags);
    mode_t mode = (flags & O_CREAT) ? va_arg(ap, mode_t) : 0;
    va_end(ap);
    return real_open(path, flags, mode);
}

int open64(const char *path, int flags, ...) {
    static int (*real_open64)(const char *, int, ...) = NULL;
    if (!real_open64) real_open64 = dlsym(RTLD_NEXT, "open64");

    if (is_kvm_path(path)) {
        int fd = dup_kvm_fd();
        if (fd >= 0) return fd;
    }

    va_list ap;
    va_start(ap, flags);
    mode_t mode = (flags & O_CREAT) ? va_arg(ap, mode_t) : 0;
    va_end(ap);
    return real_open64(path, flags, mode);
}

int openat(int dirfd, const char *path, int flags, ...) {
    static int (*real_openat)(int, const char *, int, ...) = NULL;
    if (!real_openat) real_openat = dlsym(RTLD_NEXT, "openat");

    // qemu uses absolute path for /dev/kvm, so dirfd is ignored
    if (is_kvm_path(path)) {
        int fd = dup_kvm_fd();
        if (fd >= 0) return fd;
    }

    va_list ap;
    va_start(ap, flags);
    mode_t mode = (flags & O_CREAT) ? va_arg(ap, mode_t) : 0;
    va_end(ap);
    return real_openat(dirfd, path, flags, mode);
}

int openat64(int dirfd, const char *path, int flags, ...) {
    static int (*real_openat64)(int, const char *, int, ...) = NULL;
    if (!real_openat64) real_openat64 = dlsym(RTLD_NEXT, "openat64");

    if (is_kvm_path(path)) {
        int fd = dup_kvm_fd();
        if (fd >= 0) return fd;
    }

    va_list ap;
    va_start(ap, flags);
    mode_t mode = (flags & O_CREAT) ? va_arg(ap, mode_t) : 0;
    va_end(ap);
    return real_openat64(dirfd, path, flags, mode);
}
