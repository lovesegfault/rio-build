//! The per-build castore-FUSE mount sequence (ADR-022 §2.5, P0560 §A).
//!
//! [`mount_castore_background`] is the builder-side orchestration that
//! turns a `WorkAssignment`'s castore input roots into a served FUSE
//! mount the per-build overlay can stack on:
//!
//! 1. prefetch the closure's Directory DAG ([`super::tree::build_tree`],
//!    one multi-root `GetDirectory(recursive=true)` call);
//! 2. open `/dev/fuse` (the device node every executor pod carries via
//!    containerd's `base_runtime_spec`), connect the node's `rio-mountd`
//!    UDS, and send `Mount{build_id}` with a dup of that fd in
//!    `SCM_RIGHTS` — the daemon keeps it as the target for later
//!    `BackingOpen` ioctls and sets up the build's staging dir + quota
//!    (it opens no devices and mounts nothing; see the protocol note
//!    below);
//! 3. `mount(2)` our own fd on a builder-owned mountpoint inside the
//!    builder's own mount namespace (`fuse.rio-castore`,
//!    `MS_NODEV|MS_NOSUID`);
//! 4. serve the connection ([`fuser::Session::from_fd`] answers
//!    `FUSE_INIT` before returning; `spawn()` starts the worker
//!    threads) and only then return, so the caller can mount the
//!    overlay with this mountpoint as `lowerdir`.
//!
//! # Mount-propagation decision (P0560, option b)
//!
//! `rio-mountd` ships unprivileged (`CAP_SYS_ADMIN`, NOT `privileged`),
//! so a mount made inside its container cannot propagate to builder
//! pods. The `mount(2)` therefore lives HERE, in the builder: the
//! builder already holds `CAP_SYS_ADMIN` in its own user namespace (it
//! mounts the per-build overlay today). The castore mount dies with the
//! builder's mount namespace for free, and mountd keeps zero
//! mount-lifetime state.
//!
//! The builder must also be the one to **open** `/dev/fuse`: the kernel
//! only accepts a fuse `mount(2)` from the same user namespace that
//! opened the device fd (`fs/fuse/inode.c`, "Require mount to happen
//! from the same user namespace which opened /dev/fuse"), so a
//! daemon-opened fd could never be mounted by a userns-confined
//! builder. The fd therefore flows builder → daemon (for the
//! backing-open broker), not daemon → builder.
//!
//! `allow_other` is deliberately NOT set: every consumer reads the
//! castore tree *through the per-build overlay*, and overlayfs performs
//! lower-layer access with the overlay mounter's credentials — which is
//! this process. Nothing outside the builder's uid touches the castore
//! mountpoint directly, so the smaller exposure wins
//! (`default_permissions` still applies the 0444/0555 modes the tree
//! reports). Flip to `allow_other` only if a consumer that bypasses the
//! overlay ever appears.
//!
//! # Ordering (P0541 gotcha)
//!
//! overlayfs probes its lowerdirs during `mount(2)`. If the castore
//! FUSE fd is mounted but nobody is answering requests, that probe
//! parks the mounting thread in the kernel forever. The serve-handle is
//! therefore spawned *inside* this function, before it returns — by
//! construction the overlay mount (in the caller) cannot start until
//! the castore session is live.
// r[impl builder.fs.castore-stack]
// r[impl builder.fs.fd-handoff-ordering+2]

use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use nix::mount::{MntFlags, MsFlags, mount, umount2};
use tokio::runtime::Handle;

use rio_proto::castore::RootNode;

use super::circuit::CircuitBreaker;
use super::fs::CastoreFs;
use super::mountd_client::{MountdClient, MountdError};
use super::open::{OpenConfig, OpenPath};
use super::tree::{self, TreeError};
use crate::store_fetch::StoreClients;

/// Maximum length of a mountd `build_id`
/// (`r[builder.mountd.build-id-validated]`). Mirrored here so the
/// builder can derive a compliant id without importing the daemon
/// module.
pub const MOUNTD_BUILD_ID_MAX_LEN: usize = 64;

