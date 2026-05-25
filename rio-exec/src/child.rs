//! The post-fork sandbox construction sequence.
//!
//! # Async-signal-safety is the law in this module
//!
//! Everything here runs in a forked child of a multi-threaded process,
//! between `fork(2)` and `execve(2)`. Only the forking thread survives
//! into the child; any lock another thread held at fork time — the
//! allocator's, a logger's, the runtime's — is held forever in the
//! child by a thread that no longer exists. Therefore: **no
//! allocation, no `format!`, no `panic!`/`unwrap`, no locks, no
//! `std::fs`**. Every path, string, and byte buffer the sequence needs
//! is precomputed into the [`SandboxPlan`] by the parent; every error
//! is returned as a fixed-size [`SetupError`] for the caller to write
//! to the status pipe with a single `write(2)`.
//!
//! # Process topology and the split entry points
//!
//! `unshare(CLONE_NEWPID)` does not move the calling process into the
//! new PID namespace — only its subsequently-forked children land
//! there. The executor therefore forks twice:
//!
//! ```text
//! parent (tokio)
//!   └─ intermediate      enter_namespaces(): go-pipe, unshare
//!        └─ sandbox child  setup_and_exec(): everything else, then execve
//! ```
//!
//! The *intermediate* calls [`enter_namespaces`] (read the go pipe,
//! unshare every namespace) and then forks the *sandbox child*, which
//! is pid 1 of the new PID namespace and calls [`setup_and_exec`]. The
//! intermediate waits for the sandbox child and forwards its exit
//! status. Both processes hold the status pipe's write end; a
//! [`SetupError`] from either is reported through
//! [`report_failure_and_exit`].
//!
//! The fork calls themselves, the pipe construction, and the
//! `waitpid` loop live with the `execute()` entry point, not here.

use std::os::fd::RawFd;

use nix::errno::Errno;

use crate::plan::{PlannedBind, SandboxPlan};
use crate::request::Personality;
use crate::seccomp;

/// File descriptors handed to the forked processes. All are raw fds
/// owned by the parent's wrappers; the child only `dup2`s, reads, and
/// closes them.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ChildFds {
    /// Write end of the setup-status pipe. `O_CLOEXEC`; survives until
    /// `execve` so the parent can distinguish "exec'd" (EOF with 0
    /// bytes) from "setup failed" (8 bytes then EOF).
    ///
    /// EOF only means "exec'd" once every other copy of the write end
    /// is gone, so the **intermediate must close its copy immediately
    /// after forking the sandbox child** — holding it open would delay
    /// the parent's exec notification until the whole build exits.
    pub status_pipe_w: RawFd,
    /// Read end of the go pipe. The intermediate blocks on it until
    /// the parent has attached it to the cgroup, so every descendant
    /// is accounted from its first instruction.
    pub go_pipe_r: RawFd,
    /// Becomes the sandboxed process's fd 1.
    pub stdout_fd: RawFd,
    /// Becomes the sandboxed process's fd 2 (the same fd as
    /// `stdout_fd` for merged-pty capture).
    pub stderr_fd: RawFd,
}

// ---------------------------------------------------------------------------
// SetupError: the fixed-size failure report.
// ---------------------------------------------------------------------------

