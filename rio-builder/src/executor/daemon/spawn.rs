//! Spawn `nix-daemon --stdio` for a chroot store at the per-build dir.
// r[impl builder.daemon.no-unwrap-stdio]
// r[impl builder.daemon.kill-both-paths]
// r[impl builder.ns.order+2]

use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};

use tokio::process::Command;

use tracing::instrument;

use crate::executor::ExecutorError;
use crate::overlay;

/// Spawn `nix-daemon --stdio --store 'local?root={build_dir}'`.
///
/// The daemon's binary + dynamic-library deps come from the host
/// `/nix/store` (the builder pod's namespace, untouched). Its store
/// OPERATIONS go through the chroot store at `{build_dir}`: it reads
/// `.drv` files from `{build_dir}/nix/store` (the FUSE-backed overlay),
/// uses `{build_dir}/nix/var/nix/db` (the synth-db) as state, and its
/// nested sandbox bind-mounts inputs from `{build_dir}/nix/store/{hash}`
/// (`realStoreDir`) into the build's `/nix/store/{hash}` (`storeDir`).
///
/// I-060: previously the overlay was bind-mounted at `/nix/store` in a
/// per-build namespace, with the host store as a lower layer so the
/// daemon could find its libs. That let a build whose output hash
/// matched a daemon-runtime path (same nixpkgs → same `libunistring`
/// hash) shadow the daemon's libs via overlay copy-up. Chroot-store
/// makes the per-build store and the daemon's runtime structurally
/// disjoint paths.
///
/// # Thin namespace
///
/// The child still does `unshare(CLONE_NEWNS)` + `MS_PRIVATE` + proc
/// remount — but ONLY to give the daemon an unmasked `/proc` (see the
/// proc-remount comment in the body). The namespace does NOT bind
/// `/nix/store`; the daemon's `/nix/store` is the host's.
///
/// # Constraints
///
/// - Requires `CAP_SYS_ADMIN` (for unshare + proc remount; also raised
///   to ambient for the daemon's own nested-sandbox `pivot_root`).
///
/// # Why async + spawn_blocking
///
/// `cmd.spawn()` blocks the parent thread on the child's CLOEXEC error-pipe
/// until the child either execs (pipe closes) or `pre_exec` returns `Err`.
/// `pre_exec` here only does unshare + proc remount (no FUSE traversal),
/// but `spawn_blocking` is kept: the parent block is short but real, and
/// the pre_exec `!Send` constraint means the Command must be built inside
/// the blocking closure regardless. FUSE threads use `Handle::block_on(gRPC)`, which needs
/// the tokio reactor. If BOTH tokio worker threads are blocked in `spawn()`
/// (2 concurrent builds × 1-2 cores), the reactor isn't driven → FUSE hangs
/// → child's `mount()` never returns → parent's `spawn()` never returns.
/// Running `spawn()` on the blocking pool breaks this cycle: tokio worker
/// threads stay free to drive the reactor while `spawn()` waits.
///
/// # Safety
///
/// The `pre_exec` closure runs post-fork, pre-exec, and must be async-signal-
/// safe: no allocation, no locks. `nix::sched::unshare` and `nix::mount::mount`
/// are direct syscall wrappers (no allocation). `std::io::Error::from(Errno)`
/// stores only an i32 (no allocation). PathBufs are cloned OUTSIDE the closure
/// and captured by move (the clone happens in the parent, pre-fork).
#[instrument(skip_all)]
pub(in crate::executor) async fn spawn_daemon_in_namespace(
    overlay_mount: &overlay::OverlayMount,
) -> Result<tokio::process::Child, ExecutorError> {
    // fod_proxy param removed per ADR-019: builders are airgapped
    // (no proxy needed — no internet); fetchers have direct egress
    // (no proxy needed — hash check is the integrity boundary).
    let store_arg = format!("local?root={}", overlay_mount.root_dir().display());
    let conf_dir = overlay_mount.upper_nix_conf();

    // Build the command + pre_exec closure, then spawn on the blocking pool.
    // See "Why async + spawn_blocking" in the function doc for the deadlock chain
    // this avoids. tokio::process::Command is not Send (the FnMut pre_exec
    // closure makes it !Send), so we build it INSIDE the spawn_blocking closure.
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("nix-daemon");
        cmd.arg("--stdio")
            .arg("--store")
            .arg(&store_arg)
            .env("NIX_CONF_DIR", &conf_dir)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Inherit stderr: daemon diagnostics go to worker's stderr (visible
            // in container logs). Piping without reading would deadlock if
            // nix-daemon writes >64KB to stderr (pipe buffer full, blocks on write).
            .stderr(std::process::Stdio::inherit())
            // Safety net: the caller's explicit daemon.kill() covers the
            // normal path, but any `?` between spawn and that kill (e.g.,
            // the cgroup create/add_process in executor/mod.rs) would drop
            // the Child without killing — leaking a daemon process that
            // keeps the overlay mount busy. kill_on_drop makes the drop
            // path safe. The explicit kill + wait is still the primary
            // mechanism (graceful, bounded wait for reap); this is a
            // seatbelt for early-return paths.
            .kill_on_drop(true);

        // fod_proxy env removed per ADR-019: builders never see
        // FODs (scheduler routes to fetchers); fetchers have
        // direct egress. No http_proxy/https_proxy injection.

        // SAFETY: see function doc. Closure body is async-signal-safe.
        //
        // COVERAGE: the closure body runs post-fork pre-exec in the
        // CHILD. The child never returns to main() — it exec()s
        // nix-daemon or dies — so atexit never fires and LLVM's
        // profraw never flushes. The parent's fork-point counter
        // shows the closure was entered (cmd.pre_exec line itself
        // is covered), but the body instructions belong to a PID
        // that writes no profraw. Not a test gap; a fundamental
        // coverage-model limitation for pre_exec hooks.
        unsafe {
            cmd.pre_exec(move || {
                // Raise ambient caps so they survive exec. k8s
                // `capabilities.add` sets bounding/permitted/
                // effective/inheritable but NOT ambient; without
                // ambient, exec'd nix-daemon has UID 0 but empty
                // effective caps (nix store binaries have no file
                // caps). nix-daemon's sandbox pivot_root then fails
                // EPERM. Under privileged:true, runc sets ambient
                // for ALL caps → no issue. Under !privileged, we
                // raise the needed caps ourselves. Must be in both
                // permitted + inheritable to raise to ambient
                // (kernel check in prctl PR_CAP_AMBIENT_RAISE) —
                // k8s sets inheritable for capabilities.add, so
                // this works.
                //
                // containerd (post CVE-2022-24769) does NOT set
                // inheritable caps from k8s capabilities.add — only
                // bounding/permitted/effective. PR_CAP_AMBIENT_RAISE
                // requires the cap in BOTH permitted AND inheritable.
                // So: (1) capset to add inheritable (we're root with
                // effective caps, can do this), (2) raise ambient.
                //
                // Constants from linux/capability.h. All fit in
                // cap[0] (bits 0-31) since highest is 21.
                const CAPS: &[u32] = &[
                    21, // CAP_SYS_ADMIN
                    18, // CAP_SYS_CHROOT
                    7,  // CAP_SETUID
                    6,  // CAP_SETGID
                    12, // CAP_NET_ADMIN
                    0,  // CAP_CHOWN
                    1,  // CAP_DAC_OVERRIDE
                    5,  // CAP_KILL
                    3,  // CAP_FOWNER
                    8,  // CAP_SETPCAP (for capset inheritable below)
                ];
                let mask: u32 = CAPS.iter().fold(0, |m, &c| m | (1 << c));
                // capget/capset use linux/capability.h structs. v3
                // header (_LINUX_CAPABILITY_VERSION_3 = 0x20080522)
                // with two u32[3] data arrays (low 32 caps + high 32).
                #[repr(C)]
                struct CapHeader {
                    version: u32,
                    pid: i32,
                }
                #[repr(C)]
                #[derive(Default, Clone, Copy)]
                struct CapData {
                    effective: u32,
                    permitted: u32,
                    inheritable: u32,
                }
                let mut hdr = CapHeader {
                    version: 0x2008_0522, // _LINUX_CAPABILITY_VERSION_3
                    pid: 0,               // self
                };
                let mut data = [CapData::default(); 2];
                // SAFETY: syscall with stack-allocated repr(C)
                // structs, correct v3 layout (2×12 bytes). Kernel
                // reads header, writes data. Async-signal-safe.
                // Track success — if capset or ambient-raise fail,
                // nix-daemon's pivot_root will EPERM later with no
                // useful context. Write a marker to stderr (inherited
                // by nix-daemon, visible in worker logs) so the root
                // cause is visible. write() is async-signal-safe.
                let mut raised = 0u32;
                if nix::libc::syscall(nix::libc::SYS_capget, &mut hdr as *mut _, data.as_mut_ptr())
                    == 0
                {
                    // Add our caps to inheritable (keep existing
                    // permitted/effective — we're only ADDING).
                    data[0].inheritable |= mask;
                    // SAFETY: same layout, kernel reads both.
                    if nix::libc::syscall(nix::libc::SYS_capset, &mut hdr as *mut _, data.as_ptr())
                        == 0
                    {
                        // Now raise ambient (permitted + inheritable → ok).
                        for &cap in CAPS {
                            // SAFETY: prctl with integer args only.
                            if nix::libc::prctl(
                                nix::libc::PR_CAP_AMBIENT,
                                nix::libc::PR_CAP_AMBIENT_RAISE as nix::libc::c_ulong,
                                cap as nix::libc::c_ulong,
                                0,
                                0,
                            ) == 0
                            {
                                raised |= 1 << cap;
                            }
                        }
                    }
                }
                // Sanity: if ambient-raise didn't fully succeed
                // (raised != mask), nix-daemon will later fail
                // cryptically at pivot_root EPERM. Flag it here so
                // the root cause is visible. Under privileged:true
                // or systemd-unit root, raised==mask trivially
                // (already ambient OR mask==0 effective). Only the
                // nonpriv k8s path can hit a gap.
                //
                // pre_exec context: no allocation, no tokio, no
                // format!. libc::write(2, ...) with a static byte
                // slice is async-signal-safe. Ignoring the write()
                // return is fine — if stderr is closed, the
                // diagnostic is lost but the build proceeds (and
                // fails at pivot_root with the usual opacity).
                if raised != mask {
                    const MSG: &[u8] = b"rio-builder: pre_exec: ambient capability raise \
                        incomplete (raised != mask) -- expect pivot_root EPERM. \
                        Check pod securityContext capabilities.\n";
                    // SAFETY: write(2) is async-signal-safe; MSG is a
                    // static slice with valid pointer+len.
                    nix::libc::write(2, MSG.as_ptr() as *const _, MSG.len());
                }

                // Thin mount namespace — exists ONLY so the proc
                // remount below stays private to this daemon. The
                // child's `/nix/store` is the host's; the per-build
                // store is reached via `--store local?root=...`.
                unshare(CloneFlags::CLONE_NEWNS).map_err(std::io::Error::from)?;

                // Make `/` private so bind mounts below don't propagate to the
                // parent namespace. MS_REC applies recursively (submounts too).
                mount(
                    None::<&str>,
                    "/",
                    None::<&str>,
                    MsFlags::MS_REC | MsFlags::MS_PRIVATE,
                    None::<&str>,
                )
                .map_err(std::io::Error::from)?;

                // Remount /proc fresh. Container runtimes mask /proc
                // paths for non-privileged pods (containerd over-
                // mounts /proc/{kcore,acpi,keys,sysrq-trigger,
                // timer_list,scsi,...} with /dev/null or empty
                // tmpfs). nix-daemon's mountAndPidNamespacesSupported()
                // check tries mount("none","/proc","proc",0,0) inside
                // a nested CLONE_NEWNS|CLONE_NEWPID|CLONE_NEWUSER;
                // the kernel refuses when /proc has over-mounts
                // (would reveal masked paths — fs/proc/root.c
                // proc_mount checks pid_ns_prepare_proc). Under
                // privileged:true, containerd doesn't mask → check
                // passes. procMount:Unmasked on the pod spec would
                // fix this, but k8s PSA rejects it when hostUsers:
                // true (KEP-4265). We're in the init userns with
                // CAP_SYS_ADMIN here (post-CLONE_NEWNS, pre-exec,
                // NOT CLONE_NEWUSER), so we can mount fresh proc
                // ourselves — the new mount has no masks, nix-daemon
                // inherits it, its nested-ns check passes.
                //
                // Must come AFTER MS_PRIVATE (otherwise propagates to
                // the container's /proc, unmasking for everyone) and
                // the masks are sub-mounts of /proc, so the fresh
                // mount-OVER shadows them (they stay mounted
                // underneath, invisible — standard stack semantics).
                mount(
                    Some("proc"),
                    "/proc",
                    Some("proc"),
                    MsFlags::empty(),
                    None::<&str>,
                )
                .map_err(std::io::Error::from)?;

                Ok(())
            });
        }

        cmd.spawn().map_err(ExecutorError::DaemonSpawn)
    })
    .await
    .map_err(|e| {
        ExecutorError::DaemonSpawn(std::io::Error::other(format!(
            "spawn_blocking task panicked: {e}"
        )))
    })?
}

