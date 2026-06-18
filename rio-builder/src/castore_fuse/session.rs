//! Client-side mount/serve assembly for the castore-FUSE (ADR-022 §2.5,
//! builder steps).
//!
//! [`mount_and_serve`] performs the unprivileged half of the mount
//! sequence: the rio-mountd `Mount{build_id}` handshake (which hands
//! back the `/dev/fuse` fd and the staging quota), the Directory-DAG
//! prefetch, the [`Opener`]/[`CastoreFs`] assembly, and
//! `Session::from_fd` + spawn so the filesystem is answering on its own
//! threads when the function returns. The executor places the returned
//! mountpoint under the per-build overlay as its only lower
//! (`executor::execute_build`); the `serve-castore` subcommand of
//! `spike_mountd_client` drives the same sequence inside the
//! `vm-castore` NixOS test.
//!
//! Teardown is per build: dropping the [`CastoreSession`] tears the
//! mount down — see its [`Drop`] impl for the canonical ordering
//! rationale. The builder never unmounts the castore mountpoint itself —
//! the mountpoint is mountd-owned.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::runtime::Handle;

use rio_proto::castore::{FileEntry, RootNode, SymlinkEntry, root_node};
use rio_proto::types::{InputRoot, NarEntryKind, NarIndex};

use super::CastoreFs;
use super::circuit::CircuitBreaker;
use super::mountd_client::{MountdClient, MountdError};
use super::mountd_proto::ErrKind;
use super::open::{Opener, OpenerConfig};
use super::tree::{InoMap, TreeError};
use crate::store_fetch::StoreClients;

/// Process-level castore-FUSE settings, mapped once from `Config` at
/// startup and shared by every build the pod runs. Per-build values
/// (build id, assignment token) are passed to [`mount_and_serve`] as
/// arguments.
#[derive(Clone)]
pub struct CastoreSettings {
    /// rio-mountd UDS socket path (`/run/rio-mountd.sock`).
    pub mountd_socket: PathBuf,
    /// Per-build mountpoint root (`/var/rio/castore`). mountd mounts
    /// the handed-off fd at `{castore_dir}/{build_id}`; this module
    /// only derives that path (for the overlay lower and the fusectl
    /// abort file) — it never mounts or unmounts it.
    pub castore_dir: PathBuf,
    /// Staging root (`/var/rio/staging`). The per-build dir under it is
    /// created, chowned, and quota'd by mountd at `Mount` time; this
    /// module only derives the path the `Promote` flow expects
    /// (`{staging_root}/{build_id}`).
    pub staging_root: PathBuf,
    /// Shared node backing cache (`/var/rio/cache`). mountd-owned,
    /// read-only to this process.
    pub cache_dir: PathBuf,
    /// Shared node chunk cache (`/var/rio/chunks`). mountd-owned,
    /// read-only to this process.
    pub chunks_dir: PathBuf,
    /// Budget for the mount-time `GetDirectory(recursive=true)`
    /// prefetch. Expiry is an infrastructure failure (re-queue), never
    /// a wedged mount.
    pub dag_prefetch_timeout: Duration,
    /// Slow-pool worker threads for the ring engine (clamped to ≥ 1) —
    /// cold `open()`s block a worker on the store fetch, so more than
    /// one keeps further cold opens moving while a fetch is in flight.
    /// Also the fuser thread count for the non-ring request classes.
    pub fuse_threads: usize,
    /// `open()` data-path tunables.
    pub opener: OpenerConfig,
    /// Fetch circuit breaker shared with the heartbeat loop
    /// (`store_degraded`) so a store outage observed by one build's
    /// opener is visible to the scheduler before the next dispatch.
    pub circuit: Arc<CircuitBreaker>,
}

impl CastoreSettings {
    /// Settings pointing at nonexistent paths under `dir` — for tests
    /// that never reach the mount (the kind gate, build-opts
    /// resolution). Compiled with the `test-fixtures` feature (default)
    /// like the rest of the crate's test scaffolding so integration
    /// tests can use it too.
    #[cfg(feature = "test-fixtures")]
    pub fn test_stub(dir: &Path) -> Self {
        Self {
            mountd_socket: dir.join("mountd.sock"),
            castore_dir: dir.join("castore"),
            staging_root: dir.join("staging"),
            cache_dir: dir.join("cache"),
            chunks_dir: dir.join("chunks"),
            dag_prefetch_timeout: Duration::from_secs(1),
            fuse_threads: 1,
            opener: OpenerConfig {
                stream_threshold: 8 * 1024 * 1024,
                mountd_request_timeout: Duration::from_secs(1),
                fetch_timeout: Duration::from_secs(1),
                max_backing_ids: 16,
                disable_passthrough: false,
            },
            circuit: Arc::new(CircuitBreaker::default()),
        }
    }
}