/// Which step of the sandbox construction failed.
///
/// One variant per fallible step, in execution order, so a failure is
/// attributable to a phase without the child having to format a
/// message. New variants must be added to [`SetupPhase::ALL`] and to
/// the `from_u8` match — the round-trip unit test iterates `ALL`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum SetupPhase {
    /// Reading the go byte from the parent.
    GoPipe = 0,
    /// `unshare(2)` of the namespace set.
    Unshare = 1,
    /// `setsid(2)`.
    Setsid = 2,
    /// `dup2(2)` of the capture fds onto stdout/stderr or `/dev/null`
    /// onto stdin.
    DupStdio = 3,
    /// `close_range(2)` marking every inherited fd close-on-exec.
    CloseRange = 4,
    /// `sethostname(2)` / `setdomainname(2)`.
    SetHostname = 5,
    /// Recursively marking the inherited mount tree private.
    MountPrivate = 6,
    /// Bind-mounting the chroot directory onto itself.
    BindRoot = 7,
    /// A bind mount. The error's `detail` is the index into
    /// [`SandboxPlan::binds`].
    Bind = 8,
    /// The read-only remount of a bind. `detail` is the bind index.
    BindRemount = 9,
    /// A special mount (`proc`, `devpts`, `/dev/shm`). `detail` is the
    /// index into [`SandboxPlan::special_mounts`].
    MountSpecial = 10,
    /// `chdir(2)` into the chroot directory before `pivot_root`.
    ChdirRoot = 11,
    /// `pivot_root(2)`.
    PivotRoot = 12,
    /// `chroot(2)` to the post-pivot root.
    Chroot = 13,
    /// Detaching and removing the old root.
    UmountOldRoot = 14,
    /// `chdir(2)` into the requested working directory.
    ChdirCwd = 15,
    /// `personality(2)` with `PER_LINUX32`.
    Personality = 16,
    /// `prctl(PR_SET_NO_NEW_PRIVS)`.
    NoNewPrivs = 17,
    /// Installing the seccomp filter.
    Seccomp = 18,
    /// `setrlimit(RLIMIT_CORE)`.
    Rlimit = 19,
    /// Resetting signal dispositions and the signal mask.
    SignalReset = 20,
    /// `setgroups(2)`.
    Setgroups = 21,
    /// `setgid(2)`.
    Setgid = 22,
    /// `setuid(2)`.
    Setuid = 23,
    /// The post-drop verification that the real and effective ids
    /// actually changed.
    VerifyIds = 24,
    /// `prctl(PR_SET_PDEATHSIG)`.
    Pdeathsig = 25,
    /// `execve(2)` returned.
    Exec = 26,
    /// The intermediate process could not fork the sandbox child.
    /// Reported by the executor's supervision code (not by this
    /// module) so that a fork failure is distinguishable from a build
    /// that merely exited with an unusual status code.
    ForkSandboxChild = 27,
}

impl SetupPhase {
    /// Every variant, for the serialization round-trip test. Keep in
    /// sync with `from_u8`.
    pub const ALL: &'static [SetupPhase] = &[
        SetupPhase::GoPipe,
        SetupPhase::Unshare,
        SetupPhase::Setsid,
        SetupPhase::DupStdio,
        SetupPhase::CloseRange,
        SetupPhase::SetHostname,
        SetupPhase::MountPrivate,
        SetupPhase::BindRoot,
        SetupPhase::Bind,
        SetupPhase::BindRemount,
        SetupPhase::MountSpecial,
        SetupPhase::ChdirRoot,
        SetupPhase::PivotRoot,
        SetupPhase::Chroot,
        SetupPhase::UmountOldRoot,
        SetupPhase::ChdirCwd,
        SetupPhase::Personality,
        SetupPhase::NoNewPrivs,
        SetupPhase::Seccomp,
        SetupPhase::Rlimit,
        SetupPhase::SignalReset,
        SetupPhase::Setgroups,
        SetupPhase::Setgid,
        SetupPhase::Setuid,
        SetupPhase::VerifyIds,
        SetupPhase::Pdeathsig,
        SetupPhase::Exec,
        SetupPhase::ForkSandboxChild,
    ];

    /// Decode a wire discriminant. Exhaustive over every variant so a
    /// new phase that is not added here fails the round-trip test.
    fn from_u8(b: u8) -> Option<SetupPhase> {
        SetupPhase::ALL.iter().copied().find(|p| *p as u8 == b)
    }

    /// A static human-readable name for error messages assembled by the
    /// parent. The child never formats strings.
    pub fn describe(self) -> &'static str {
        match self {
            SetupPhase::GoPipe => "waiting for the start signal",
            SetupPhase::Unshare => "creating namespaces",
            SetupPhase::Setsid => "creating a session",
            SetupPhase::DupStdio => "wiring stdio",
            SetupPhase::CloseRange => "marking inherited fds close-on-exec",
            SetupPhase::SetHostname => "setting the hostname",
            SetupPhase::MountPrivate => "making the mount tree private",
            SetupPhase::BindRoot => "binding the sandbox root onto itself",
            SetupPhase::Bind => "applying a bind mount",
            SetupPhase::BindRemount => "remounting a bind read-only",
            SetupPhase::MountSpecial => "mounting a kernel filesystem",
            SetupPhase::ChdirRoot => "entering the sandbox root",
            SetupPhase::PivotRoot => "pivoting the root mount",
            SetupPhase::Chroot => "chrooting to the new root",
            SetupPhase::UmountOldRoot => "detaching the old root",
            SetupPhase::ChdirCwd => "entering the working directory",
            SetupPhase::Personality => "setting the architecture personality",
            SetupPhase::NoNewPrivs => "setting no_new_privs",
            SetupPhase::Seccomp => "installing the seccomp filter",
            SetupPhase::Rlimit => "setting resource limits",
            SetupPhase::SignalReset => "resetting signal dispositions",
            SetupPhase::Setgroups => "clearing supplementary groups",
            SetupPhase::Setgid => "dropping group privileges",
            SetupPhase::Setuid => "dropping user privileges",
            SetupPhase::VerifyIds => "verifying the privilege drop",
            SetupPhase::Pdeathsig => "arming the parent-death signal",
            SetupPhase::Exec => "executing the program",
            SetupPhase::ForkSandboxChild => "forking the sandbox child",
        }
    }
}