/// Builder-side castore/mountd configuration, lifted from
/// [`crate::config::Config`] once at startup and carried into every
/// build's [`mount_castore_background`] call.
#[derive(Clone, Debug)]
pub struct CastoreOptions {
    /// `rio-mountd` UDS path (`/run/rio-mountd/mountd.sock`).
    pub mountd_socket: PathBuf,
    /// Shared node-SSD backing cache root (`/var/rio/cache`),
    /// mountd-owned, builder-readonly.
    pub cache_dir: PathBuf,
    /// Shared node-SSD chunk cache root (`/var/rio/chunks`),
    /// mountd-owned, builder-readonly.
    pub chunks_dir: PathBuf,
    /// Per-build staging root (`/var/rio/staging`); the daemon creates
    /// `{staging_root}/{build_id}` at `Mount` time, owned by this uid.
    pub staging_root: PathBuf,
    /// Whole-file JIT fetch budget (`ReadBlob` + verify).
    pub jit_fetch_timeout: Duration,
    /// Per-request mountd UDS round-trip budget.
    pub mountd_request_timeout: Duration,
    /// Mount-time `GetDirectory(recursive)` DAG prefetch budget.
    pub dag_prefetch_timeout: Duration,
    /// Files larger than this take the P0575 streaming open path.
    pub stream_threshold: u64,
    /// Per-build concurrent-open ceiling (≈ live backing registrations).
    pub max_backing_ids: usize,
    /// `false` disables FUSE passthrough (userspace reads) — the
    /// `RIO_DISABLE_PASSTHROUGH` escape hatch.
    pub passthrough: bool,
    /// fuser worker threads serving this mount.
    pub fuse_threads: u32,
}

impl CastoreOptions {
    /// Project the builder [`Config`](crate::config::Config) into the
    /// castore mount options.
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            mountd_socket: cfg.mountd_socket.clone(),
            cache_dir: cfg.castore_cache_dir.clone(),
            chunks_dir: cfg.castore_chunks_dir.clone(),
            staging_root: cfg.castore_staging_dir.clone(),
            jit_fetch_timeout: cfg.jit_fetch_timeout,
            mountd_request_timeout: cfg.mountd_request_timeout,
            dag_prefetch_timeout: cfg.dag_prefetch_timeout,
            stream_threshold: cfg.stream_threshold_bytes,
            max_backing_ids: cfg.max_backing_ids,
            passthrough: !cfg.disable_passthrough,
            fuse_threads: cfg.fuse_threads,
        }
    }
}

/// The per-build inputs to a castore mount, extracted from the
/// `WorkAssignment`: the (mountd-compliant) build id, the closure's
/// castore roots, and the HMAC assignment token every castore RPC
/// carries as `x-rio-assignment-token` (rio-store derives the caller's
/// tenant from it — `r[store.castore.tenant-scope]`; without it the
/// DAG prefetch and every JIT fetch are rejected as `UNAUTHENTICATED`).
pub struct MountInputs<'a> {
    pub build_id: &'a str,
    pub roots: &'a [(String, RootNode)],
    pub assignment_token: &'a str,
}