/// Errors from the mount/serve sequence. Every variant means the build
/// never started — callers classify them as infrastructure failures
/// (re-queue), not build failures.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    /// The mountd handshake (or a later mountd request) failed.
    #[error("mountd: {0}")]
    Mountd(#[from] MountdError),
    /// The closure DAG prefetch / [`InoMap`](crate::castore_fuse::tree::InoMap)
    /// construction failed.
    #[error("DAG prefetch: {0}")]
    Prefetch(#[from] TreeError),
    /// `Session::from_fd` on the handed-off `/dev/fuse` fd failed
    /// (FUSE_INIT or io_uring setup).
    #[error("FUSE session on the handed-off fd: {0}")]
    Fuse(#[from] std::io::Error),
}

/// A live castore-FUSE serving one build's closure. Holding one means
/// the mountd handshake, the DAG prefetch, and the FUSE_INIT handshake
/// (including passthrough negotiation) all succeeded — [`Self::mountpoint`]
/// is ready to be used as the overlay lower (or read directly, as the
/// VM test does). Dropping it tears the mount down — see [`Drop`].
pub struct CastoreSession {
    /// Keeps the FUSE request-loop threads alive — they exit on their
    /// own once the kernel aborts the connection after mountd detaches
    /// the mount (triggered by the UDS shutdown in `Drop`). Never read,
    /// hence the underscore.
    _fuse: fuser::BackgroundSession,
    /// The build's mountd connection. Kept so teardown can `shutdown(2)`
    /// the socket explicitly per build instead of waiting for process
    /// exit (the Opener inside the running session holds clones, so
    /// merely dropping this handle would not close the connection).
    mountd: MountdClient,
    /// `{castore_dir}/{build_id}` — the overlay lowerdir.
    mountpoint: PathBuf,
    /// fusectl `abort` control file for this connection, captured at
    /// mount time (computing it later would `stat()` through the very
    /// FUSE we are tearing down). `None` when fusectl is unavailable;
    /// teardown then degrades to UDS-shutdown-only.
    abort_path: Option<PathBuf>,
    /// The fuse-over-io_uring engine serving the session. Held so the
    /// ring buffers outlive the kernel's references; its threads wind
    /// down on the teardown CQEs that the abort write in [`Drop`]
    /// triggers (no join — same never-block contract as the fuser
    /// threads).
    _uring: super::uring::Engine,
    /// Kernel-enforced staging quota mountd applied at `Mount` time
    /// (0 = no quota configured on the daemon).
    pub staging_quota_bytes: u64,
}

impl CastoreSession {
    /// The mountd-owned per-build mountpoint serving this session's
    /// tree: the overlay lowerdir.
    pub fn mountpoint(&self) -> &Path {
        &self.mountpoint
    }
}

impl Drop for CastoreSession {
    /// Per-build teardown. Ordering:
    ///
    /// 1. `shutdown(2)` the mountd UDS — rio-mountd reacts to the EOF
    ///    by `MNT_DETACH`ing `{castore_dir}/{build_id}`, removing the
    ///    staging dir, and closing its kept `/dev/fuse` fd.
    /// 2. Write the connection's fusectl `abort` file (best-effort) so
    ///    any request still parked in the kernel queue errors out
    ///    immediately and the fuser worker threads' `read(/dev/fuse)`
    ///    returns ENODEV instead of waiting for the superblock to be
    ///    reclaimed — same I-165 abort discipline as the pre-ADR-022
    ///    process-level FUSE, now applied per build.
    /// 3. Drop the `BackgroundSession` (field drop). fuser does not
    ///    join the worker threads on drop, so this never blocks; the
    ///    threads exit on the ENODEV from step 1/2.
    ///
    /// The caller unmounts the overlay BEFORE dropping the session so
    /// the lower is not yanked out from under a live overlay.
    fn drop(&mut self) {
        self.mountd.shutdown();
        // r[impl builder.shutdown.fuse-abort+2]
        if let Some(abort_path) = &self.abort_path {
            match std::fs::write(abort_path, "1") {
                Ok(()) => tracing::debug!(
                    abort_path = %abort_path.display(),
                    "castore-FUSE connection aborted"
                ),
                Err(e) => tracing::debug!(
                    abort_path = %abort_path.display(),
                    error = %e,
                    "castore-FUSE abort write failed (connection likely already gone)"
                ),
            }
        }
        tracing::info!(
            mountpoint = %self.mountpoint.display(),
            "castore session torn down (mountd reaps the mountpoint and staging dir)"
        );
    }
}

/// Mount-handshake attempts when mountd answers with a transient/busy
/// rejection (see [`mount_rejection_is_transient`]). The exponential
/// schedule (400 ms · 2ⁿ → ~6 s total) absorbs the whole
/// teardown-in-flight class — including a daemon-version-skewed node
/// that still holds the claims for the previous build's staging
/// delete, which the pre-backoff schedule (2 × 400 ms) could not.
/// Genuinely dead daemons still surface within ~6 s — an infra retry,
/// not a wedge.
const MOUNT_RETRYABLE_ATTEMPTS: u32 = 5;

/// Base pause between transient Mount attempts; attempt `n` sleeps
/// `MOUNT_RETRY_BASE_DELAY << (n-1)`.
const MOUNT_RETRY_BASE_DELAY: Duration = Duration::from_millis(400);

/// `true` for Mount rejections a short retry can absorb: a re-dispatch
/// to the same pod can race mountd's teardown of the previous attempt's
/// connection — the build_id may still be claimed
/// ([`ErrKind::DuplicateBuildId`]) — or hit a daemon mid-restart
/// ([`MountdError::Closed`], EPIPE/ECONNRESET). Validation rejections
/// (`BadBuildId`, `AlreadyMounted`, …) are deterministic and never
/// retried.
fn mount_rejection_is_transient(err: &MountdError) -> bool {
    match err {
        MountdError::Closed => true,
        // The same daemon-side close observed from the send side
        // instead of the reader thread — which side sees it first is a
        // race. Only the peer-closed kinds; anything else (EMSGSIZE,
        // EBADF, …) is a local bug, not the daemon reaping a
        // predecessor.
        MountdError::Frame(super::mountd_proto::FrameError::Io(e)) => matches!(
            e.kind(),
            std::io::ErrorKind::BrokenPipe | std::io::ErrorKind::ConnectionReset
        ),
        MountdError::Rejected(kind) => {
            matches!(kind, ErrKind::DuplicateBuildId | ErrKind::Retryable(_))
        }
        _ => false,
    }
}

/// Connect + `Mount{build_id}` with bounded exponential backoff on
/// transient rejections. Each attempt is a fresh connection: a
/// transiently-rejected one is daemon-side doomed (typed rejection or
/// silent close), so reusing it can only return [`MountdError::Closed`]
/// again. Blocking, like [`mount_and_serve`].
fn mount_with_retry(
    socket: &Path,
    build_id: &str,
    request_timeout: Duration,
) -> Result<(MountdClient, u64, std::os::fd::OwnedFd), MountdError> {
    let mut attempt = 1;
    loop {
        let mountd = MountdClient::connect(socket)?;
        match mountd.mount(build_id, request_timeout) {
            Ok((quota, fd)) => return Ok((mountd, quota, fd)),
            Err(e) if attempt < MOUNT_RETRYABLE_ATTEMPTS && mount_rejection_is_transient(&e) => {
                let delay = MOUNT_RETRY_BASE_DELAY * 2u32.pow(attempt - 1);
                tracing::warn!(
                    build_id,
                    error = %e,
                    attempt,
                    delay_ms = delay.as_millis() as u64,
                    "mountd Mount rejected transiently; retrying"
                );
                drop(mountd);
                std::thread::sleep(delay);
                attempt += 1;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Run the full client-side mount/serve sequence: `Mount{build_id}`,
/// DAG prefetch, [`Opener`]/[`CastoreFs`] assembly, `Session::from_fd`,
/// spawn.
///
/// `build_id` is the per-build identity mountd uses as the basename of
/// the mountpoint (`{castore_dir}/{build_id}`) and staging dir; it must
/// satisfy mountd's `^[A-Za-z0-9_-]{1,64}$` validation (the executor
/// passes the drv's store hash). `assignment_token` is the HMAC token
/// sent as `x-rio-assignment-token` on every store RPC —
/// DirectoryService/ChunkService are tenant-scoped and refuse anonymous
/// callers; empty = no header (dev-mode store).
///
/// Blocking — call it from a non-async thread (a binary's `main`, or
/// `spawn_blocking` in the builder runtime); `runtime` is the tokio
/// runtime the prefetch and the `open()`-path fetches run on and must
/// outlive the returned session.
pub fn mount_and_serve(
    settings: &CastoreSettings,
    build_id: &str,
    assignment_token: &str,
    store: &StoreClients,
    input_roots: &[InputRoot],
    runtime: Handle,
) -> Result<CastoreSession, SessionError> {
    // Mount first: a rejected build_id or an unreachable daemon fails
    // fast, before the DAG prefetch. If anything later in this function
    // fails, dropping `mountd` (and the fd) closes the UDS connection
    // and the daemon detaches the mount and reaps the staging dir.
    // Transient/busy rejections get a bounded backoff-retry — see
    // `mount_rejection_is_transient`.
    let (mountd, staging_quota_bytes, fuse_fd) = mount_with_retry(
        &settings.mountd_socket,
        build_id,
        settings.opener.mountd_request_timeout,
    )?;

    let mut directory = store.directory.clone();
    let tree = Arc::new(runtime.block_on(InoMap::prefetch(
        &mut directory,
        input_roots,
        settings.dag_prefetch_timeout,
        assignment_token,
    ))?);
    tracing::info!(
        build_id,
        roots = input_roots.len(),
        inodes = tree.inode_count(),
        "castore Directory DAG prefetched"
    );

    // mountd created `{staging_root}/{build_id}` (and its `chunks/`
    // subdir) at Mount time and `Promote{digest}` reads back exactly
    // `{staging_root}/{build_id}/{hex(digest)}` — derive, never mkdir.
    let staging_dir = settings.staging_root.join(build_id);
    let opener = Arc::new(Opener::new(
        mountd.clone(),
        store.directory.clone(),
        store.chunk.clone(),
        assignment_token.to_owned(),
        runtime,
        Arc::clone(&settings.circuit),
        settings.cache_dir.clone(),
        settings.chunks_dir.clone(),
        staging_dir,
        settings.opener.clone(),
    ));

    // fuse-over-io_uring, step 1 of 3 (probe): create the rings and
    // dup the session fd BEFORE the INIT handshake. The INIT reply
    // advertises FUSE_OVER_IO_URING, after which the kernel holds
    // request processing until registration completes — so the rings
    // must already exist when the flag is offered. The transport is
    // mandatory: a failed probe (seccomp/sysctl-blocked io_uring, fd
    // limits) fails the mount with the kernel requirement named.
    // r[impl builder.fs.io-uring-required]
    let (uring_prep, ring_fd) = super::uring::prepare()
        .and_then(|prep| Ok((prep, fuse_fd.try_clone()?)))
        .map_err(|e| {
            std::io::Error::other(format!(
                "io_uring unavailable ({e}); the castore-FUSE serves exclusively over \
                 fuse-over-io_uring and requires Linux 6.14+ with fuse.enable_uring=1"
            ))
        })?;

    let fs = CastoreFs::new(Arc::clone(&tree));

    let mut fuse_cfg = fuser::Config::default();
    fuse_cfg.n_threads = Some(settings.fuse_threads.max(1));
    // SessionACL::All matches the `allow_other` mount option mountd
    // passed: the build's userns uids (not this process's) are the ones
    // traversing the tree.
    //
    // Step 2 of 3 (handshake): `init` inside `from_fd` requires the
    // kernel to accept FUSE_OVER_IO_URING — a kernel without it (or
    // without FUSE_PASSTHROUGH) fails the mount here, with the
    // requirement in the error.
    let session = fuser::Session::from_fd(fs, fuse_fd, fuser::SessionACL::All, fuse_cfg)?;
    // `spawn` only starts the fuser threads serving the non-ring
    // request classes (INTERRUPT, FORGET, notifications).
    let fuse = session.spawn()?;

    // Step 3 of 3 (register): with INIT done, register the rings.
    // `spawn` returns once every queue's REGISTER batch is parked in
    // the kernel; any rejection fails the mount — dropping the session
    // (and the mountd connection) closes the fuse connection, which
    // completes whatever did register.
    let uring = uring_prep
        .spawn(
            ring_fd,
            Arc::clone(&tree),
            Arc::clone(&opener),
            settings.fuse_threads.max(1),
        )
        .map_err(|e| {
            std::io::Error::other(format!(
                "fuse-over-io_uring ring registration failed ({e}); the castore-FUSE \
                 requires Linux 6.14+ with fuse.enable_uring=1"
            ))
        })?;

    // The serving session is up; capture the fusectl abort path NOW
    // (the stat below is answered by the threads we just spawned). At
    // teardown time the connection may already be half-dead.
    let mountpoint = settings.castore_dir.join(build_id);
    ensure_fusectl_mounted();
    let abort_path = fusectl_abort_path(&mountpoint);

    Ok(CastoreSession {
        _fuse: fuse,
        mountd,
        mountpoint,
        abort_path,
        _uring: uring,
        staging_quota_bytes,
    })
}

/// Map a `GetNarIndex` response to the path's castore [`RootNode`]: a
/// directory-rooted NAR carries its root `dir_digest` in `root_digest`;
/// a single-file or single-symlink NAR carries the full entry at index
/// 0 (path `""` — the NAR root has no name).
///
/// Used by the executor's `WorkAssignment.input_roots` fallback (the
/// scheduler sends `root_node: None` for paths whose `nar_index` row it
/// could not read) and by `spike_mountd_client`'s client-side root
/// resolution in the `vm-castore` test.
pub fn root_node_from_nar_index(index: &NarIndex) -> anyhow::Result<RootNode> {
    use anyhow::bail;

    if !index.root_digest.is_empty() {
        return Ok(RootNode {
            node: Some(root_node::Node::DirDigest(index.root_digest.clone())),
        });
    }
    let root = index
        .entries
        .first()
        .filter(|e| e.path.is_empty())
        .ok_or_else(|| anyhow::anyhow!("NAR index has no root entry"))?;
    let node = match NarEntryKind::try_from(root.kind) {
        Ok(NarEntryKind::Regular) => {
            if root.file_digest.len() != 32 {
                bail!(
                    "root file entry has a {}-byte file_digest, want 32",
                    root.file_digest.len()
                );
            }
            root_node::Node::File(FileEntry {
                name: Vec::new(),
                digest: root.file_digest.clone(),
                size: root.size,
                executable: root.executable,
            })
        }
        Ok(NarEntryKind::Symlink) => root_node::Node::Symlink(SymlinkEntry {
            name: Vec::new(),
            target: root.target.clone(),
        }),
        // A directory root with an empty root_digest (or an unknown
        // kind) means the index row is corrupt — refusing beats
        // mounting a tree whose root can never resolve.
        Ok(NarEntryKind::Directory | NarEntryKind::Unspecified) | Err(_) => {
            bail!("root entry kind {} has no usable digest", root.kind)
        }
    };
    Ok(RootNode { node: Some(node) })
}

/// Directory where the kernel exposes per-connection FUSE control files
/// (`abort`, `waiting`, `max_background`, …). One subdirectory per live
/// connection, named by the connection's kernel `dev_t` (== minor for
/// FUSE's anonymous superblocks). Populated only when the `fusectl`
/// pseudo-filesystem is mounted there — sysfs creates the directory
/// regardless, so an empty dir is the "not mounted" signal.
const FUSECTL_ROOT: &str = "/sys/fs/fuse/connections";

/// Ensure `fusectl` is mounted at [`FUSECTL_ROOT`]. Best-effort.
///
/// I-165b: in Bottlerocket + `hostUsers:false` containers, the host's
/// systemd-mounted fusectl is NOT propagated into the container's mount
/// namespace — `/sys/fs/fuse/connections/` exists (sysfs creates the
/// stub directory) but is empty. [`fusectl_abort_path`]'s existence
/// check then returns `None` and the per-build teardown loses its
/// abort-the-connection step. We hold `CAP_SYS_ADMIN` (the overlay
/// mount requires it), so mount fusectl ourselves. `EBUSY` (already
/// mounted — systemd-hosted dev box, or a prior build's call) is fine;
/// anything else is logged at warn and teardown degrades to
/// UDS-shutdown-only.
fn ensure_fusectl_mounted() {
    // fusectl is a virtual fs that enumerates live connections at
    // readdir time. The just-spawned session's connection shows up if
    // fusectl is mounted; an empty or unreadable dir means it isn't.
    let already = std::fs::read_dir(FUSECTL_ROOT)
        .map(|mut d| d.next().is_some())
        .unwrap_or(false);
    if already {
        tracing::debug!(root = FUSECTL_ROOT, "fusectl already mounted");
        return;
    }
    match nix::mount::mount(
        Some("fusectl"),
        FUSECTL_ROOT,
        Some("fusectl"),
        nix::mount::MsFlags::empty(),
        None::<&str>,
    ) {
        Ok(()) => tracing::info!(
            root = FUSECTL_ROOT,
            "mounted fusectl for castore-FUSE abort-on-teardown (I-165b)"
        ),
        // Already mounted (heuristic false-negative). Fine.
        Err(nix::errno::Errno::EBUSY) => {
            tracing::debug!(root = FUSECTL_ROOT, "fusectl mount EBUSY (already mounted)");
        }
        Err(e) => tracing::warn!(
            root = FUSECTL_ROOT,
            error = %e,
            "fusectl mount failed; castore-FUSE abort-on-teardown will no-op (I-165b)"
        ),
    }
}

/// Compute the fusectl `abort` control-file path for `mount_point`.
///
/// `/sys/fs/fuse/connections/<N>/abort` where `<N>` is the kernel's
/// `dev_t` for the mount's anonymous superblock. FUSE uses anonymous
/// block devices (major 0), so kernel `dev_t = MKDEV(0, minor) = minor`;
/// the directory name as printed by `fs/fuse/control.c`'s
/// `sprintf("%u", fc->dev)` is therefore the userspace minor number.
///
/// `None` if stat fails or fusectl isn't mounted. Called once at mount
/// time, NOT at teardown — see [`CastoreSession`]'s `Drop`.
fn fusectl_abort_path(mount_point: &Path) -> Option<PathBuf> {
    fusectl_abort_path_at(mount_point, Path::new(FUSECTL_ROOT))
}

/// [`fusectl_abort_path`] with an explicit connections-root. Split out
/// so unit tests can point at a tempdir instead of `/sys`.
fn fusectl_abort_path_at(mount_point: &Path, connections_root: &Path) -> Option<PathBuf> {
    let st_dev = match nix::sys::stat::stat(mount_point) {
        Ok(s) => s.st_dev,
        Err(e) => {
            tracing::warn!(
                mount_point = %mount_point.display(),
                error = %e,
                "stat(mount_point) failed; castore-FUSE abort path unavailable"
            );
            return None;
        }
    };
    // glibc-compatible `gnu_dev_minor()`. `nix` 0.31 doesn't expose
    // major/minor and `libc` isn't a direct dep; the encoding is stable
    // ABI (sys/sysmacros.h).
    let minor = (st_dev & 0xff) | ((st_dev >> 12) & 0xff_ff_ff_00);
    let abort = connections_root.join(minor.to_string()).join("abort");
    // Existence check: fusectl may not be mounted even after
    // [`ensure_fusectl_mounted`] (mount refused). Teardown then relies
    // on the UDS shutdown alone — warn, not debug (I-165b: the silent
    // no-op masked a prod regression for days).
    if abort.exists() {
        Some(abort)
    } else {
        tracing::warn!(
            path = %abort.display(),
            "fusectl abort path not present (fusectl not mounted?); \
             castore-FUSE abort-on-teardown disabled — see I-165b"
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::NarIndexEntry;

    /// Only the daemon-transient Mount rejection class is retried;
    /// validation rejections never are — retrying them just repeats a
    /// deterministic failure and delays the scheduler requeue.
    #[test]
    fn mount_retry_skips_validation_rejections() {
        // Transient/busy: daemon restart (surfaces as Closed),
        // build-id-still-claimed, daemon-side transient.
        assert!(mount_rejection_is_transient(&MountdError::Closed));
        // The same daemon-side close seen from the send path.
        assert!(mount_rejection_is_transient(&MountdError::Frame(
            super::super::mountd_proto::FrameError::Io(std::io::Error::from(
                std::io::ErrorKind::BrokenPipe
            ))
        )));
        assert!(!mount_rejection_is_transient(&MountdError::Frame(
            super::super::mountd_proto::FrameError::Io(std::io::Error::from(
                std::io::ErrorKind::InvalidInput
            ))
        )));
        assert!(mount_rejection_is_transient(&MountdError::Rejected(
            ErrKind::DuplicateBuildId
        )));
        assert!(mount_rejection_is_transient(&MountdError::Rejected(
            ErrKind::Retryable("staging reap in progress".into())
        )));
        // Deterministic: never retried.
        assert!(!mount_rejection_is_transient(&MountdError::Rejected(
            ErrKind::BadBuildId
        )));
        assert!(!mount_rejection_is_transient(&MountdError::Rejected(
            ErrKind::AlreadyMounted
        )));
        assert!(!mount_rejection_is_transient(&MountdError::Timeout(
            Duration::from_secs(1)
        )));
    }

    /// A scripted mountd stand-in that accepts SUCCESSIVE connections
    /// (unlike `testing::RecordingMountd`, which serves exactly one):
    /// each accepted connection consumes the next entry of `script`.
    /// `None` = close without replying (what a crashing or restarting
    /// daemon produces); `Some(resp)` = read one frame, reply to its
    /// seq, attach a `/dev/null` fd iff the resp is `Mounted`, keep the
    /// connection open until the client closes.
    fn scripted_serial_daemon(
        sock: &std::path::Path,
        script: Vec<Option<super::super::mountd_proto::Resp>>,
    ) -> std::thread::JoinHandle<usize> {
        use super::super::mountd_proto::{self as proto, Reply, Request, Resp};
        use nix::sys::socket::{
            AddressFamily, Backlog, SockFlag, SockType, UnixAddr, accept, bind, listen, socket,
        };
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let listener = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        bind(listener.as_raw_fd(), &UnixAddr::new(sock).unwrap()).unwrap();
        listen(&listener, Backlog::new(8).unwrap()).unwrap();
        std::thread::spawn(move || {
            let mut served = 0;
            let mut held = Vec::new();
            for entry in script {
                let conn = accept(listener.as_raw_fd()).unwrap();
                // SAFETY: accept(2) just returned a fresh fd we own.
                let conn = unsafe { OwnedFd::from_raw_fd(conn) };
                served += 1;
                let Some(resp) = entry else {
                    continue; // drop = close without reply
                };
                let frame = proto::recv_frame(conn.as_raw_fd()).unwrap();
                let req: Request = proto::decode(&frame.bytes).unwrap();
                let reply = Reply { seq: req.seq, resp };
                let bytes = proto::encode(&reply).unwrap();
                // `Mounted` carries one fd; the kernel holds its own
                // reference once sendmsg succeeds, so the local File can
                // drop right after.
                let null = std::fs::File::open("/dev/null").unwrap();
                let fds: Vec<i32> = if matches!(reply.resp, Resp::Mounted { .. }) {
                    vec![null.as_raw_fd()]
                } else {
                    Vec::new()
                };
                proto::send_frame(conn.as_raw_fd(), &bytes, &fds).unwrap();
                // Keep the successful connection open until the client
                // is done with it (dropping it here would EOF the
                // client's reader thread mid-test).
                if matches!(reply.resp, Resp::Mounted { .. }) {
                    held.push(conn);
                }
            }
            served
        })
    }

    /// `mount_with_retry` must absorb the transient-rejection class —
    /// a silent close (daemon restart) and a typed retryable rejection
    /// — and succeed on a later attempt. Red on the pre-backoff
    /// 2-attempt schedule.
    #[test]
    fn mount_with_retry_recovers_after_transient_rejections() {
        use super::super::mountd_proto::{ErrKind, Resp};
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("mountd.sock");
        let daemon = scripted_serial_daemon(
            &sock,
            vec![
                None, // attempt 1: silent close → MountdError::Closed
                Some(Resp::Err(ErrKind::Retryable(
                    "staging reap in progress".into(),
                ))),
                Some(Resp::Mounted {
                    staging_quota_bytes: 42,
                }),
            ],
        );

        let (client, quota, _fd) = mount_with_retry(&sock, "retry-build", Duration::from_secs(2))
            .expect("third attempt must succeed");
        assert_eq!(quota, 42);
        drop(client);
        assert_eq!(daemon.join().unwrap(), 3, "exactly three attempts");
    }

    /// Validation rejections fail fast: one connection, no retry —
    /// retrying a deterministic failure only delays the scheduler's
    /// requeue.
    #[test]
    fn mount_with_retry_fails_fast_on_validation_rejection() {
        use super::super::mountd_proto::{ErrKind, Resp};
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("mountd.sock");
        let daemon = scripted_serial_daemon(&sock, vec![Some(Resp::Err(ErrKind::BadBuildId))]);

        let err = match mount_with_retry(&sock, "bad/id", Duration::from_secs(2)) {
            Ok(_) => panic!("validation rejection must not be retried"),
            Err(e) => e,
        };
        assert!(matches!(err, MountdError::Rejected(ErrKind::BadBuildId)));
        assert_eq!(daemon.join().unwrap(), 1, "exactly one attempt");
    }

    // I-165b: the path computation must return Some when the
    // connections root is populated (see `ensure_fusectl_mounted` for
    // the incident background). Pins the contract against a
    // tempdir-backed fake root; the actual fusectl mount(2) needs
    // CAP_SYS_ADMIN and is exercised by the VM tests.
    #[test]
    fn fusectl_abort_path_resolves_when_connections_root_populated() {
        // Use the tempdir itself as both mount_point (statted for
        // st_dev) and connections_root parent. The minor we compute is
        // whatever device the test fs lives on — we don't care what
        // number, only that the path round-trips.
        let tmp = tempfile::tempdir().unwrap();
        let mount_point = tmp.path();
        let connections_root = tmp.path().join("connections");

        // Precondition: empty root → None (with a warn, not debug —
        // the I-165b severity bump).
        std::fs::create_dir(&connections_root).unwrap();
        assert_eq!(
            fusectl_abort_path_at(mount_point, &connections_root),
            None,
            "empty connections root must yield None"
        );

        // Compute the minor the same way the impl does, then
        // materialize the abort file as ensure_fusectl_mounted +
        // kernel would.
        let st_dev = nix::sys::stat::stat(mount_point).unwrap().st_dev;
        let minor = (st_dev & 0xff) | ((st_dev >> 12) & 0xff_ff_ff_00);
        let conn_dir = connections_root.join(minor.to_string());
        std::fs::create_dir(&conn_dir).unwrap();
        let abort = conn_dir.join("abort");
        std::fs::write(&abort, "").unwrap();

        // Postcondition: populated root → Some(exact path).
        assert_eq!(
            fusectl_abort_path_at(mount_point, &connections_root),
            Some(abort),
            "populated connections root must yield the abort path"
        );
    }

    #[test]
    fn fusectl_abort_path_none_on_stat_failure() {
        // Nonexistent mount_point → stat fails → None (warn-logged).
        // Guards the early-return arm.
        let nonexistent = Path::new("/nonexistent/rio-i165b-test-mount-point");
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(fusectl_abort_path_at(nonexistent, tmp.path()), None);
    }

    fn entry(path: &[u8], kind: NarEntryKind) -> NarIndexEntry {
        NarIndexEntry {
            path: path.to_vec(),
            kind: kind.into(),
            ..Default::default()
        }
    }

    /// Each NAR root shape maps to its `RootNode` variant: dir roots
    /// use `root_digest`, single-file roots carry digest/size/exec
    /// inline, symlink roots carry the target. Handling only the
    /// `root_digest` case would silently break every `.patch`/script
    /// input a closure declares.
    #[test]
    fn root_node_resolution_covers_all_root_kinds() {
        let dir = NarIndex {
            root_digest: vec![7u8; 32],
            entries: vec![entry(b"", NarEntryKind::Directory)],
        };
        let rn = root_node_from_nar_index(&dir).unwrap();
        assert_eq!(rn.node, Some(root_node::Node::DirDigest(vec![7u8; 32])));

        let file = NarIndex {
            root_digest: Vec::new(),
            entries: vec![NarIndexEntry {
                path: Vec::new(),
                kind: NarEntryKind::Regular.into(),
                size: 42,
                executable: true,
                file_digest: vec![9u8; 32],
                ..Default::default()
            }],
        };
        let rn = root_node_from_nar_index(&file).unwrap();
        assert_eq!(
            rn.node,
            Some(root_node::Node::File(FileEntry {
                name: Vec::new(),
                digest: vec![9u8; 32],
                size: 42,
                executable: true,
            }))
        );

        let link = NarIndex {
            root_digest: Vec::new(),
            entries: vec![NarIndexEntry {
                path: Vec::new(),
                kind: NarEntryKind::Symlink.into(),
                target: b"/nix/store/target".to_vec(),
                ..Default::default()
            }],
        };
        let rn = root_node_from_nar_index(&link).unwrap();
        assert_eq!(
            rn.node,
            Some(root_node::Node::Symlink(SymlinkEntry {
                name: Vec::new(),
                target: b"/nix/store/target".to_vec(),
            }))
        );
    }

    /// Corrupt or partial index rows must be refused, not mounted: a
    /// root that can never resolve produces confusing mid-build ENOENTs
    /// instead of a clear mount-time error.
    #[test]
    fn root_node_resolution_rejects_malformed_indexes() {
        let empty = NarIndex {
            root_digest: Vec::new(),
            entries: Vec::new(),
        };
        assert!(root_node_from_nar_index(&empty).is_err());

        // Regular root with a truncated file_digest.
        let short = NarIndex {
            root_digest: Vec::new(),
            entries: vec![NarIndexEntry {
                path: Vec::new(),
                kind: NarEntryKind::Regular.into(),
                file_digest: vec![1u8; 16],
                ..Default::default()
            }],
        };
        assert!(root_node_from_nar_index(&short).is_err());

        // Directory root but no root_digest — corrupt row.
        let dir_without_digest = NarIndex {
            root_digest: Vec::new(),
            entries: vec![entry(b"", NarEntryKind::Directory)],
        };
        assert!(root_node_from_nar_index(&dir_without_digest).is_err());

        // First entry is not the root (path non-empty).
        let no_root = NarIndex {
            root_digest: Vec::new(),
            entries: vec![entry(b"bin", NarEntryKind::Directory)],
        };
        assert!(root_node_from_nar_index(&no_root).is_err());
    }
}