/// A sandbox-construction failure: which phase, which errno, and (for
/// indexed phases like [`SetupPhase::Bind`]) which entry.
///
/// Fixed-size and allocation-free so the child can serialize it with
/// [`to_bytes`](Self::to_bytes) and report it with a single `write(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupError {
    /// The step that failed.
    pub phase: SetupPhase,
    /// The raw errno from the failing syscall (0 when the failure is
    /// not a syscall failure, e.g. the id-verification mismatch).
    pub errno: i32,
    /// Phase-specific index: the failing entry in
    /// [`SandboxPlan::binds`] or [`SandboxPlan::special_mounts`].
    /// Zero for phases that are not indexed.
    pub detail: u16,
}

/// The number of bytes a serialized [`SetupError`] occupies on the
/// status pipe. Below `PIPE_BUF`, so the write is atomic.
pub(crate) const SETUP_ERROR_WIRE_LEN: usize = 8;

impl SetupError {
    fn new(phase: SetupPhase, errno: Errno) -> SetupError {
        SetupError {
            phase,
            errno: errno as i32,
            detail: 0,
        }
    }

    fn with_detail(phase: SetupPhase, errno: Errno, detail: u16) -> SetupError {
        SetupError {
            phase,
            errno: errno as i32,
            detail,
        }
    }

    /// Serialize to the fixed wire form: phase, a reserved zero byte,
    /// the detail index (LE), the errno (LE).
    pub(crate) fn to_bytes(self) -> [u8; SETUP_ERROR_WIRE_LEN] {
        let mut b = [0u8; SETUP_ERROR_WIRE_LEN];
        b[0] = self.phase as u8;
        b[1] = 0;
        b[2..4].copy_from_slice(&self.detail.to_le_bytes());
        b[4..8].copy_from_slice(&self.errno.to_le_bytes());
        b
    }

    /// Deserialize the wire form. `None` for an unknown phase
    /// discriminant (a corrupt or truncated report).
    pub(crate) fn from_bytes(b: &[u8; SETUP_ERROR_WIRE_LEN]) -> Option<SetupError> {
        Some(SetupError {
            phase: SetupPhase::from_u8(b[0])?,
            detail: u16::from_le_bytes([b[2], b[3]]),
            errno: i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
        })
    }
}

// ---------------------------------------------------------------------------
// Constants libc 0.2 does not export for linux-gnu.
// ---------------------------------------------------------------------------

/// `include/uapi/linux/close_range.h`: make the fds close-on-exec
/// instead of closing them immediately.
const CLOSE_RANGE_CLOEXEC: libc::c_uint = 1 << 2;

/// `include/uapi/linux/personality.h`. `PER_LINUX32` is a base
/// personality value (it replaces the whole word); `ADDR_NO_RANDOMIZE`
/// is a flag OR'd into it.
const PER_LINUX32: libc::c_ulong = 0x0008;
const ADDR_NO_RANDOMIZE: libc::c_ulong = 0x0004_0000;
/// The "query, do not change" argument to `personality(2)`.
const PERSONALITY_QUERY: libc::c_ulong = 0xffff_ffff;

// ---------------------------------------------------------------------------
// The intermediate's half.
// ---------------------------------------------------------------------------

