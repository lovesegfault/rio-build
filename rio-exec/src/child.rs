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

/// File descriptors handed to the forked processes — and, just as
/// importantly, the complete **keep set**: these four fds (plus stdio
/// 0–2) are the only file descriptors a forked executor process may
/// retain. [`shed_inherited_fds`] closes everything else as the
/// intermediate's first act, which is what makes the executor's
/// fd-based EOF protocols trustworthy:
///
/// * go-pipe EOF means "the parent died" only because the intermediate
///   no longer holds an inherited copy of the pipe's write end;
/// * status-pipe EOF means "the program exec'd" only because every
///   surviving copy of the write end is close-on-exec.
///
/// All are raw fds owned by the parent's wrappers; the child only
/// `dup2`s, reads, and closes them.
///
/// **The go pipe's write end must never become a field here.** The
/// keep set is what the go-pipe *reader* retains; keeping the write
/// end too would mean the reader waits on an EOF that can never
/// arrive, silently re-breaking the parent-death protocol this type
/// exists to guarantee.
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
    /// `close_range(2)` sweep of every inherited fd outside the keep
    /// set, run as the intermediate's first statement. Declared first
    /// because it executes first; the wire discriminant is append-only
    /// (28) so existing decoders are unaffected.
    FdSweep = 28,
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
    /// A bind mount. The error's `detail` is the index into the
    /// sandbox plan's bind list (`SandboxPlan::binds`).
    Bind = 8,
    /// The read-only remount of a bind. `detail` is the bind index.
    BindRemount = 9,
    /// A special mount (`proc`, `devpts`, `/dev/shm`). `detail` is the
    /// index into the sandbox plan's special-mount list
    /// (`SandboxPlan::special_mounts`).
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
    /// `prctl(PR_SET_PDEATHSIG)` — the post-credential-drop re-arm.
    Pdeathsig = 25,
    /// `execve(2)` returned.
    Exec = 26,
    /// The intermediate process could not fork the sandbox child.
    /// Reported by the executor's supervision code (not by this
    /// module) so that a fork failure is distinguishable from a build
    /// that merely exited with an unusual status code.
    ForkSandboxChild = 27,
    /// `prctl(PR_SET_PDEATHSIG)` — the arm that runs as the sandbox
    /// child's *first* setup statement, before any other step, so
    /// intermediate death cascades during setup too. Declared in
    /// execution order (right after the fork); the wire discriminant
    /// is append-only (29).
    PdeathsigEarly = 29,
}

impl SetupPhase {
    /// Every variant, for the serialization round-trip test. Keep in
    /// sync with `from_u8`. Ordered by execution order (which is why
    /// [`SetupPhase::FdSweep`] leads despite its append-only wire
    /// discriminant).
    pub const ALL: &'static [SetupPhase] = &[
        SetupPhase::FdSweep,
        SetupPhase::GoPipe,
        SetupPhase::Unshare,
        SetupPhase::ForkSandboxChild,
        SetupPhase::PdeathsigEarly,
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
            SetupPhase::FdSweep => "shedding inherited file descriptors",
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
            SetupPhase::Pdeathsig => "re-arming the parent-death signal",
            SetupPhase::PdeathsigEarly => "arming the parent-death signal",
            SetupPhase::Exec => "executing the program",
            SetupPhase::ForkSandboxChild => "forking the sandbox child",
        }
    }
}

/// A sandbox-construction failure: which phase, which errno, and (for
/// indexed phases like [`SetupPhase::Bind`]) which entry.
///
/// Fixed-size and allocation-free so the child can serialize it with
/// `SetupError::to_bytes` and report it with a single `write(2)`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SetupError {
    /// The step that failed.
    pub phase: SetupPhase,
    /// The raw errno from the failing syscall (0 when the failure is
    /// not a syscall failure, e.g. the id-verification mismatch).
    pub errno: i32,
    /// Phase-specific index: the failing entry in the sandbox plan's
    /// bind or special-mount list (`SandboxPlan::binds` /
    /// `SandboxPlan::special_mounts`). Zero for phases that are not
    /// indexed.
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