/// Why a castore mount could not be assembled. Every variant is an
/// infrastructure failure (the build never started, no partial state);
/// the messages name the unhealthy component so the on-call signal is
/// actionable.
#[derive(Debug, thiserror::Error)]
pub enum CastoreMountError {
    /// The DAG prefetch failed: the store has not indexed an input, the
    /// stream returned no Directory bodies for a root, the prefetch
    /// timed out, or the assignment carried an unindexed root.
    #[error(
        "castore DAG prefetch failed: {0} — is rio-store healthy and has its NAR indexer \
         processed every input path?"
    )]
    Tree(#[from] TreeError),
    /// rio-store rejected a castore RPC as unauthenticated. The builder
    /// attaches the build's HMAC assignment token to every castore RPC;
    /// this firing means the token is missing/invalid, carries no
    /// tenant claim, or the store has no matching HMAC verifier.
    #[error(
        "rio-store rejected the castore DAG prefetch as unauthenticated: {source} — the \
         builder sends the build's HMAC assignment token (x-rio-assignment-token) on every \
         castore RPC; check that the scheduler signs assignment tokens with a tenant claim \
         (hmac key + tenanted submission) and that rio-store is configured with the matching \
         HMAC verifier"
    )]
    Unauthenticated {
        #[source]
        source: tonic::Status,
    },
    /// Opening `/dev/fuse` failed — the device node is missing from the
    /// pod or the device cgroup denies it.
    #[error(
        "cannot open /dev/fuse: {0} — is the fuse device injected into executor pods \
         (containerd base_runtime_spec)?"
    )]
    FuseDevice(#[source] std::io::Error),
    /// `connect(2)` to the mountd socket failed.
    #[error(
        "cannot connect to rio-mountd at {socket}: {source} — rio-mountd not running on this \
         node — is the DaemonSet deployed?"
    )]
    MountdConnect {
        socket: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The `Mount{build_id}` request failed.
    #[error("rio-mountd Mount{{{build_id}}} failed: {source}")]
    Mountd {
        build_id: String,
        #[source]
        source: MountdError,
    },
    /// Creating the builder-owned mountpoint directory failed.
    #[error("cannot create castore mountpoint {path}: {source}")]
    MountPoint {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// The `mount(2)` of the handed-off fd failed.
    #[error("mount(fuse.rio-castore) at {path} failed: {source}")]
    Mount {
        path: PathBuf,
        #[source]
        source: nix::errno::Errno,
    },
    /// Creating or spawning the fuser session over the handed-off fd
    /// failed (the `FUSE_INIT` handshake is part of session creation).
    #[error("castore-FUSE session over the handed-off fd failed: {0}")]
    Serve(#[source] std::io::Error),
}

/// Derive a mountd-compliant build id (`^[A-Za-z0-9_-]{1,64}$`) from an
/// arbitrary identifier: out-of-class bytes collapse to `_` and the
/// result is truncated to [`MOUNTD_BUILD_ID_MAX_LEN`]. Store-path-derived
/// ids keep their leading 32-char nixbase32 hash, so truncation never
/// makes two different derivations collide.
pub fn mountd_build_id(raw: &str) -> String {
    let mut id: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .take(MOUNTD_BUILD_ID_MAX_LEN)
        .collect();
    if id.is_empty() {
        id.push('_');
    }
    id
}

/// Everything [`mount_castore_background`] assembles *before* the
/// privileged `mount(2)`: the prefetched filesystem, the mountd
/// connection, and this build's own `/dev/fuse` fd (a dup of which the
/// daemon now holds). Split out so the orchestration (DAG prefetch, fd
/// handoff, error mapping) is unit-testable without `CAP_SYS_ADMIN`.
pub(super) struct PreparedMount {
    pub(super) fs: CastoreFs,
    pub(super) client: MountdClient,
    pub(super) fuse_fd: std::os::fd::OwnedFd,
    pub(super) staging_quota_bytes: u64,
}

/// Steps (1) and (2): DAG prefetch, opening `/dev/fuse`, and the mountd
/// `Mount{build_id}` handshake. Blocking (bridges the async prefetch
/// via `runtime.block_on`); call from a thread that may block.
pub(super) fn prepare_mount(
    inputs: &MountInputs<'_>,
    clients: StoreClients,
    runtime: Handle,
    circuit: Arc<CircuitBreaker>,
    opts: &CastoreOptions,
) -> Result<PreparedMount, CastoreMountError> {
    let build_id = inputs.build_id;
    // ── (1) Directory-DAG prefetch. One multi-root recursive
    // GetDirectory for the whole closure; an unindexed root or an empty
    // stream is a typed, actionable error. An auth rejection gets its
    // own variant: "the indexer is behind" and "the store refused the
    // token" need very different operators.
    let tree = runtime
        .block_on(tree::build_tree(
            &clients,
            inputs.roots,
            opts.dag_prefetch_timeout,
            inputs.assignment_token,
        ))
        .map_err(|e| match e {
            TreeError::Rpc(status)
                if matches!(
                    status.code(),
                    tonic::Code::Unauthenticated | tonic::Code::PermissionDenied
                ) =>
            {
                CastoreMountError::Unauthenticated { source: status }
            }
            other => CastoreMountError::Tree(other),
        })?;
    if tree.is_empty() {
        // Legal (a closure-less derivation mounts an empty tree), but
        // worth a breadcrumb: an empty `input_roots` can also mean the
        // scheduler's closure compute failed or the store is unindexed.
        tracing::warn!(
            build_id,
            "castore mount has no input roots; the build sees an empty /nix/store lower"
        );
    }

    // ── (2) Open our own /dev/fuse — the kernel only accepts a fuse
    // mount(2) from the user namespace that opened the device — then
    // the mountd handshake: claim the build id, hand the daemon a dup
    // of the fd (its target for BackingOpen ioctls), get staging +
    // quota.
    let fuse_fd: std::os::fd::OwnedFd = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/fuse")
        .map_err(CastoreMountError::FuseDevice)?
        .into();
    let client = MountdClient::connect(&opts.mountd_socket).map_err(|source| {
        CastoreMountError::MountdConnect {
            socket: opts.mountd_socket.clone(),
            source,
        }
    })?;
    let staging_quota_bytes = client
        .mount(build_id, fuse_fd.as_raw_fd(), opts.mountd_request_timeout)
        .map_err(|source| CastoreMountError::Mountd {
            build_id: build_id.to_string(),
            source,
        })?;

    // ── Assemble the filesystem that will serve the fd.
    let open_path = OpenPath::new(
        opts.cache_dir.clone(),
        opts.staging_root.join(build_id),
        opts.chunks_dir.clone(),
        clients,
        runtime,
        client.clone(),
        circuit,
        inputs.assignment_token.to_owned(),
        OpenConfig {
            jit_fetch_timeout: opts.jit_fetch_timeout,
            mountd_request_timeout: opts.mountd_request_timeout,
            stream_threshold: opts.stream_threshold,
        },
    );
    let fs = CastoreFs::new(tree, open_path, opts.max_backing_ids, opts.passthrough);
    Ok(PreparedMount {
        fs,
        client,
        fuse_fd,
        staging_quota_bytes,
    })
}

/// Mount and serve the per-build castore FUSE. Returns only once the
/// FUSE session is live (INIT answered, worker threads running), so the
/// caller may immediately mount the per-build overlay with
/// [`CastoreMount::mount_point`] as its `lowerdir`.
///
/// Blocking: performs `mount(2)` and bridges the async DAG prefetch via
/// `runtime.block_on` — call from `spawn_blocking` (the executor does).
///
/// `mount_point` must be a builder-owned path on a filesystem the
/// builder can `mkdir` (the executor uses
/// `{overlay_base_dir}/{build_id}.castore`); it is created if missing
/// and removed again at teardown. `inputs.build_id` must already be
/// mountd-compliant (see [`mountd_build_id`]).
pub fn mount_castore_background(
    mount_point: &Path,
    inputs: &MountInputs<'_>,
    clients: StoreClients,
    runtime: Handle,
    circuit: Arc<CircuitBreaker>,
    opts: &CastoreOptions,
) -> Result<CastoreMount, CastoreMountError> {
    let prepared = prepare_mount(inputs, clients, runtime, circuit, opts)?;
    serve_prepared(prepared, mount_point, inputs.build_id, opts)
}

/// Steps (3) and (4): mount the handed-off fd and start serving it.
fn serve_prepared(
    prepared: PreparedMount,
    mount_point: &Path,
    build_id: &str,
    opts: &CastoreOptions,
) -> Result<CastoreMount, CastoreMountError> {
    let PreparedMount {
        fs,
        client,
        fuse_fd,
        staging_quota_bytes,
    } = prepared;

    std::fs::create_dir_all(mount_point).map_err(|source| CastoreMountError::MountPoint {
        path: mount_point.to_path_buf(),
        source,
    })?;

    // ── (3) The builder's own mount(2) of the handed-off fd, inside its
    // own mount namespace (option b of the P0560 mount-propagation
    // decision — mountd does not mount anything).
    //
    // rootmode=40555: directory, world-readable. user_id/group_id are
    // this process's euid/egid as seen in its own user namespace —
    // required by the kernel to map into the mounting userns, and the
    // identity every overlay-mediated access carries (overlayfs uses
    // the overlay mounter's creds for lower access). No allow_other:
    // see the module docs.
    let data = format!(
        "fd={},rootmode=40555,user_id={},group_id={},default_permissions",
        fuse_fd.as_raw_fd(),
        nix::unistd::geteuid().as_raw(),
        nix::unistd::getegid().as_raw(),
    );
    if let Err(source) = mount(
        Some("rio-castore"),
        mount_point,
        Some("fuse.rio-castore"),
        MsFlags::MS_NODEV | MsFlags::MS_NOSUID,
        Some(data.as_str()),
    ) {
        // Nothing got mounted — remove the mountpoint dir we just
        // created so a retry (or the overlay-base sweep) doesn't find a
        // stray empty `<build_id>.castore` left behind. Mirrors the
        // from_fd/spawn error arms below.
        let _ = std::fs::remove_dir(mount_point);
        return Err(CastoreMountError::Mount {
            path: mount_point.to_path_buf(),
            source,
        });
    }

    // ── (4) Serve. `Session::from_fd` answers the FUSE_INIT request the
    // mount(2) above queued before it returns; `spawn()` starts the
    // worker threads. From here on anything may probe the mountpoint —
    // including the caller's overlay mount(2) — without deadlocking.
    let mut config = fuser::Config::default();
    config.n_threads = Some(opts.fuse_threads.max(1) as usize);
    let session = match fuser::Session::from_fd(fs, fuse_fd, fuser::SessionACL::Owner, config) {
        Ok(s) => s,
        Err(e) => {
            // The mountpoint exists but will never be served — undo the
            // mount before surfacing the error so nothing later blocks
            // probing it.
            let _ = umount2(mount_point, MntFlags::MNT_DETACH);
            let _ = std::fs::remove_dir(mount_point);
            return Err(CastoreMountError::Serve(e));
        }
    };
    let session = match session.spawn() {
        Ok(bg) => bg,
        Err(e) => {
            let _ = umount2(mount_point, MntFlags::MNT_DETACH);
            let _ = std::fs::remove_dir(mount_point);
            return Err(CastoreMountError::Serve(e));
        }
    };

    // Capture the fusectl abort path NOW, while the session threads are
    // healthy — computing it requires stat(mount_point), which is a
    // FUSE getattr(ROOT) upcall.
    ensure_fusectl_mounted();
    let abort_path = fusectl_abort_path(mount_point);

    tracing::info!(
        build_id,
        mount_point = %mount_point.display(),
        staging_quota_bytes,
        threads = opts.fuse_threads,
        passthrough = opts.passthrough,
        "castore FUSE mounted and serving"
    );

    Ok(CastoreMount {
        mount_point: mount_point.to_path_buf(),
        session: Some(session),
        client: Some(client),
        abort_path,
        staging_quota_bytes,
        torn_down: false,
    })
}

/// A live per-build castore FUSE mount: the serve session, the mountd
/// connection, and the builder-owned mountpoint.
///
/// Tear down explicitly via [`CastoreMount::teardown`] (the executor
/// does, after the overlay above it is gone); `Drop` performs the same
/// sequence as a safety net for early-error paths.
pub struct CastoreMount {
    mount_point: PathBuf,
    /// The fuser worker threads serving the handed-off fd. `None` only
    /// after teardown.
    session: Option<fuser::BackgroundSession>,
    /// A handle on the mountd UDS connection. The daemon reaps this
    /// build's staging dir and releases its build_id/uid claims when
    /// the connection CLOSES — i.e. when the *last* clone of this
    /// client drops, not when this field is taken: the serve session's
    /// `OpenPath` and any still-running streaming-fill thread hold
    /// clones, so the reap can lag teardown by however long those take
    /// to wind down. `None` only after teardown.
    client: Option<MountdClient>,
    /// fusectl `abort` control file for this connection, captured at
    /// mount time (statting the mountpoint at teardown time would queue
    /// behind the very requests an abort exists to flush).
    abort_path: Option<PathBuf>,
    staging_quota_bytes: u64,
    torn_down: bool,
}

impl CastoreMount {
    /// The builder-owned mountpoint — the overlay's `lowerdir`.
    pub fn mount_point(&self) -> &Path {
        &self.mount_point
    }

    /// The kernel-enforced project quota mountd applied to this build's
    /// staging directory (0 = no quota on this node).
    pub fn staging_quota_bytes(&self) -> u64 {
        self.staging_quota_bytes
    }

    /// Tear the mount down: detach the mountpoint, release our handle on
    /// the mountd connection (the daemon reaps staging and releases the
    /// build's claims once the connection's last clone drops — see the
    /// `client` field doc), then abort the FUSE connection so any
    /// kernel-side waiter unblocks with `ENOTCONN` instead of parking in
    /// D-state — the same I-165 abort discipline the pre-cutover FUSE
    /// shutdown used.
    // r[impl builder.shutdown.fuse-abort]
    pub fn teardown(mut self) {
        self.teardown_inner();
    }

    fn teardown_inner(&mut self) {
        if self.torn_down {
            return;
        }
        self.torn_down = true;

        // 1. Detach the mountpoint. MNT_DETACH so a straggling open
        // somewhere cannot wedge teardown; the superblock dies when the
        // last reference does.
        if let Err(e) = umount2(&self.mount_point, MntFlags::MNT_DETACH) {
            tracing::warn!(
                mount_point = %self.mount_point.display(),
                error = %e,
                "castore umount2 failed (continuing teardown)"
            );
        }
        let _ = std::fs::remove_dir(&self.mount_point);

        // 2. Release this handle on the UDS connection. The daemon's
        // teardown (reap staging/{build_id}, release the build_id and
        // uid claims, close its kept /dev/fuse dup) fires when the
        // connection actually closes — that is, once the LAST clone of
        // the client drops: the serve session's OpenPath (released at
        // step 3 / when the worker threads exit) and any detached
        // streaming-fill thread also hold clones, so the daemon-side
        // reap is prompt but not synchronous with this line.
        drop(self.client.take());

        // 3. Abort the FUSE connection. Pending kernel-side requests get
        // ECONNABORTED and the fuser worker threads' next read returns
        // ENODEV, so they exit instead of lingering. Best-effort: if
        // fusectl is unavailable (no init-ns CAP_SYS_ADMIN to mount it),
        // the threads still exit once the detached superblock is
        // released.
        if let Some(abort) = &self.abort_path {
            match std::fs::write(abort, "1") {
                Ok(()) => tracing::debug!(
                    abort = %abort.display(),
                    "castore FUSE connection aborted"
                ),
                // The connection's fusectl dir is removed when the
                // superblock dies; with the daemon already reaped and the
                // mountpoint detached above, that routinely happens before
                // this write. Nothing left to abort — the desired end
                // state — so don't WARN about it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => tracing::debug!(
                    abort = %abort.display(),
                    "castore FUSE connection already released; nothing to abort"
                ),
                Err(e) => tracing::warn!(
                    abort = %abort.display(),
                    error = %e,
                    "castore FUSE abort failed; serve threads exit when the superblock is released"
                ),
            }
        }
        // Dropping the BackgroundSession does NOT join the worker
        // threads — deliberate: they exit on ENODEV (post-abort or
        // post-detach) on their own, and a join here could park teardown
        // behind a leaked open if the abort path was unavailable.
        drop(self.session.take());
    }
}

impl Drop for CastoreMount {
    fn drop(&mut self) {
        self.teardown_inner();
    }
}

// ─── fusectl abort plumbing ────────────────────────────────────────────
//
// Ported verbatim from the pre-cutover `fuse` module (I-165/I-165b): the
// abort write is the only thing that wakes kernel-side waiters parked on
// an unanswered FUSE request, and fusectl is not mounted in every
// container. The old module re-uses these via `crate::castore_fuse::mount`
// until P0560 §A's deletion removes it.

/// Directory where the kernel exposes per-connection FUSE control files
/// (`abort`, `waiting`, …). Populated only when the `fusectl`
/// pseudo-filesystem is mounted there — sysfs creates the directory
/// regardless, so an empty dir is the "not mounted" signal.
const FUSECTL_ROOT: &str = "/sys/fs/fuse/connections";

/// Ensure `fusectl` is mounted at [`FUSECTL_ROOT`]. Best-effort.
///
/// I-165b: in Bottlerocket + `hostUsers:false` containers the host's
/// systemd-mounted fusectl is NOT propagated into the container's mount
/// namespace. Mounting it requires init-namespace `CAP_SYS_ADMIN`, which
/// a userns-confined builder does not hold — the attempt then fails with
/// `EPERM`, is logged, and the abort degrades to "threads exit when the
/// superblock is released" (the mount itself was already detached).
pub(crate) fn ensure_fusectl_mounted() {
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
            "mounted fusectl for FUSE abort-on-teardown (I-165b)"
        ),
        // Already mounted (heuristic false-negative). Fine.
        Err(nix::errno::Errno::EBUSY) => {
            tracing::debug!(root = FUSECTL_ROOT, "fusectl mount EBUSY (already mounted)");
        }
        Err(e) => tracing::warn!(
            root = FUSECTL_ROOT,
            error = %e,
            "fusectl mount failed; FUSE abort-on-teardown will no-op (I-165b)"
        ),
    }
}