/// The intermediate process's setup: wait for the parent's go signal,
/// then create every namespace.
///
/// Run by the first forked process. After this returns `Ok`, the
/// intermediate forks the sandbox child (which lands inside the new
/// PID namespace as pid 1) and waits for it.
///
/// # Safety
///
/// Must only be called between `fork` and `exec`/`_exit`, and only
/// with fds that stay open for the duration of the call.
pub(crate) fn enter_namespaces(plan: &SandboxPlan, fds: &ChildFds) -> Result<(), SetupError> {
    // Block until the parent has written the child into the cgroup.
    // EOF (0 bytes) means the parent died or gave up before releasing
    // us; treat it the same as a read error so we never run a build
    // outside its cgroup.
    let mut go = [0u8; 1];
    loop {
        // SAFETY: reading into a stack buffer from an fd the parent
        // keeps open until it writes the go byte.
        let n = unsafe { libc::read(fds.go_pipe_r, go.as_mut_ptr().cast(), 1) };
        match n {
            1 => break,
            0 => return Err(SetupError::new(SetupPhase::GoPipe, Errno::EPIPE)),
            _ if Errno::last() == Errno::EINTR => continue,
            _ => return Err(SetupError::new(SetupPhase::GoPipe, Errno::last())),
        }
    }
    // SAFETY: closing an fd this process owns.
    unsafe { libc::close(fds.go_pipe_r) };

    // SAFETY: unshare(2) with namespace flags has no memory
    // preconditions. CLONE_NEWPID affects only future children — the
    // sandbox child forked after this lands in the new namespace.
    let rc = unsafe { libc::unshare(plan.child.unshare_flags) };
    if rc != 0 {
        return Err(SetupError::new(SetupPhase::Unshare, Errno::last()));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The sandbox child's half.
// ---------------------------------------------------------------------------

/// The sandbox child's setup: everything between landing in the new
/// namespaces and `execve(2)`.
///
/// Only returns on failure — on success the process image is replaced.
/// The caller reports the returned error via
/// [`report_failure_and_exit`].
///
/// # Safety
///
/// Must only be called in a forked child (it irreversibly rearranges
/// the process's fd table, mount namespace, root directory, and
/// credentials), after [`enter_namespaces`] succeeded in its parent.
pub(crate) fn setup_and_exec(
    plan: &SandboxPlan,
    fds: &ChildFds,
    argv: &[*const libc::c_char],
    envp: &[*const libc::c_char],
) -> SetupError {
    match setup(plan, fds) {
        Ok(()) => {}
        Err(e) => return e,
    }
    // SAFETY: `program`, `argv`, and `envp` are NUL-terminated C
    // strings and NULL-terminated pointer arrays owned by the plan,
    // which outlives this call. execve only returns on failure.
    unsafe {
        libc::execve(plan.exec.program.as_ptr(), argv.as_ptr(), envp.as_ptr());
    }
    SetupError::new(SetupPhase::Exec, Errno::last())
}

/// The fallible body of [`setup_and_exec`], split out so `?` can be
/// used over the per-step helpers.
fn setup(plan: &SandboxPlan, fds: &ChildFds) -> Result<(), SetupError> {
    let c = &plan.child;

    // --- Session and stdio -------------------------------------------------
    // A fresh session so the pty the parent allocated can become the
    // controlling terminal and so a later kill of the process group
    // does not reach the executor.
    // SAFETY: no preconditions; fails only if we are already a group
    // leader, which a freshly forked process is not.
    if unsafe { libc::setsid() } < 0 {
        return Err(SetupError::new(SetupPhase::Setsid, Errno::last()));
    }
    // SAFETY: dup2 onto the well-known stdio fds; the source fds are
    // open by construction (the parent created them before forking).
    unsafe {
        if libc::dup2(fds.stdout_fd, 1) < 0 || libc::dup2(fds.stderr_fd, 2) < 0 {
            return Err(SetupError::new(SetupPhase::DupStdio, Errno::last()));
        }
        // stdin is /dev/null: the host's, opened before the fd table
        // is sealed — the sandbox's own /dev/null does not exist yet.
        let null = libc::open(c"/dev/null".as_ptr(), libc::O_RDONLY);
        if null < 0 || libc::dup2(null, 0) < 0 {
            return Err(SetupError::new(SetupPhase::DupStdio, Errno::last()));
        }
        if null > 2 {
            libc::close(null);
        }
    }
    // Mark every inherited fd >= 3 close-on-exec: the runtime's epoll
    // fd, the other ends of the executor's pipes, anything a library
    // opened — none of it may leak into the sandboxed process. The
    // status pipe is in that range *by design*: its close-on-exec is
    // what tells the parent the exec succeeded. The fds stay open (and
    // usable) until execve.
    // SAFETY: close_range(2) over a numeric range has no memory
    // preconditions.
    let rc = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3u32,
            libc::c_uint::MAX,
            CLOSE_RANGE_CLOEXEC,
        )
    };
    if rc != 0 {
        // No fallback: a silent fd leak into the sandbox is an
        // impurity that surfaces as unreproducible build behavior.
        // Every kernel this executor targets (>= 5.9) has
        // close_range(2).
        return Err(SetupError::new(SetupPhase::CloseRange, Errno::last()));
    }

    // --- Identity of the new namespaces ------------------------------------
    // SAFETY: sethostname/setdomainname copy from the provided buffer.
    unsafe {
        if libc::sethostname(c.hostname.as_ptr().cast(), c.hostname.len()) != 0 {
            return Err(SetupError::new(SetupPhase::SetHostname, Errno::last()));
        }
        // "(none)" is the kernel's own default for an unset NIS domain
        // name; setting it explicitly makes the value independent of
        // the host's.
        let domain = c"(none)";
        if libc::setdomainname(domain.as_ptr(), 6) != 0 {
            return Err(SetupError::new(SetupPhase::SetHostname, Errno::last()));
        }
    }

    // Loopback, best-effort: some test suites expect 127.0.0.1 to be
    // reachable. Requires CAP_NET_ADMIN over the new network
    // namespace; a deployment that dropped it gets a sandbox without
    // a routable loopback rather than no sandbox at all.
    if c.bring_up_loopback {
        bring_up_loopback();
    }

    // --- The mount tree ----------------------------------------------------
    apply_mounts(plan)?;

    // --- Enter the new root ------------------------------------------------
    // SAFETY: chdir/pivot_root/chroot/umount2/rmdir over NUL-terminated
    // paths precomputed in the plan.
    unsafe {
        if libc::chdir(c.chroot_dir_c.as_ptr()) != 0 {
            return Err(SetupError::new(SetupPhase::ChdirRoot, Errno::last()));
        }
        // pivot_root(".", ".real-root"): the new root is the cwd, the
        // old root lands on .real-root inside it. Requires both to be
        // mount points (the chroot dir was bound onto itself above)
        // and the caller to be in the mount namespace that owns them.
        if libc::syscall(
            libc::SYS_pivot_root,
            c".".as_ptr(),
            crate::plan::PIVOT_OLD_ROOT_C.as_ptr(),
        ) != 0
        {
            return Err(SetupError::new(SetupPhase::PivotRoot, Errno::last()));
        }
        // pivot_root already re-points the root and cwd of every
        // process that had the old root; the explicit chroot+chdir is
        // belt-and-suspenders that also covers kernels where the cwd
        // update is not guaranteed (pivot_root(2) NOTES).
        if libc::chroot(c".".as_ptr()) != 0 {
            return Err(SetupError::new(SetupPhase::Chroot, Errno::last()));
        }
        if libc::chdir(c"/".as_ptr()) != 0 {
            return Err(SetupError::new(SetupPhase::Chroot, Errno::last()));
        }
        // Drop the old root: lazily detach the mount (everything the
        // host had mounted is still busy from the host's side, hence
        // MNT_DETACH) and remove the now-empty directory so the
        // sandbox does not contain a mysterious /.real-root.
        if libc::umount2(c.pivot_old_root_abs_c.as_ptr(), libc::MNT_DETACH) != 0 {
            return Err(SetupError::new(SetupPhase::UmountOldRoot, Errno::last()));
        }
        if libc::rmdir(c.pivot_old_root_abs_c.as_ptr()) != 0 {
            return Err(SetupError::new(SetupPhase::UmountOldRoot, Errno::last()));
        }
        if libc::chdir(c.cwd_c.as_ptr()) != 0 {
            return Err(SetupError::new(SetupPhase::ChdirCwd, Errno::last()));
        }
    }

    // --- Hardening ---------------------------------------------------------
    apply_personality(c.personality)?;
    // SAFETY: prctl with integer arguments.
    if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
        return Err(SetupError::new(SetupPhase::NoNewPrivs, Errno::last()));
    }
    if let Some(prog) = &c.seccomp_program {
        // Failure is fatal: a sandbox that silently runs without the
        // purity filter produces outputs whose setuid bits and xattrs
        // are silently stripped during archiving instead of failing
        // the operation inside the build.
        seccomp::install(prog).map_err(|e| SetupError::new(SetupPhase::Seccomp, e))?;
    }
    // Core dumps off: a crashing build tool must not leave multi-GB
    // core files in the output tree.
    let no_core = libc::rlimit {
        rlim_cur: 0,
        rlim_max: libc::RLIM_INFINITY,
    };
    // SAFETY: setrlimit reads the struct.
    if unsafe { libc::setrlimit(libc::RLIMIT_CORE, &no_core) } != 0 {
        return Err(SetupError::new(SetupPhase::Rlimit, Errno::last()));
    }
    // A deterministic umask and default signal dispositions: the
    // executor's own mask and handlers (tokio installs several) must
    // not be observable inside the sandbox.
    // SAFETY: umask cannot fail; signal/sigprocmask with valid
    // arguments.
    unsafe {
        libc::umask(0o022);
        for sig in [
            libc::SIGCHLD,
            libc::SIGPIPE,
            libc::SIGTERM,
            libc::SIGINT,
            libc::SIGHUP,
            libc::SIGQUIT,
        ] {
            if libc::signal(sig, libc::SIG_DFL) == libc::SIG_ERR {
                return Err(SetupError::new(SetupPhase::SignalReset, Errno::last()));
            }
        }
        let mut empty: libc::sigset_t = std::mem::zeroed();
        if libc::sigemptyset(&mut empty) != 0
            || libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) != 0
        {
            return Err(SetupError::new(SetupPhase::SignalReset, Errno::last()));
        }
    }

    // --- Privilege drop ----------------------------------------------------
    // Order matters: groups and gid before uid, because after setuid
    // the process no longer has the privilege to change them.
    // SAFETY: plain credential syscalls.
    unsafe {
        if libc::setgroups(0, std::ptr::null()) != 0 {
            return Err(SetupError::new(SetupPhase::Setgroups, Errno::last()));
        }
        if libc::setgid(c.gid) != 0 {
            return Err(SetupError::new(SetupPhase::Setgid, Errno::last()));
        }
        if libc::setuid(c.uid) != 0 {
            return Err(SetupError::new(SetupPhase::Setuid, Errno::last()));
        }
        // A setuid that "succeeds" without actually changing the ids
        // is the classic privilege-drop bug; verify all four.
        if libc::getuid() != c.uid
            || libc::geteuid() != c.uid
            || libc::getgid() != c.gid
            || libc::getegid() != c.gid
        {
            return Err(SetupError {
                phase: SetupPhase::VerifyIds,
                errno: 0,
                detail: 0,
            });
        }
        // setuid clears the parent-death signal; re-arm it so the
        // sandboxed process tree dies with the executor instead of
        // leaking. The traditional getppid()-changed race check is
        // meaningless here: this process is pid 1 of its PID
        // namespace, so getppid() always returns 0 regardless of
        // whether the real parent is alive. The race window (parent
        // dying between fork and this prctl) is closed by the
        // executor's cgroup kill on its own teardown path instead.
        if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) != 0 {
            return Err(SetupError::new(SetupPhase::Pdeathsig, Errno::last()));
        }
    }

    Ok(())
}

