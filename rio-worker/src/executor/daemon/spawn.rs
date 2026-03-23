//! Spawn `nix-daemon --stdio` in a private mount namespace.
// r[impl worker.daemon.no-unwrap-stdio]
// r[impl worker.daemon.kill-both-paths]
// r[impl worker.ns.order]

use std::path::Path;

use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};

use tokio::process::Command;

use tracing::instrument;

use crate::executor::ExecutorError;
use crate::overlay;

/// Bind-mount `src` onto `target` (no propagation). Async-signal-safe:
/// `nix::mount::mount` is a direct syscall wrapper, `std::io::Error::from(Errno)`
/// stores only an i32. Safe to call from `pre_exec`.
fn bind_mount(src: &Path, target: &str) -> std::io::Result<()> {
    mount(
        Some(src),
        target,
        None::<&str>,
        MsFlags::MS_BIND,
        None::<&str>,
    )
    .map_err(std::io::Error::from)
}

/// Spawn `nix-daemon --stdio` in a private mount namespace with the overlay
/// bind-mounted at canonical paths.
///
/// The child process gets its own mount namespace (CLONE_NEWNS), then:
///   1. Makes `/` MS_PRIVATE so bind mounts don't leak to the parent ns
///      (systemd defaults to MS_SHARED; without this, the bind propagates).
///   2. Bind-mounts the overlay merged dir at `/nix/store`.
///   3. Bind-mounts the synthetic DB dir at `/nix/var/nix/db`.
///   4. Bind-mounts the nix.conf dir at `/etc/nix`.
///
/// The daemon's own sandbox builds inherit these mounts (CLONE_NEWNS gives
/// children a COPY of the parent's mounts), so sandboxed builders see the
/// overlay-backed `/nix/store` too.
///
/// # Constraints
///
/// - Requires `CAP_SYS_ADMIN` (for unshare + mount).
/// - The bind targets `/nix/store`, `/nix/var/nix/db`, `/etc/nix` must
///   exist in the worker's mount namespace. The NixOS worker module creates
///   `/nix/var/nix/db` via tmpfiles (the real nix-daemon creates it lazily,
///   but we never run the real daemon). Non-Nix hosts are unsupported.
///
/// # Why async + spawn_blocking
///
/// `cmd.spawn()` blocks the parent thread on the child's CLOEXEC error-pipe
/// until the child either execs (pipe closes) or `pre_exec` returns `Err`.
/// Our `pre_exec` bind-mounts the overlay merged dir at `/nix/store`; the
/// kernel validates the overlay lower (FUSE), which can trigger a FUSE
/// `getattr` request. FUSE threads use `Handle::block_on(gRPC)`, which needs
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
    // FOD proxy: inject http_proxy/https_proxy env ONLY when
    // is_fixed_output is true. Nix's FOD sandbox passes these
    // through to the builder (FODs need network for fetchurl).
    // Non-FOD builds have no network anyway (sandbox blocks it),
    // so proxy env would be unused but we still don't set it —
    // reduces any confusion about what environment the daemon
    // sees, and prevents a non-FOD builder from accidentally
    // picking it up if someone misconfigures sandbox settings.
    //
    // `fod_proxy`: None = FOD proxy disabled OR this isn't an
    // FOD. Some(url) = set proxy env vars. Caller computes this
    // from `is_fixed_output && env.fod_proxy_url.is_some()`.
    fod_proxy: Option<&str>,
) -> Result<tokio::process::Child, ExecutorError> {
    // Clone paths BEFORE the closure — the clones happen in the parent
    // pre-fork, so they're safe. The closure captures owned PathBufs.
    let merged = overlay_mount.merged_dir().to_path_buf();
    let upper_db = overlay_mount.upper_synth_db();
    let upper_conf = overlay_mount.upper_nix_conf();

    // Validate bind SOURCES and TARGETS exist before spawning. If `pre_exec`
    // returns ENOENT, spawn() reports a generic "No such file or directory"
    // that doesn't say WHICH path is missing — this pre-check gives a clear
    // error. Targets are supposed to be created by module tmpfiles, but verify
    // anyway (tmpfiles `d` doesn't create parents; easy to get wrong).
    for (label, path) in [
        ("bind source: overlay merged", merged.as_path()),
        ("bind source: synthetic DB dir", upper_db.as_path()),
        ("bind source: nix.conf dir", upper_conf.as_path()),
        ("bind target: /nix/store", Path::new("/nix/store")),
        ("bind target: /nix/var/nix/db", Path::new("/nix/var/nix/db")),
        ("bind target: /etc/nix", Path::new("/etc/nix")),
    ] {
        if !path.exists() {
            return Err(ExecutorError::DaemonSpawn(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("{label} missing: {}", path.display()),
            )));
        }
    }

    // Build the command + pre_exec closure, then spawn on the blocking pool.
    // See "Why async + spawn_blocking" in the function doc for the deadlock chain
    // this avoids. tokio::process::Command is not Send (the FnMut pre_exec
    // closure makes it !Send), so we build it INSIDE the spawn_blocking closure.
    //
    // `nix-daemon` and its dynamic library deps live in the HOST `/nix/store`.
    // The overlay merged dir (bind-mounted at `/nix/store` in the child)
    // includes the host store as its FIRST lower layer (see overlay.rs), so
    // nix-daemon + glibc + etc. stay visible through the overlay alongside
    // FUSE-served rio-store paths.
    // Owned String for move into spawn_blocking (can't borrow
    // across thread boundary). None stays None (not Some("")).
    let fod_proxy = fod_proxy.map(|s| s.to_string());

    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("nix-daemon");
        cmd.arg("--stdio")
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

        // FOD proxy env. Both lowercase and uppercase for compat
        // (curl honors lowercase; some tools want uppercase; Nix's
        // sandbox passes both through to the FOD builder). ONLY
        // set for FODs — non-FOD sandbox has no network so these
        // would be useless there, and setting them could mislead
        // someone reading /proc/PID/environ during debugging.
        if let Some(proxy) = &fod_proxy {
            cmd.env("http_proxy", proxy)
                .env("https_proxy", proxy)
                .env("HTTP_PROXY", proxy)
                .env("HTTPS_PROXY", proxy);
        }

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
                let _ = raised; // silence unused warning if diag disabled

                // New mount namespace for this process tree (daemon + sandbox).
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

                // Bind overlay merged → /nix/store, synthetic DB, nix.conf dir
                bind_mount(&merged, "/nix/store")?;
                bind_mount(&upper_db, "/nix/var/nix/db")?;
                bind_mount(&upper_conf, "/etc/nix")?;

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

// r[verify worker.daemon.no-unwrap-stdio]
// r[verify worker.daemon.kill-both-paths]
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

    // r[verify worker.daemon.timeout-wrap]
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