// r[verify builder.daemon.no-unwrap-stdio]
// r[verify builder.daemon.kill-both-paths]
#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    use tokio::sync::mpsc;

    use crate::executor::daemon::{DAEMON_SETUP_TIMEOUT, run_daemon_build};
    use crate::log_stream::LogBatcher;

    // -----------------------------------------------------------------------
    // Daemon lifecycle
    // -----------------------------------------------------------------------

    /// Verify that run_daemon_build fails when the process doesn't speak the
    /// Nix protocol (handshake failure), and that the caller's always-kill
    /// pattern leaves no leaked process.
    #[tokio::test]
    async fn test_daemon_killed_on_handshake_failure() -> anyhow::Result<()> {
        // Spawn a process that closes stdout immediately (causing handshake
        // read to get EOF fast) but keeps running. `sh -c 'exec >&-; sleep 1000'`
        // closes stdout (FD 1) then sleeps.
        let mut fake_daemon = Command::new("sh")
            .arg("-c")
            .arg("exec >&-; sleep 1000")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("spawn sh");
        let pid = fake_daemon.id().expect("pid");

        // Minimal basic derivation for the test
        let output =
            rio_nix::derivation::DerivationOutput::new("out", "/nix/store/test-out", "", "")?;
        let basic_drv = rio_nix::derivation::BasicDerivation::new(
            vec![output],
            Default::default(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            Default::default(),
        )?;
        let batcher = LogBatcher::new(
            "test.drv".into(),
            "test-worker".into(),
            crate::log_stream::LogLimits::UNLIMITED,
        );
        let (log_tx, _log_rx) = mpsc::channel(4);

        // run_daemon_build should fail quickly (handshake reads EOF from closed stdout).
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_daemon_build(
                &mut fake_daemon,
                "/nix/store/test.drv",
                &basic_drv,
                Duration::from_secs(5),
                0, // max_silent_time: unbounded (handshake fails before it matters)
                0, // build_cores: all
                batcher,
                &log_tx,
            ),
        )
        .await
        .expect("handshake should fail fast, not hang");
        assert!(
            result.is_err(),
            "handshake against closed stdout should fail"
        );

        // Caller must kill (as execute_build does)
        let _ = fake_daemon.kill().await;
        let _ = tokio::time::timeout(Duration::from_secs(2), fake_daemon.wait()).await;

        // Verify the process is actually dead. On Linux, /proc/<pid> goes away
        // once the process is reaped. We reaped it above via wait(), so check.
        tokio::time::sleep(Duration::from_millis(100)).await;
        let proc_path = format!("/proc/{pid}");
        let alive = std::path::Path::new(&proc_path).exists();
        assert!(!alive, "daemon process should be dead after kill + wait");
        Ok(())
    }

    // r[verify builder.daemon.timeout-wrap]
    /// Verify that DAEMON_SETUP_TIMEOUT is shorter than the default daemon
    /// build timeout. If setup timeout were longer, it would be pointless.
    #[test]
    fn test_timeout_ordering() {
        assert!(
            DAEMON_SETUP_TIMEOUT < crate::executor::DEFAULT_DAEMON_TIMEOUT,
            "setup timeout ({DAEMON_SETUP_TIMEOUT:?}) must be shorter than default daemon timeout ({:?})",
            crate::executor::DEFAULT_DAEMON_TIMEOUT
        );
        // Also: the gRPC stream timeout (300s, NAR transfers) is
        // shorter than the daemon build timeout. That ordering used
        // to be asserted in rio-common before daemon_timeout moved here.
        assert!(
            rio_common::grpc::GRPC_STREAM_TIMEOUT < crate::executor::DEFAULT_DAEMON_TIMEOUT,
            "NAR stream timeout must be shorter than daemon build timeout"
        );
    }
}