/// Make the inherited mount tree private, bind the chroot root onto
/// itself, then apply every planned bind and special mount in order.
fn apply_mounts(plan: &SandboxPlan) -> Result<(), SetupError> {
    let c = &plan.child;
    // SAFETY: mount(2) over NUL-terminated paths precomputed in the
    // plan; the kernel copies every string argument.
    unsafe {
        // Stop every mount event from propagating back to the host's
        // namespace. MS_REC because the inherited tree can have shared
        // subtrees anywhere under /.
        if libc::mount(
            std::ptr::null(),
            c"/".as_ptr(),
            std::ptr::null(),
            libc::MS_PRIVATE | libc::MS_REC,
            std::ptr::null(),
        ) != 0
        {
            return Err(SetupError::new(SetupPhase::MountPrivate, Errno::last()));
        }
        // pivot_root requires the new root to be a mount point.
        if libc::mount(
            c.chroot_dir_c.as_ptr(),
            c.chroot_dir_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ) != 0
        {
            return Err(SetupError::new(SetupPhase::BindRoot, Errno::last()));
        }
    }

    for (i, bind) in plan.binds.iter().enumerate() {
        if bind.skipped {
            continue;
        }
        let idx = index_u16(i);
        apply_bind(bind, idx)?;
    }

    for (i, sm) in plan.special_mounts.iter().enumerate() {
        let idx = index_u16(i);
        // SAFETY: mount(2) with precomputed NUL-terminated strings.
        let rc = unsafe {
            libc::mount(
                sm.fstype.as_ptr(),
                sm.target_in_chroot_c.as_ptr(),
                sm.fstype.as_ptr(),
                sm.flags,
                sm.data.as_ptr().cast(),
            )
        };
        if rc != 0 {
            return Err(SetupError::with_detail(
                SetupPhase::MountSpecial,
                Errno::last(),
                idx,
            ));
        }
    }
    Ok(())
}