/// The `RLIMIT_NOFILE` soft and hard limit a build sees. CppNix leaves
/// the limit untouched, so daemon-era builders inherited
/// nix-daemon.service's `LimitNOFILE=1048576`; pinning the same value
/// here keeps the build-visible limit identical across delivery paths
/// (pod, systemd unit, test harness) and identical to what packages
/// were historically built under.
const SANDBOX_NOFILE_LIMIT: libc::rlim_t = 1_048_576;

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

/// Close every fd in `[first, last]` (really close, not mark
/// close-on-exec) with one `close_range(2)` call.
///
/// Async-signal-safe: a single syscall, no allocation.
fn close_fd_range(first: libc::c_uint, last: libc::c_uint) -> Result<(), Errno> {
    // SAFETY: close_range(2) over a numeric range has no memory
    // preconditions.
    let rc = unsafe { libc::syscall(libc::SYS_close_range, first, last, 0u32) };
    if rc != 0 {
        return Err(Errno::last());
    }
    Ok(())
}

/// Close every inherited file descriptor outside the keep set.
///
/// Run as the **first statement** of the intermediate process — before
/// its first blocking read. The keep set is exactly the four
/// [`ChildFds`] fds plus stdio (0–2); everything else the fork
/// duplicated — the parent's ends of every pipe (including the go
/// pipe's write end), the pty master, the async runtime's internals —
/// is closed via `close_range(2)` over the gaps between the (sorted)
/// kept fds.
///
/// This sweep is what makes the executor's fd-based EOF protocols
/// valid (see [`ChildFds`]): no forked process may wait on a pipe end
/// it itself holds a copy of, and a future fd added to the parent
/// cannot silently leak into the sandbox tree.
///
/// # Safety contract
///
/// Async-signal-safe: stack-only bookkeeping (at most 4 fds, sorted in
/// place) plus `close_range(2)` syscalls. Must only be called between
/// `fork` and `exec`/`_exit`.
// r[impl builder.exec.fd-keep-set]
pub(crate) fn shed_inherited_fds(keep: &ChildFds) -> Result<(), SetupError> {
    let mut kept: [i64; 4] = [
        i64::from(keep.status_pipe_w),
        i64::from(keep.go_pipe_r),
        i64::from(keep.stdout_fd),
        i64::from(keep.stderr_fd),
    ];
    kept.sort_unstable();

    // Close the gaps between kept fds, starting at 3 (stdio stays).
    let mut next: i64 = 3;
    for &fd in &kept {
        if fd < next {
            // Stdio-range or duplicate keep fd: nothing to close below it.
            continue;
        }
        if fd > next {
            close_fd_range(next as libc::c_uint, (fd - 1) as libc::c_uint)
                .map_err(|e| SetupError::new(SetupPhase::FdSweep, e))?;
        }
        next = fd + 1;
    }
    if next <= i64::from(libc::c_uint::MAX) {
        close_fd_range(next as libc::c_uint, libc::c_uint::MAX)
            .map_err(|e| SetupError::new(SetupPhase::FdSweep, e))?;
    }
    Ok(())
}

/// Block until the parent writes the go byte — or report that the
/// parent died first.
///
/// EOF (0 bytes) means every copy of the go pipe's write end is gone:
/// the parent died or gave up before releasing this process, and the
/// build must not run (it was never attached to its cgroup). That EOF
/// can only arrive because [`shed_inherited_fds`] already closed this
/// process's own inherited copy of the write end — before the sweep
/// existed, this branch was structurally dead code (the reader held
/// the very write end it was waiting on) and a crashed parent left the
/// intermediate parked here forever.
///
/// On success the go-pipe read end is closed (it has served its
/// purpose).
///
/// # Safety contract
///
/// Must only be called between `fork` and `exec`/`_exit`, after
/// [`shed_inherited_fds`].
// r[impl builder.exec.fd-keep-set]
pub(crate) fn await_go_signal(fds: &ChildFds) -> Result<(), SetupError> {
    let mut go = [0u8; 1];
    loop {
        // SAFETY: reading into a stack buffer from an fd this process
        // owns.
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
    Ok(())
}

/// The intermediate process's setup: wait for the parent's go signal,
/// then create every namespace.
///
/// Run by the first forked process, after [`shed_inherited_fds`].
/// After this returns `Ok`, the intermediate forks the sandbox child
/// (which lands inside the new PID namespace as pid 1) and waits for
/// it.
///
/// # Safety
///
/// Must only be called between `fork` and `exec`/`_exit`, and only
/// with fds that stay open for the duration of the call.
pub(crate) fn enter_namespaces(plan: &SandboxPlan, fds: &ChildFds) -> Result<(), SetupError> {
    await_go_signal(fds)?;

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

/// Arm `PR_SET_PDEATHSIG` so this process is `SIGKILL`ed when its
/// parent dies. `phase` attributes a failure to the early arm or the
/// post-credential-drop re-arm.
///
/// Async-signal-safe: a single `prctl(2)`.
// r[impl builder.exec.pdeathsig-first]
fn arm_pdeathsig(phase: SetupPhase) -> Result<(), SetupError> {
    // SAFETY: prctl with integer arguments has no memory preconditions.
    if unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) } != 0 {
        return Err(SetupError::new(phase, Errno::last()));
    }
    Ok(())
}

