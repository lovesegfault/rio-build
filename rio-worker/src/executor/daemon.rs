//! Daemon lifecycle: spawn `nix-daemon --stdio`, run build, stream logs.

use std::path::Path;
use std::time::Duration;

use nix::mount::{MsFlags, mount};
use nix::sched::{CloneFlags, unshare};

use tokio::process::Command;
use tokio::sync::mpsc;

use rio_nix::protocol::build::{BuildMode, BuildResult, BuildStatus};
use rio_nix::protocol::client::{
    StderrMessage, client_handshake, client_set_options, read_stderr_message,
};
use rio_nix::protocol::wire;
use rio_proto::types::{WorkerMessage, worker_message};

use crate::log_stream::LogBatcher;
use crate::overlay;

use super::ExecutorError;

/// Timeout for the daemon setup sequence (handshake + setOptions + send build).
/// This bounds the blast radius of a stuck daemon before the build timeout kicks in.
pub(super) const DAEMON_SETUP_TIMEOUT: Duration = Duration::from_secs(30);

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
pub(super) async fn spawn_daemon_in_namespace(
    overlay_mount: &overlay::OverlayMount,
) -> Result<tokio::process::Child, ExecutorError> {
    // Clone paths BEFORE the closure — the clones happen in the parent
    // pre-fork, so they're safe. The closure captures owned PathBufs.
    let merged = overlay_mount.merged_dir().to_path_buf();
    let upper_db = overlay_mount.upper_dir().join("nix/var/nix/db");
    let upper_conf = overlay_mount.upper_dir().join("etc/nix");

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
    tokio::task::spawn_blocking(move || {
        let mut cmd = Command::new("nix-daemon");
        cmd.arg("--stdio")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            // Inherit stderr: daemon diagnostics go to worker's stderr (visible
            // in container logs). Piping without reading would deadlock if
            // nix-daemon writes >64KB to stderr (pipe buffer full, blocks on write).
            .stderr(std::process::Stdio::inherit());

        // SAFETY: see function doc. Closure body is async-signal-safe.
        unsafe {
            cmd.pre_exec(move || {
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

/// All daemon I/O after spawn: handshake, setOptions, wopBuildDerivation, stderr loop.
///
/// Caller MUST kill the daemon after this returns (whether Ok or Err).
/// This is the only function that should touch daemon stdin/stdout —
/// keeping it isolated ensures the caller's always-kill path is reliable.
pub(super) async fn run_daemon_build(
    daemon: &mut tokio::process::Child,
    drv_path: &str,
    basic_drv: &rio_nix::derivation::BasicDerivation,
    build_timeout: Duration,
    batcher: &mut LogBatcher,
    log_tx: &mpsc::Sender<WorkerMessage>,
) -> Result<BuildResult, ExecutorError> {
    let mut stdin = daemon
        .stdin
        .take()
        .ok_or_else(|| ExecutorError::DaemonSetup("failed to get daemon stdin".into()))?;
    let mut stdout = daemon
        .stdout
        .take()
        .ok_or_else(|| ExecutorError::DaemonSetup("failed to get daemon stdout".into()))?;

    // Handshake + setOptions + send build — all bounded by DAEMON_SETUP_TIMEOUT.
    // Previously only the handshake was timed out; a stuck setOptions or
    // a stalled write would hang until build_timeout (potentially hours).
    tokio::time::timeout(DAEMON_SETUP_TIMEOUT, async {
        let handshake_result = client_handshake(&mut stdout, &mut stdin).await?;

        tracing::debug!(
            version = handshake_result.negotiated_version(),
            "daemon handshake complete"
        );

        client_set_options(&mut stdout, &mut stdin).await?;

        wire::write_u64(
            &mut stdin,
            rio_nix::protocol::opcodes::WorkerOp::BuildDerivation as u64,
        )
        .await?;
        wire::write_string(&mut stdin, drv_path).await?;
        rio_nix::protocol::build::write_basic_derivation(&mut stdin, basic_drv).await?;
        wire::write_u64(&mut stdin, BuildMode::Normal as u64).await?;
        tokio::io::AsyncWriteExt::flush(&mut stdin)
            .await
            .map_err(wire::WireError::from)?;

        Ok::<_, ExecutorError>(())
    })
    .await
    .map_err(|_| ExecutorError::DaemonSetup("daemon setup sequence timed out".into()))??;

    // Read STDERR loop with log streaming (build may run for a long time)
    let build_result = tokio::time::timeout(build_timeout, async {
        read_build_stderr_loop(&mut stdout, batcher, log_tx).await
    })
    .await
    .map_err(|_| ExecutorError::BuildFailed("build timed out".into()))??;

    Ok(build_result)
}

/// Read the STDERR loop from the daemon, streaming logs via the batcher.
///
/// If the log channel closes during the build, returns an InfrastructureFailure —
/// the scheduler stream is gone, so there's no way to report completion anyway.
async fn read_build_stderr_loop<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    batcher: &mut LogBatcher,
    log_tx: &mpsc::Sender<WorkerMessage>,
) -> Result<BuildResult, wire::WireError> {
    const MAX_BUILD_STDERR_MESSAGES: u64 = 10_000_000;
    let mut msg_count: u64 = 0;

    /// Helper: send a log batch. Returns false if the channel is closed.
    async fn send_batch(
        log_tx: &mpsc::Sender<WorkerMessage>,
        batch: rio_proto::types::BuildLogBatch,
    ) -> bool {
        let msg = WorkerMessage {
            msg: Some(worker_message::Msg::LogBatch(batch)),
        };
        log_tx.send(msg).await.is_ok()
    }

    let misc_fail = |m: &str| {
        Ok(BuildResult::failure(
            BuildStatus::MiscFailure,
            m.to_string(),
        ))
    };

    loop {
        if msg_count >= MAX_BUILD_STDERR_MESSAGES {
            return Err(wire::WireError::Io(std::io::Error::other(
                "exceeded maximum STDERR messages during build",
            )));
        }
        msg_count += 1;

        // Check for timeout-based flush.
        //
        // TODO(phase2b): honor the 100ms BATCH_TIMEOUT during silent periods.
        // Currently maybe_flush() only fires once per stderr message, so a
        // build that's silent for 60s (common: long compile that buffers
        // stdout) won't flush its partial batch until the next STDERR_NEXT
        // or STDERR_LAST arrives. The observability spec's "64 lines / 100ms"
        // guarantee is NOT upheld during quiet periods.
        //
        // The obvious fix (tokio::time::timeout around read_stderr_message)
        // is UNSAFE: dropping the read future mid-u64-read leaves partial
        // bytes consumed from the daemon stdout pipe; the next read desyncs
        // the Nix STDERR protocol. Safe fixes:
        //   (a) Spawn read_stderr_message into an owned task that pushes to
        //       a mpsc channel (cancel-safe); select! on rx.recv() + interval.
        //   (b) Fused-future pattern: hold the pinned read future across
        //       select! iterations, only recreate on completion.
        // Both require reworking the &mut R borrow. Impact is user-visible
        // log latency (build appears hung), not correctness.
        if let Some(batch) = batcher.maybe_flush()
            && !send_batch(log_tx, batch).await
        {
            return misc_fail("log channel closed during build (scheduler stream gone)");
        }

        match read_stderr_message(reader).await? {
            StderrMessage::Last => break,
            StderrMessage::Error(e) => {
                return misc_fail(e.message());
            }
            StderrMessage::Next(msg) => {
                if let Some(batch) = batcher.add_line(msg.into_bytes())
                    && !send_batch(log_tx, batch).await
                {
                    return misc_fail("log channel closed during build (scheduler stream gone)");
                }
            }
            StderrMessage::Read(_) => {
                return misc_fail("daemon sent STDERR_READ, not supported");
            }
            // Activity/progress messages we explicitly don't care about.
            StderrMessage::Write(_)
            | StderrMessage::StartActivity { .. }
            | StderrMessage::StopActivity { .. }
            | StderrMessage::Result { .. } => {}
        }
    }

    // Read BuildResult
    rio_nix::protocol::build::read_build_result(reader).await
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // Group 3: Daemon lifecycle
    // -----------------------------------------------------------------------

    /// Verify that run_daemon_build fails when the process doesn't speak the
    /// Nix protocol (handshake failure), and that the caller's always-kill
    /// pattern leaves no leaked process.
    #[tokio::test]
    async fn test_daemon_killed_on_handshake_failure() {
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
            rio_nix::derivation::DerivationOutput::new("out", "/nix/store/test-out", "", "")
                .unwrap();
        let basic_drv = rio_nix::derivation::BasicDerivation::new(
            vec![output],
            Default::default(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            Default::default(),
        )
        .unwrap();
        let mut batcher = LogBatcher::new("test.drv".into(), "test-worker".into());
        let (log_tx, _log_rx) = mpsc::channel(4);

        // run_daemon_build should fail quickly (handshake reads EOF from closed stdout).
        let result = tokio::time::timeout(
            Duration::from_secs(5),
            run_daemon_build(
                &mut fake_daemon,
                "/nix/store/test.drv",
                &basic_drv,
                Duration::from_secs(5),
                &mut batcher,
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
    }

    /// Verify that DAEMON_SETUP_TIMEOUT is shorter than the default daemon
    /// build timeout. If setup timeout were longer, it would be pointless.
    #[test]
    fn test_timeout_ordering() {
        assert!(
            DAEMON_SETUP_TIMEOUT < rio_common::grpc::DEFAULT_DAEMON_TIMEOUT,
            "setup timeout ({DAEMON_SETUP_TIMEOUT:?}) must be shorter than default daemon timeout ({:?})",
            rio_common::grpc::DEFAULT_DAEMON_TIMEOUT
        );
    }
}