/// Compute the fusectl `abort` control-file path for `mount_point`:
/// `/sys/fs/fuse/connections/<minor>/abort`, where `<minor>` is the
/// mount's anonymous-device minor. `None` if stat fails or fusectl is
/// not mounted. Call at mount time, NOT at abort time.
pub(crate) fn fusectl_abort_path(mount_point: &Path) -> Option<PathBuf> {
    fusectl_abort_path_at(mount_point, Path::new(FUSECTL_ROOT))
}

/// [`fusectl_abort_path`] with an explicit connections-root, so unit
/// tests can point at a tempdir instead of `/sys`.
pub(crate) fn fusectl_abort_path_at(
    mount_point: &Path,
    connections_root: &Path,
) -> Option<PathBuf> {
    let st_dev = match nix::sys::stat::stat(mount_point) {
        Ok(s) => s.st_dev,
        Err(e) => {
            tracing::warn!(
                mount_point = %mount_point.display(),
                error = %e,
                "stat(mount_point) failed; FUSE abort path unavailable"
            );
            return None;
        }
    };
    // glibc-compatible `gnu_dev_minor()`. FUSE superblocks use anonymous
    // block devices (major 0), so the fusectl directory name is the
    // minor number.
    let minor = (st_dev & 0xff) | ((st_dev >> 12) & 0xff_ff_ff_00);
    let abort = connections_root.join(minor.to_string()).join("abort");
    if abort.exists() {
        Some(abort)
    } else {
        tracing::warn!(
            path = %abort.display(),
            "fusectl abort path not present (fusectl not mounted?); \
             FUSE abort-on-teardown disabled — see I-165b"
        );
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `mountd_build_id` always produces something the daemon's
    /// validator accepts, and never lets two distinct store-path hashes
    /// collide (truncation keeps the 32-char hash prefix).
    #[test]
    fn mountd_build_id_is_always_compliant() {
        let valid = |id: &str| {
            !id.is_empty()
                && id.len() <= MOUNTD_BUILD_ID_MAX_LEN
                && id
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
        };

        for raw in [
            "abcd1234",
            "vwb2lprckpd4kbg67sczakiqqqd4jxzy-llvm-tblgen-src-21_1_8_drv",
            // I-167 shape: query strings and dots collapse to '_'.
            "q2x9mcm9hzf2cdh22cbjmaqm6qmh1k1f-opensp-1.5.2-c11-using.patch?id=688d9675",
            // Longer than 64 chars once sanitized → truncated.
            &format!("{}-{}", "a".repeat(32), "b".repeat(64)),
            "",
            "weird/../traversal",
        ] {
            let id = mountd_build_id(raw);
            assert!(valid(&id), "{raw:?} → {id:?} is not mountd-compliant");
        }

        // The discriminating hash prefix survives truncation.
        let a = mountd_build_id(&format!("{}-{}", "a".repeat(32), "x".repeat(80)));
        let b = mountd_build_id(&format!("{}-{}", "b".repeat(32), "x".repeat(80)));
        assert_ne!(a, b, "distinct drv hashes must stay distinct");
        assert_eq!(a.len(), MOUNTD_BUILD_ID_MAX_LEN);
    }

    /// Connecting to a socket nobody listens on must produce the
    /// actionable "is the DaemonSet deployed?" error, not a bare errno.
    #[test]
    fn connect_refused_is_actionable() {
        let missing = PathBuf::from("/nonexistent/rio-mountd/mountd.sock");
        let err = match MountdClient::connect(&missing) {
            Err(e) => e,
            Ok(_) => panic!("connect to a nonexistent socket cannot succeed"),
        };
        let mapped = CastoreMountError::MountdConnect {
            socket: missing,
            source: err,
        };
        let msg = mapped.to_string();
        assert!(
            msg.contains("rio-mountd not running on this node")
                && msg.contains("is the DaemonSet deployed?"),
            "got: {msg}"
        );
    }

    /// The DAG-prefetch failure wrapper keeps the underlying tree error
    /// (which names the unindexed path / missing digest) and adds the
    /// "is the indexer running" hint.
    #[test]
    fn tree_error_is_actionable() {
        let err = CastoreMountError::Tree(TreeError::EmptyRootNode(
            "/nix/store/aaaa-unindexed".to_string(),
        ));
        let msg = err.to_string();
        assert!(
            msg.contains("aaaa-unindexed") && msg.contains("NAR indexer"),
            "got: {msg}"
        );
    }

    // The fusectl path computation, ported with the helper from the old
    // FUSE module (I-165b regression coverage).
    // r[verify builder.shutdown.fuse-abort]
    #[test]
    fn fusectl_abort_path_resolves_when_connections_root_populated() {
        let tmp = tempfile::tempdir().unwrap();
        let mount_point = tmp.path();
        let connections_root = tmp.path().join("connections");

        // Empty root → None (warn-logged).
        std::fs::create_dir(&connections_root).unwrap();
        assert_eq!(fusectl_abort_path_at(mount_point, &connections_root), None);

        // Compute the minor the same way the impl does, then materialize
        // the abort file the way the kernel would.
        let st_dev = nix::sys::stat::stat(mount_point).unwrap().st_dev;
        let minor = (st_dev & 0xff) | ((st_dev >> 12) & 0xff_ff_ff_00);
        let conn_dir = connections_root.join(minor.to_string());
        std::fs::create_dir(&conn_dir).unwrap();
        let abort = conn_dir.join("abort");
        std::fs::write(&abort, "").unwrap();

        assert_eq!(
            fusectl_abort_path_at(mount_point, &connections_root),
            Some(abort)
        );
    }

    #[test]
    fn fusectl_abort_path_none_on_stat_failure() {
        let nonexistent = Path::new("/nonexistent/rio-castore-test-mount-point");
        let tmp = tempfile::tempdir().unwrap();
        assert_eq!(fusectl_abort_path_at(nonexistent, tmp.path()), None);
    }
}