/// The fallible body of [`setup_and_exec`], split out so `?` can be
/// used over the per-step helpers.
fn setup(plan: &SandboxPlan, fds: &ChildFds) -> Result<(), SetupError> {
    let c = &plan.child;

    // --- Parent-death cascade, armed FIRST ---------------------------------
    // If the intermediate dies at any point during the long setup
    // sequence below — mounts, pivot_root, hardening, the privilege
    // drop — this process must die with it instead of continuing to
    // build a sandbox nobody supervises. CppNix arms the death signal
    // as its child's first statement too (processes.cc, dieWithParent).
    // The arm is repeated after setuid because the kernel clears
    // pdeath_signal on credential changes.
    // r[impl builder.exec.pdeathsig-first]
    arm_pdeathsig(SetupPhase::PdeathsigEarly)?;

    // --- Session and stdio -------------------------------------------------
    // A fresh session so the build has NO controlling terminal — the
    // oracle's stated intent (CppNix child.cc:17-22: "Put the child in
    // a separate session (and thus a separate process group) so that
    // it has no controlling terminal (meaning that e.g. ssh cannot
    // open /dev/tty) and it doesn't receive terminal signals"). The
    // pty fds the parent allocated arrive only via the dup2 below,
    // which never acquires a ctty (no open(2) of the slave, no
    // TIOCSCTTY) — so /dev/tty inside the sandbox is unopenable
    // (ENXIO; pinned against the oracle by the sandbox-identity
    // differential entry), and a later kill of the build's process
    // group does not reach the executor.
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
    // File-descriptor limit: pin RLIMIT_NOFILE so the build-visible
    // value does not depend on how the executor itself was launched
    // (pod OCI spec, systemd unit, test harness). CppNix never touches
    // this limit, so daemon-era builders simply inherited
    // nix-daemon.service's LimitNOFILE — 1048576 on NixOS — and that
    // inherited value is the de-facto sandbox ABI existing packages
    // were built under. Pin exactly that. We are still root here (the
    // uid/gid drop happens below), so raising the hard limit succeeds
    // wherever the executor has CAP_SYS_RESOURCE; if it does not and
    // the inherited hard limit is lower, clamp to the inherited hard
    // limit instead of failing the build.
    let nofile = libc::rlimit {
        rlim_cur: SANDBOX_NOFILE_LIMIT,
        rlim_max: SANDBOX_NOFILE_LIMIT,
    };
    // SAFETY: setrlimit/getrlimit read and write plain structs.
    if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &nofile) } != 0 {
        let mut inherited = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut inherited) } != 0 {
            return Err(SetupError::new(SetupPhase::Rlimit, Errno::last()));
        }
        let clamped = libc::rlimit {
            rlim_cur: SANDBOX_NOFILE_LIMIT.min(inherited.rlim_max),
            rlim_max: inherited.rlim_max,
        };
        if unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &clamped) } != 0 {
            return Err(SetupError::new(SetupPhase::Rlimit, Errno::last()));
        }
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
    }

    // setuid cleared the parent-death signal (the kernel does this on
    // any credential change); re-arm it so intermediate death keeps
    // cascading here. The traditional getppid()-changed race check is
    // meaningless in this topology: this process is pid 1 of its PID
    // namespace, so getppid() always returns 0 whether or not the real
    // parent is alive. Without a cgroup, two residual windows remain
    // where intermediate death does NOT cascade — the handful of
    // instructions in [fork, the early arm] and in [setuid, this
    // re-arm]; with a per-build cgroup the executor's cgroup kill
    // covers both. Documented at the executor's kill guard.
    // r[impl builder.exec.pdeathsig-first]
    arm_pdeathsig(SetupPhase::Pdeathsig)?;

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
            // r[impl builder.platform.i686+2]
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
    use std::os::fd::AsRawFd as _;
    use std::time::{Duration, Instant};

    use super::*;

    // -- fd shedding (fork-based, unprivileged) ------------------------------

    /// Wait for `pid` with a deadline, SIGKILLing it on timeout. Returns
    /// the exit code. The deadline is generous (these children do a few
    /// syscalls and exit); hitting it means the child hung, which is
    /// itself the regression the deadline exists to catch.
    fn wait_with_deadline(pid: libc::pid_t, deadline: Duration) -> i32 {
        let start = Instant::now();
        loop {
            let mut status: libc::c_int = 0;
            // SAFETY: waitpid into a stack buffer for a direct child.
            let rc = unsafe { libc::waitpid(pid, &raw mut status, libc::WNOHANG) };
            match rc {
                0 => {
                    if start.elapsed() > deadline {
                        // SAFETY: SIGKILL to a child this test forked and
                        // has not reaped.
                        unsafe {
                            libc::kill(pid, libc::SIGKILL);
                            libc::waitpid(pid, &raw mut status, 0);
                        }
                        panic!("forked test child hung past {deadline:?} (the regression)");
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                rc if rc == pid => {
                    assert!(
                        libc::WIFEXITED(status),
                        "child terminated abnormally: raw status {status}"
                    );
                    return libc::WEXITSTATUS(status);
                }
                _ => panic!("waitpid failed: {}", std::io::Error::last_os_error()),
            }
        }
    }

    /// Is `fd` open in this process? (`fcntl(F_GETFD)` succeeds.)
    fn fd_is_open(fd: RawFd) -> bool {
        // SAFETY: fcntl F_GETFD has no memory preconditions.
        unsafe { libc::fcntl(fd, libc::F_GETFD) != -1 }
    }

    /// A non-CLOEXEC pipe pair for fd-inheritance tests.
    fn plain_pipe() -> (std::os::fd::OwnedFd, std::os::fd::OwnedFd) {
        nix::unistd::pipe().expect("pipe")
    }

    /// The keep-set sweep closes decoys and parent-side ends but leaves
    /// the keep set and stdio open.
    // r[verify builder.exec.fd-keep-set]
    #[test]
    fn shed_inherited_fds_closes_everything_outside_the_keep_set() {
        let (decoy_r, decoy_w) = plain_pipe();
        let (status_r, status_w) = plain_pipe();
        let (go_r, go_w) = plain_pipe();
        let (out_r, out_w) = plain_pipe();
        let (err_r, err_w) = plain_pipe();
        let fds = ChildFds {
            status_pipe_w: status_w.as_raw_fd(),
            go_pipe_r: go_r.as_raw_fd(),
            stdout_fd: out_w.as_raw_fd(),
            stderr_fd: err_w.as_raw_fd(),
        };
        let decoys = [decoy_r.as_raw_fd(), decoy_w.as_raw_fd()];
        let parent_side = [
            status_r.as_raw_fd(),
            go_w.as_raw_fd(),
            out_r.as_raw_fd(),
            err_r.as_raw_fd(),
        ];

        // SAFETY: the child branch only calls async-signal-safe code
        // (the function under test, fcntl, _exit).
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                // Child: sweep, then verify with exit codes (no panics —
                // this is a forked child of a possibly multi-threaded
                // test process).
                if shed_inherited_fds(&fds).is_err() {
                    // SAFETY: _exit is always safe.
                    unsafe { libc::_exit(10) };
                }
                for fd in decoys {
                    if fd_is_open(fd) {
                        unsafe { libc::_exit(11) };
                    }
                }
                for fd in parent_side {
                    if fd_is_open(fd) {
                        unsafe { libc::_exit(12) };
                    }
                }
                if !fd_is_open(fds.status_pipe_w)
                    || !fd_is_open(fds.go_pipe_r)
                    || !fd_is_open(fds.stdout_fd)
                    || !fd_is_open(fds.stderr_fd)
                {
                    unsafe { libc::_exit(13) };
                }
                if !fd_is_open(0) || !fd_is_open(1) || !fd_is_open(2) {
                    unsafe { libc::_exit(14) };
                }
                unsafe { libc::_exit(0) };
            }
            pid => {
                let code = wait_with_deadline(pid, Duration::from_secs(10));
                assert_eq!(
                    code, 0,
                    "child fd checks failed (10=sweep error, 11=decoy open, \
                     12=parent-side end open, 13=keep fd closed, 14=stdio touched)"
                );
            }
        }
    }

    /// THE merged_bug_005 regression: the parent closing the go pipe's
    /// write end without writing must unblock the intermediate with a
    /// "parent died" error. Before the sweep the intermediate inherited
    /// its own copy of the write end, so this EOF could never arrive
    /// and the child hung forever (which the deadline converts into a
    /// test failure).
    // r[verify builder.exec.fd-keep-set]
    #[test]
    fn parent_go_pipe_close_unblocks_intermediate_after_sweep() {
        let (status_r, status_w) = plain_pipe();
        let (go_r, go_w) = plain_pipe();
        let (out_r, out_w) = plain_pipe();
        let fds = ChildFds {
            status_pipe_w: status_w.as_raw_fd(),
            go_pipe_r: go_r.as_raw_fd(),
            stdout_fd: out_w.as_raw_fd(),
            stderr_fd: out_w.as_raw_fd(),
        };

        // SAFETY: the child branch only calls async-signal-safe code.
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                // Child: the intermediate's first two steps.
                if shed_inherited_fds(&fds).is_err() {
                    unsafe { libc::_exit(10) };
                }
                match await_go_signal(&fds) {
                    Err(e) if e.phase == SetupPhase::GoPipe && e.errno == Errno::EPIPE as i32 => {
                        // Parent death detected — the property under test.
                        unsafe { libc::_exit(42) };
                    }
                    Err(_) => unsafe { libc::_exit(11) },
                    Ok(()) => unsafe { libc::_exit(12) },
                }
            }
            pid => {
                // Parent: close the write end WITHOUT writing the go
                // byte (what a crashed/cancelled parent looks like),
                // and drop every other parent-side copy so the child's
                // own (swept) table is the only thing that matters.
                drop(go_w);
                drop(status_r);
                drop(status_w);
                drop(out_r);
                drop(out_w);
                drop(go_r);
                let code = wait_with_deadline(pid, Duration::from_secs(10));
                assert_eq!(
                    code, 42,
                    "child must observe go-pipe EOF as parent death \
                     (10=sweep error, 11=wrong error, 12=spurious go byte)"
                );
            }
        }
    }

    /// The sweep must not break the normal handshake: a go byte written
    /// by the parent still arrives.
    // r[verify builder.exec.fd-keep-set]
    #[test]
    fn go_byte_still_arrives_after_sweep() {
        // The parent keeps every end open here (underscore bindings
        // hold them alive): only the write of the go byte matters.
        let (_status_r, status_w) = plain_pipe();
        let (go_r, go_w) = plain_pipe();
        let (_out_r, out_w) = plain_pipe();
        let fds = ChildFds {
            status_pipe_w: status_w.as_raw_fd(),
            go_pipe_r: go_r.as_raw_fd(),
            stdout_fd: out_w.as_raw_fd(),
            stderr_fd: out_w.as_raw_fd(),
        };

        // SAFETY: the child branch only calls async-signal-safe code.
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                if shed_inherited_fds(&fds).is_err() {
                    unsafe { libc::_exit(10) };
                }
                match await_go_signal(&fds) {
                    Ok(()) => unsafe { libc::_exit(0) },
                    Err(_) => unsafe { libc::_exit(11) },
                }
            }
            pid => {
                nix::unistd::write(&go_w, &[1u8]).expect("write the go byte");
                let code = wait_with_deadline(pid, Duration::from_secs(10));
                assert_eq!(code, 0, "the go byte must still arrive after the sweep");
            }
        }
    }

    // -- parent-death signal (fork-based, unprivileged) ----------------------

    /// The early arm: a sandbox child whose intermediate dies during
    /// setup must die with it. Exact production topology — the test
    /// plays the executor and forks A (the intermediate), which forks B
    /// (the sandbox child). B arms the death signal as its FIRST
    /// statement (the property under test), reports readiness, and
    /// parks; the test then lets A exit and asserts B is SIGKILLed by
    /// the kernel (its liveness-pipe write end closes). No wall-clock
    /// race assertions — the deadline only bounds the failure case.
    // r[verify builder.exec.pdeathsig-first]
    #[test]
    fn pdeathsig_armed_child_dies_with_its_parent() {
        let (ready_r, ready_w) = plain_pipe(); // B → test: "armed and parked"
        let (exit_r, exit_w) = plain_pipe(); // test → A: "you may exit"
        let (live_r, live_w) = plain_pipe(); // B holds live_w; EOF = B died

        // SAFETY: child branches only call async-signal-safe code.
        match unsafe { libc::fork() } {
            -1 => panic!("fork failed: {}", std::io::Error::last_os_error()),
            0 => {
                // A: the intermediate.
                match unsafe { libc::fork() } {
                    -1 => unsafe { libc::_exit(20) },
                    0 => {
                        // B: the sandbox child. Arm FIRST, then report,
                        // then park until the parent-death SIGKILL.
                        if arm_pdeathsig(SetupPhase::PdeathsigEarly).is_err() {
                            unsafe { libc::_exit(21) };
                        }
                        // SAFETY: write/pause are async-signal-safe;
                        // live_w stays open in B for the test to observe.
                        unsafe {
                            let byte = [1u8];
                            libc::write(ready_w.as_raw_fd(), byte.as_ptr().cast(), 1);
                            loop {
                                libc::pause();
                            }
                        }
                    }
                    _b_pid => {
                        // A: hold until the test says B's arm was
                        // observed, then exit — which must SIGKILL B.
                        let mut buf = [0u8; 1];
                        // SAFETY: blocking read into a stack buffer,
                        // then _exit.
                        unsafe {
                            libc::read(exit_r.as_raw_fd(), buf.as_mut_ptr().cast(), 1);
                            libc::_exit(0);
                        }
                    }
                }
            }
            a_pid => {
                // The test (executor side): drop our copies of the ends
                // whose EOF we must observe.
                drop(live_w);
                drop(ready_w);
                // B reports it has armed.
                let mut buf = [0u8; 1];
                nix::unistd::read(&ready_r, &mut buf).expect("readiness byte from B");
                // Let A exit; it must exit cleanly.
                nix::unistd::write(&exit_w, &[1u8]).expect("exit permission to A");
                let code = wait_with_deadline(a_pid, Duration::from_secs(10));
                assert_eq!(code, 0, "the intermediate must exit when told");
                // B must now die from the parent-death signal: its
                // liveness pipe EOFs. Poll nonblocking with a deadline
                // (the deadline only bounds the failure case).
                // SAFETY: fcntl F_SETFL on an fd this test owns.
                unsafe {
                    libc::fcntl(live_r.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK);
                }
                let start = Instant::now();
                loop {
                    let mut b = [0u8; 1];
                    match nix::unistd::read(&live_r, &mut b) {
                        Ok(0) => break, // EOF: B is gone.
                        Ok(_) => panic!("unexpected bytes on the liveness pipe"),
                        Err(Errno::EAGAIN) => {
                            assert!(
                                start.elapsed() < Duration::from_secs(10),
                                "sandbox child survived its parent's death — \
                                 the early pdeathsig arm did not take effect"
                            );
                            std::thread::sleep(Duration::from_millis(20));
                        }
                        Err(e) => panic!("liveness-pipe read failed: {e}"),
                    }
                }
            }
        }
    }

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