/// Apply one bind mount and, for read-only binds, the remount that
/// actually enforces read-onlyness.
fn apply_bind(bind: &PlannedBind, idx: u16) -> Result<(), SetupError> {
    // SAFETY: mount(2) with precomputed NUL-terminated strings.
    unsafe {
        // MS_REC so a source that itself contains mount points is
        // carried over whole rather than as an empty stub.
        if libc::mount(
            bind.source_c.as_ptr(),
            bind.target_in_chroot_c.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        ) != 0
        {
            return Err(SetupError::with_detail(
                SetupPhase::Bind,
                Errno::last(),
                idx,
            ));
        }
        if bind.read_only {
            // A bind mount ignores MS_RDONLY on creation; making it
            // read-only takes a second, remounting call. The remount is
            // non-recursive, so read-onlyness is only guaranteed for
            // the top mount: a source that itself contained submounts
            // would carry them over (MS_REC above) still writable. The
            // caller contract (see `Mount::writable`) is therefore that
            // read-only mount sources must not contain submounts —
            // true for every source the intended callers bind. This is
            // a deliberate divergence from implementations that rely on
            // file ownership alone for read-onlyness — the observable
            // difference is EROFS instead of EACCES for a write
            // attempt.
            if libc::mount(
                std::ptr::null(),
                bind.target_in_chroot_c.as_ptr(),
                std::ptr::null(),
                libc::MS_REMOUNT | libc::MS_BIND | libc::MS_RDONLY,
                std::ptr::null(),
            ) != 0
            {
                return Err(SetupError::with_detail(
                    SetupPhase::BindRemount,
                    Errno::last(),
                    idx,
                ));
            }
        }
    }
    Ok(())
}

