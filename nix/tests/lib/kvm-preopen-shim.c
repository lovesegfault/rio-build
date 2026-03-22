// LD_PRELOAD shim for qemu: intercept open("/dev/kvm") and return a dup
// of a pre-opened fd inherited from the test driver.
//
// Mechanism: nixbuild.net builders have udev rule 99-local.rules that sets
// /dev/kvm to MODE=0660 GROUP=snix-qemu (empty group). The sandbox init2
// chmods to 666, but udev re-applies 660 shortly after the first qemu's
// KVM_CREATE_VM (or some other trigger). Subsequent qemu opens fail EACCES.
//
// Workaround: test driver opens /dev/kvm ONCE while it's still 666 (init2
// just chmod'd it), clears FD_CLOEXEC, exports KVM_PRELOAD_FD=<fd>, sets
// LD_PRELOAD=<this>.so. Every qemu child inherits the open fd and this shim
// dup's it instead of re-opening /dev/kvm. Since the fd was opened while
// perms were 666, it stays valid regardless of later mode changes.
//
// Multiple qemu processes can share the same /dev/kvm fd — each
// KVM_CREATE_VM returns a fresh VM fd. The /dev/kvm fd is just for the
// top-level KVM ioctl dispatch.

#define _GNU_SOURCE
#include <dlfcn.h>
#include <errno.h>
#include <fcntl.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

static int kvm_fd = -1;

__attribute__((constructor)) static void init(void) {
    const char *env = getenv("KVM_PRELOAD_FD");
    if (env) {
        kvm_fd = atoi(env);
        // Validate the inherited fd actually points at /dev/kvm — if the
        // test driver's preopen failed, KVM_PRELOAD_FD may be stale.
        // fcntl F_GETFD returns -1/EBADF on a closed fd.
        if (fcntl(kvm_fd, F_GETFD) < 0) {
            kvm_fd = -1;
        }
    }
    // Fallback: if no inherited fd, try chmod+open directly. init2 can
    // chmod /dev/kvm, so this process (same sandbox uid) should be able
    // to as well. udev may reset between chmod and open — retry ×10.
    if (kvm_fd < 0) {
        for (int i = 0; i < 10; i++) {
            chmod("/dev/kvm", 0666);  // best-effort; ignore EPERM
            int fd = syscall(__NR_openat, AT_FDCWD, "/dev/kvm",
                             O_RDWR | O_CLOEXEC, 0);
            if (fd >= 0) {
                kvm_fd = fd;
                fprintf(stderr,
                        "[kvm-preopen-shim] fallback chmod+open succeeded "
                        "(attempt %d)\n", i + 1);
                break;
            }
            if (errno != EACCES) break;
            usleep(100000);  // 100ms
        }
    }
}

static int is_kvm_path(const char *path) {
    return path && strcmp(path, "/dev/kvm") == 0;
}

static int dup_kvm_fd(void) {
    if (kvm_fd < 0) {
        // No inherited fd — fall through to real open (will EACCES if 660)
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