/// Apply the requested `personality(2)` configuration.
fn apply_personality(p: Personality) -> Result<(), SetupError> {
    // SAFETY: personality(2) takes and returns an integer.
    unsafe {
        match p {
            Personality::Native => {}
            Personality::Linux32 => {
                // The 32-bit base personality replaces the whole word;
                // a process that cannot get it would run the 64-bit
                // toolchain's `uname -m` answer inside a build that
                // declared itself 32-bit, so failure is fatal.
                if libc::personality(PER_LINUX32) < 0 {
                    return Err(SetupError::new(SetupPhase::Personality, Errno::last()));
                }
            }
        }
        // Address-space randomization off, best-effort: deployments
        // whose own seccomp policy blocks personality(2) values with
        // this bit get ASLR-on builds, which is also what they get
        // from every other sandbox implementation that ignores this
        // call's result. Reproducible builds cannot depend on ASLR
        // being off either way.
        let cur = libc::personality(PERSONALITY_QUERY);
        if cur >= 0 {
            let _ = libc::personality(cur as libc::c_ulong | ADDR_NO_RANDOMIZE);
        }
    }
    Ok(())
}

/// Bring up the loopback interface in the (freshly unshared) network
/// namespace. Best-effort by design — see the call site.
fn bring_up_loopback() {
    // SAFETY: socket/ioctl/close over a stack-allocated ifreq. The
    // interface name "lo" (3 bytes with NUL) fits IFNAMSIZ.
    unsafe {
        let fd = libc::socket(libc::AF_INET, libc::SOCK_DGRAM, 0);
        if fd < 0 {
            return;
        }
        let mut ifr: libc::ifreq = std::mem::zeroed();
        ifr.ifr_name[0] = b'l' as libc::c_char;
        ifr.ifr_name[1] = b'o' as libc::c_char;
        if libc::ioctl(fd, libc::SIOCGIFFLAGS, &mut ifr) == 0 {
            ifr.ifr_ifru.ifru_flags |= libc::IFF_UP as libc::c_short;
            let _ = libc::ioctl(fd, libc::SIOCSIFFLAGS, &ifr);
        }
        libc::close(fd);
    }
}

/// `i` as a `u16` wire index, saturating: a plan with more than 65535
/// binds would mis-attribute the failing entry but cannot be made to
/// misbehave (the index is diagnostic only).
fn index_u16(i: usize) -> u16 {
    u16::try_from(i).unwrap_or(u16::MAX)
}

/// Write `err` to the status pipe and `_exit(127)`.
///
/// The single 8-byte write is atomic (well under `PIPE_BUF`); a failed
/// write cannot be reported anywhere, so it is ignored — the parent
/// then sees EOF with 0 bytes and a 127 exit, which it reports as an
/// unattributed setup failure.
pub(crate) fn report_failure_and_exit(status_pipe_w: RawFd, err: &SetupError) -> ! {
    let buf = err.to_bytes();
    // SAFETY: writing a stack buffer to an fd this process owns;
    // _exit(2) is async-signal-safe and does not run destructors or
    // atexit handlers (which could touch the poisoned allocator).
    unsafe {
        let _ = libc::write(status_pipe_w, buf.as_ptr().cast(), buf.len());
        libc::_exit(127);
    }
}

/// `_exit(2)` without running any Rust or libc teardown. Used by the
/// intermediate to forward the sandbox child's exit status.
pub(crate) fn exit_immediately(status: i32) -> ! {
    // SAFETY: _exit is always safe to call.
    unsafe { libc::_exit(status) }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every phase must survive the wire round-trip: a phase that is
    /// added to the enum but not to `ALL`/`from_u8` would deserialize
    /// to `None` and the parent would report an unattributed failure.
    #[test]
    fn setup_error_round_trips_every_phase() {
        for (i, &phase) in SetupPhase::ALL.iter().enumerate() {
            let err = SetupError {
                phase,
                errno: 1000 + i32::try_from(i).expect("phase count fits i32"),
                detail: u16::try_from(i).expect("phase count fits u16") * 3,
            };
            let decoded = SetupError::from_bytes(&err.to_bytes())
                .unwrap_or_else(|| panic!("{phase:?} did not round-trip"));
            assert_eq!(decoded, err, "{phase:?} round-trip mismatch");
        }
    }

    /// Negative errnos (impossible, but the field is i32) and the full
    /// detail range survive the round trip.
    #[test]
    fn setup_error_round_trips_extreme_values() {
        let err = SetupError {
            phase: SetupPhase::Bind,
            errno: i32::MIN,
            detail: u16::MAX,
        };
        assert_eq!(SetupError::from_bytes(&err.to_bytes()), Some(err));
    }

    /// An unknown phase discriminant decodes to `None` rather than a
    /// wrong phase.
    #[test]
    fn setup_error_rejects_unknown_phase() {
        let mut bytes = SetupError::new(SetupPhase::Exec, Errno::ENOENT).to_bytes();
        bytes[0] = 0xFF;
        assert_eq!(SetupError::from_bytes(&bytes), None);
    }

    /// `ALL` and the discriminants stay in sync: every discriminant is
    /// unique and `from_u8` inverts `as u8` for every variant.
    #[test]
    fn setup_phase_discriminants_are_unique_and_decodable() {
        let mut seen = std::collections::HashSet::new();
        for &p in SetupPhase::ALL {
            assert!(seen.insert(p as u8), "duplicate discriminant for {p:?}");
            assert_eq!(SetupPhase::from_u8(p as u8), Some(p));
        }
    }

    /// `describe` is total over `ALL` (a new phase without a
    /// description would be a compile error thanks to the exhaustive
    /// match, but this keeps the strings non-empty).
    #[test]
    fn every_phase_has_a_description() {
        for &p in SetupPhase::ALL {
            assert!(!p.describe().is_empty());
        }
    }
}
