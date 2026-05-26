//! Gateway-side delta-sync substituter (ADR-022 §8, P0574).
//!
//! When a `nix copy --to ssh-ng://gateway --substitute-on-destination`
//! client asks the gateway whether a closure is already valid
//! (`wopQueryValidPaths` with `substitute = true`), the gateway can —
//! if a peer rio-store is configured — materialize the missing paths
//! itself by delta-syncing them from the peer instead of making the
//! client push whole NARs over SSH. The discovery and transfer scale
//! with what *changed* between the two stores (see the `dag_sync`
//! submodule's docs for the algorithm).
//!
//! ## Capability dispatch
//!
//! There is no explicit capability-negotiation RPC between rio
//! deployments. The dispatch is per-path and probe-based:
//!
//! 1. `QueryPathInfo` on the remote — the path must exist there at
//!    all, and its `PathInfo` (nar hash, references, signatures) is
//!    what the local store will be told to expect.
//! 2. `GetNarIndex(nar_hash)` on the remote — `Unimplemented` (a
//!    pre-ADR-022 store), `NotFound` (not yet indexed), or an empty
//!    `root_digest` (a single-file/symlink NAR with no Directory DAG)
//!    all mean "this path cannot be delta-synced".
//!
//! Any path that fails the probe stays in the `wopQueryValidPaths`
//! "missing" set, and the client falls through to the existing
//! whole-NAR `wopAddToStoreNar` push — exactly what it would have done
//! if the gateway had no substituter at all. Probe failures are never
//! session errors.

use std::collections::HashMap;

use rio_common::grpc::{DEFAULT_GRPC_TIMEOUT, GRPC_STREAM_TIMEOUT};
use rio_common::limits::MAX_NAR_SIZE;
use rio_proto::types::{
    GetDirectoryRequest, GetNarIndexRequest, HasBlobsRequest, HasDirectoriesRequest, NarIndex,
    ReadBlobRequest, get_directory_request,
};
use rio_proto::validated::ValidatedPathInfo;
use rio_proto::{DirectoryServiceClient, StoreServiceClient};
use tonic::transport::Channel;
use tracing::{debug, info, instrument, warn};

use crate::handler::{SessionContext, with_jwt};

pub(crate) mod dag_sync;

use dag_sync::{CastoreView, DagSyncError, DirectoryChildren, SyncStats};

/// gRPC clients for the delta-sync peer plus the local store's
/// castore surface. Constructed once in `main.rs` from the configured
/// `substitute_store_addr` and threaded into every [`SessionContext`]
/// — clones share the underlying `tonic::transport::Channel`s.
#[derive(Clone)]
pub struct DagSyncPeer {
    /// The remote (source) store's path-level surface: `QueryPathInfo`
    /// + `GetNarIndex`.
    pub remote_store: StoreServiceClient<Channel>,
    /// The remote (source) store's castore surface: `GetDirectory` +
    /// `ReadBlob`.
    pub remote_directory: DirectoryServiceClient<Channel>,
    /// The local (destination) store's castore surface:
    /// `HasDirectories` + `HasBlobs` + `ReadBlob`. Same endpoint the
    /// session's `store_client` talks to.
    pub local_directory: DirectoryServiceClient<Channel>,
    /// Where the peer lives — for log lines only.
    pub addr: String,
}

impl DagSyncPeer {
    /// Wrap the two channels into the typed clients. `local_channel`
    /// is the gateway's existing store channel; `remote_channel` is a
    /// lazy channel to `addr` (the peer may be unreachable at gateway
    /// startup — the first sync attempt surfaces that, not boot).
    pub fn new(local_channel: Channel, remote_channel: Channel, addr: String) -> Self {
        let max = rio_common::grpc::max_message_size();
        let dir = |ch: Channel| {
            DirectoryServiceClient::new(ch)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max)
        };
        Self {
            remote_store: StoreServiceClient::new(remote_channel.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            remote_directory: dir(remote_channel),
            local_directory: dir(local_channel),
            addr,
        }
    }
}

/// A [`CastoreView`] over one `DirectoryServiceClient`. The JWT is the
/// session's tenant token — `DirectoryService` requires a tenant on
/// every call (`r[store.castore.tenant-scope]`), so an unauthenticated
/// session cannot delta-sync (the probe fails and the client falls
/// back to the whole-NAR push).
struct GrpcCastore {
    client: DirectoryServiceClient<Channel>,
    jwt: Option<String>,
}

impl GrpcCastore {
    fn rpc_err(rpc: &'static str) -> impl FnOnce(tonic::Status) -> DagSyncError {
        move |status| DagSyncError::Rpc { rpc, status }
    }
}

impl CastoreView for GrpcCastore {
    async fn has_directories(&mut self, digests: &[[u8; 32]]) -> Result<Vec<bool>, DagSyncError> {
        let req = with_jwt(
            HasDirectoriesRequest {
                digests: digests.iter().map(|d| d.to_vec()).collect(),
            },
            self.jwt.as_deref(),
        )
        .map_err(|e| DagSyncError::Rpc {
            rpc: "HasDirectories",
            status: tonic::Status::internal(e.to_string()),
        })?;
        let resp = rio_common::grpc::with_timeout_status(
            "HasDirectories",
            DEFAULT_GRPC_TIMEOUT,
            self.client.has_directories(req),
        )
        .await
        .map_err(Self::rpc_err("HasDirectories"))?;
        dag_sync::decode_bitmap("HasDirectories", &resp.into_inner().bitmap, digests.len())
    }

    async fn has_blobs(&mut self, digests: &[[u8; 32]]) -> Result<Vec<bool>, DagSyncError> {
        let req = with_jwt(
            HasBlobsRequest {
                digests: digests.iter().map(|d| d.to_vec()).collect(),
            },
            self.jwt.as_deref(),
        )
        .map_err(|e| DagSyncError::Rpc {
            rpc: "HasBlobs",
            status: tonic::Status::internal(e.to_string()),
        })?;
        let resp = rio_common::grpc::with_timeout_status(
            "HasBlobs",
            DEFAULT_GRPC_TIMEOUT,
            self.client.has_blobs(req),
        )
        .await
        .map_err(Self::rpc_err("HasBlobs"))?;
        dag_sync::decode_bitmap("HasBlobs", &resp.into_inner().bitmap, digests.len())
    }

    async fn get_directory(&mut self, digest: [u8; 32]) -> Result<DirectoryChildren, DagSyncError> {
        let req = with_jwt(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(digest.to_vec())),
                recursive: false,
                digests: Vec::new(),
            },
            self.jwt.as_deref(),
        )
        .map_err(|e| DagSyncError::Rpc {
            rpc: "GetDirectory",
            status: tonic::Status::internal(e.to_string()),
        })?;
        let mut stream = rio_common::grpc::with_timeout_status(
            "GetDirectory",
            DEFAULT_GRPC_TIMEOUT,
            self.client.get_directory(req),
        )
        .await
        .map_err(Self::rpc_err("GetDirectory"))?
        .into_inner();
        let body = tokio::time::timeout(DEFAULT_GRPC_TIMEOUT, stream.message())
            .await
            .map_err(|_| DagSyncError::Rpc {
                rpc: "GetDirectory",
                status: tonic::Status::deadline_exceeded("body stream timed out"),
            })?
            .map_err(Self::rpc_err("GetDirectory"))?
            .ok_or_else(|| DagSyncError::Rpc {
                rpc: "GetDirectory",
                status: tonic::Status::not_found("empty stream for a non-recursive get"),
            })?;
        let mut out = DirectoryChildren::default();
        for d in &body.directories {
            out.dirs.push(dag_sync::digest32(
                &d.digest,
                &format!("DirectoryEntry {:?}", d.name.escape_ascii()),
            )?);
        }
        for f in &body.files {
            out.files.push((
                dag_sync::digest32(&f.digest, &format!("FileEntry {:?}", f.name.escape_ascii()))?,
                f.size,
            ));
        }
        Ok(out)
    }

    async fn read_blob(&mut self, digest: [u8; 32]) -> Result<Vec<u8>, DagSyncError> {
        let req = with_jwt(
            ReadBlobRequest {
                file_digest: digest.to_vec(),
            },
            self.jwt.as_deref(),
        )
        .map_err(|e| DagSyncError::Rpc {
            rpc: "ReadBlob",
            status: tonic::Status::internal(e.to_string()),
        })?;
        let mut stream = rio_common::grpc::with_timeout_status(
            "ReadBlob",
            DEFAULT_GRPC_TIMEOUT,
            self.client.read_blob(req),
        )
        .await
        .map_err(Self::rpc_err("ReadBlob"))?
        .into_inner();
        let mut body = Vec::new();
        loop {
            // Per-frame idle timeout, not a whole-stream deadline — a
            // multi-hundred-MiB blob is fine as long as frames keep
            // arriving (same contract as `get_path_nar`).
            let frame = tokio::time::timeout(GRPC_STREAM_TIMEOUT, stream.message())
                .await
                .map_err(|_| DagSyncError::Rpc {
                    rpc: "ReadBlob",
                    status: tonic::Status::deadline_exceeded("blob stream stalled"),
                })?
                .map_err(Self::rpc_err("ReadBlob"))?;
            match frame {
                Some(chunk) => {
                    if body.len() as u64 + chunk.data.len() as u64 > MAX_NAR_SIZE {
                        return Err(DagSyncError::Rpc {
                            rpc: "ReadBlob",
                            status: tonic::Status::resource_exhausted(format!(
                                "blob exceeds MAX_NAR_SIZE ({MAX_NAR_SIZE})"
                            )),
                        });
                    }
                    body.extend_from_slice(&chunk.data);
                }
                None => return Ok(body),
            }
        }
    }
}

/// One path that passed the capability probe and is ready to sync.
struct SyncCandidate {
    info: ValidatedPathInfo,
    index: NarIndex,
    root_digest: [u8; 32],
}

/// Probe + delta-sync every path in `missing` from the configured
/// peer. Returns the subset that is *still* missing afterwards (probe
/// failed, sync failed, or the path genuinely isn't on the peer
/// either) — the caller reports those back to the client, which falls
/// through to the whole-NAR push.
///
/// Never returns an error: every failure mode degrades to "still
/// missing". A broken peer must not break `nix copy` against a
/// gateway that could have served the request without one.
///
/// TODO: the whole sync runs inside the client's `wopQueryValidPaths`
/// round-trip with no STDERR activity sent back, so a long delta-sync
/// (many candidates, slow peer) looks like a hung `nix copy` until it
/// finishes. Emitting periodic STDERR_NEXT progress lines (or activity
/// frames) from here would make the wait observable client-side.
#[instrument(skip_all, fields(peer = %peer.addr, missing = missing.len()))]
pub(crate) async fn try_substitute_missing(
    ctx: &mut SessionContext,
    peer: &DagSyncPeer,
    missing: &[String],
) -> Vec<String> {
    let jwt = ctx.jwt.token_owned();
    let mut still_missing: Vec<String> = Vec::new();
    let mut candidates: Vec<SyncCandidate> = Vec::new();

    // ── Capability probe, per path ──────────────────────────────────
    let mut remote_store = peer.remote_store.clone();
    for (i, path) in missing.iter().enumerate() {
        match probe_path(&mut remote_store, jwt.as_deref(), path).await {
            Ok(c) => candidates.push(c),
            Err(reason) if reason.is_peer_unreachable() => {
                // A black-holed or down peer fails each probe only
                // after its connect/RPC timeout — paying that per
                // remaining path would stall the client's copy for
                // 10-30s × N before the whole-NAR fallback even
                // starts. One transport-level failure ⇒ stop probing
                // this batch; everything unprobed stays missing.
                warn!(
                    %path, %reason,
                    remaining = missing.len() - i,
                    "dag-sync peer unreachable; skipping remaining probes for this request"
                );
                still_missing.extend(missing[i..].iter().cloned());
                break;
            }
            Err(reason) => {
                debug!(%path, %reason, "dag-sync probe declined; falling through to whole-NAR push");
                still_missing.push(path.clone());
            }
        }
    }
    if candidates.is_empty() {
        return still_missing;
    }

    // ── Discovery: one BFS over the union of all candidate roots ────
    let mut local = GrpcCastore {
        client: peer.local_directory.clone(),
        jwt: jwt.clone(),
    };
    let mut remote = GrpcCastore {
        client: peer.remote_directory.clone(),
        jwt: jwt.clone(),
    };
    let roots: Vec<[u8; 32]> = candidates.iter().map(|c| c.root_digest).collect();
    let mut stats = SyncStats::default();
    let synced = sync_candidates(
        ctx,
        &mut local,
        &mut remote,
        &roots,
        &mut stats,
        &candidates,
    )
    .await;
    emit_stats(&stats);
    match synced {
        Ok(synced_set) => {
            for c in &candidates {
                if !synced_set.contains(c.info.store_path.as_str()) {
                    still_missing.push(c.info.store_path.to_string());
                }
            }
            info!(
                synced = synced_set.len(),
                declined = still_missing.len(),
                subtrees_pruned = stats.subtrees_pruned,
                dirs_fetched = stats.dirs_fetched,
                blobs_fetched = stats.blobs_fetched,
                bytes_fetched = stats.bytes_fetched,
                bytes_saved = stats.bytes_saved,
                "directory-DAG delta-sync finished"
            );
        }
        Err(e) => {
            // The walk itself failed (not a per-path problem) — every
            // candidate stays missing.
            warn!(error = %e, "directory-DAG delta-sync aborted; falling through to whole-NAR push");
            for c in &candidates {
                still_missing.push(c.info.store_path.to_string());
            }
        }
    }
    still_missing
}

/// The fallible middle of [`try_substitute_missing`]: walk, fetch,
/// reassemble, upload. Returns the set of store paths that landed in
/// the local store.
async fn sync_candidates(
    ctx: &mut SessionContext,
    local: &mut GrpcCastore,
    remote: &mut GrpcCastore,
    roots: &[[u8; 32]],
    stats: &mut SyncStats,
    candidates: &[SyncCandidate],
) -> Result<std::collections::HashSet<String>, DagSyncError> {
    // TODO: two scaling characteristics bite when a closure is heavily
    // divergent from the local store (the opposite of the substitution
    // use case this was built for, where deltas are small):
    //   (a) every stage runs sequentially — one probe, one
    //       GetDirectory, one ReadBlob, one PutPath at a time; no
    //       request pipelining or bounded concurrency;
    //   (b) `fetch_missing_blobs` holds ALL missing blobs for the
    //       whole path set in memory at once (each blob is capped at
    //       MAX_NAR_SIZE but the aggregate is uncapped), whereas the
    //       whole-NAR fallback path buffers one NAR at a time.
    // On top of (b), `upload_one` peaks at roughly 3× one path's
    // content size (the fetched/local bodies, their clones in the
    // NarNode tree, and the serialized NAR buffer) while it runs.
    // Future fix: bounded-concurrency fetches plus per-path (or
    // streaming) blob retrieval so the working set is one path's
    // contents, like upload_one already is. A peer-down cooldown
    // shared across sessions is also not implemented — each
    // wopQueryValidPaths re-probes; the first transport failure only
    // short-circuits the rest of THAT call's probes — and probe
    // outcomes are not cached per session either, so a client that
    // re-asks about the same paths in one session pays the
    // QueryPathInfo + GetNarIndex probe again. Acceptable today
    // because a mostly-missing closure declines cheaply at the
    // probe/HasBlobs stage anyway and the client's whole-NAR push
    // takes over.
    let walk = dag_sync::walk_dag(local, remote, roots, stats).await?;
    let fetched =
        dag_sync::fetch_missing_blobs(local, remote, &walk.candidate_files, stats).await?;

    // ── Reassemble + upload, one path at a time ─────────────────────
    // Per-path failures (a blob the local store claimed to have but
    // can't serve, a hash mismatch at PutPath) skip that path only.
    let mut synced = std::collections::HashSet::new();
    for c in candidates {
        match upload_one(ctx, local, &fetched, c, stats).await {
            Ok(()) => {
                synced.insert(c.info.store_path.to_string());
            }
            Err(e) => {
                warn!(
                    path = %c.info.store_path,
                    error = %e,
                    "delta-sync reassembly failed for one path; falling through to whole-NAR push"
                );
            }
        }
    }
    Ok(synced)
}

/// Reassemble one candidate's NAR and `PutPath` it into the local
/// store. File contents come from the remote-fetched map first
/// (changed files) and the local store's `ReadBlob` second (unchanged
/// files — both those under pruned subtrees and the candidates the
/// local store already had).
async fn upload_one(
    ctx: &mut SessionContext,
    local: &mut GrpcCastore,
    fetched: &HashMap<[u8; 32], Vec<u8>>,
    c: &SyncCandidate,
    stats: &mut SyncStats,
) -> anyhow::Result<()> {
    // Resolve every distinct file digest in this NAR to its content
    // BEFORE building the tree: changed files come from the
    // remote-fetched map, everything else from the local store. The
    // pre-resolution keeps `assemble_nar`'s content lookup synchronous
    // and bounds this path's working set at one NAR's distinct
    // contents (≈ nar_size) — the same bound the whole-NAR
    // `wopAddToStoreNar` path already has.
    let needed = dag_sync::file_digests_of(c.info.store_path.as_str(), &c.index)?;
    let mut contents: HashMap<[u8; 32], Vec<u8>> = HashMap::with_capacity(needed.len());
    for (digest, size) in &needed {
        if let Some(body) = fetched.get(digest) {
            contents.insert(*digest, body.clone());
            continue;
        }
        // Not a changed file → the local store has it (either it was a
        // candidate that HasBlobs reported present, or it lives under
        // a pruned subtree whose path was indexed in one transaction
        // with its blobs). A NotFound here is a real inconsistency and
        // fails this path only.
        let body = local.read_blob(*digest).await?;
        if body.len() as u64 != *size {
            anyhow::bail!(
                "local blob {} is {} bytes, index says {size}",
                hex::encode(digest),
                body.len(),
            );
        }
        stats.bytes_saved += body.len() as u64;
        contents.insert(*digest, body);
    }
    // Tree-build + NAR serialization is pure CPU over up to
    // MAX_NAR_SIZE of owned bytes — same shape (and same cross-tenant
    // tail-latency rationale) as `handle_add_to_store`'s
    // spawn_blocking'd hash + serialize: keep it off the reactor
    // threads.
    let store_path = c.info.store_path.to_string();
    let index = c.index.clone();
    let nar = tokio::task::spawn_blocking(move || {
        dag_sync::assemble_nar(&store_path, &index, |digest, _| {
            contents.get(&digest).cloned().ok_or(DagSyncError::Rpc {
                rpc: "ReadBlob",
                status: tonic::Status::internal("blob missing from the pre-resolved content map"),
            })
        })
    })
    .await
    .map_err(|e| anyhow::anyhow!("NAR reassembly task: {e}"))??;
    if nar.len() as u64 != c.info.nar_size {
        anyhow::bail!(
            "reassembled NAR is {} bytes but the remote PathInfo says {}",
            nar.len(),
            c.info.nar_size
        );
    }
    // The store re-hashes the NAR and rejects a mismatch against
    // `info.nar_hash` — the end-to-end integrity check for the whole
    // reassembly.
    let created = crate::handler::grpc::grpc_put_path(
        &mut ctx.store_client,
        ctx.jwt.token(),
        ctx.service_signer.as_deref(),
        c.info.clone(),
        nar,
    )
    .await?;
    debug!(path = %c.info.store_path, created, "delta-synced path committed to the local store");
    Ok(())
}

/// Why a path was declined by the capability probe. Only ever logged.
#[derive(Debug, thiserror::Error)]
enum ProbeDecline {
    #[error("not on the remote store")]
    NotOnRemote,
    #[error("remote QueryPathInfo: {0}")]
    QueryPathInfo(tonic::Status),
    #[error("remote does not implement GetNarIndex (pre-ADR-022 store)")]
    NoNarIndex,
    #[error("remote has no NAR index for this path yet")]
    NotIndexed,
    #[error("NAR root is not a directory (no root_digest to walk)")]
    NoRootDigest,
    #[error("remote GetNarIndex: {0}")]
    GetNarIndex(tonic::Status),
    #[error("nar_size {0} exceeds MAX_NAR_SIZE")]
    TooLarge(u64),
    #[error("remote answered for {got:?}, not the requested path")]
    PathMismatch { got: String },
}

impl ProbeDecline {
    /// `true` when the decline is a transport-level failure (peer
    /// down, black-holed, or timing out) rather than a per-path
    /// answer. The probe loop stops on the first such failure —
    /// every further probe would pay the same connect/RPC timeout.
    /// `Unknown` is included because tonic surfaces some transport
    /// breakage as it; the cost of over-matching is only that the
    /// rest of the batch falls back to the whole-NAR push.
    fn is_peer_unreachable(&self) -> bool {
        match self {
            Self::QueryPathInfo(s) | Self::GetNarIndex(s) => matches!(
                s.code(),
                tonic::Code::Unavailable | tonic::Code::DeadlineExceeded | tonic::Code::Unknown
            ),
            _ => false,
        }
    }
}

/// Validate the remote's `QueryPathInfo` answer against the path the
/// client actually asked for. Split from [`probe_path`] so the
/// decision is unit-testable without a gRPC server.
///
/// The path-identity check is load-bearing for correctness, not just
/// hygiene: candidates and the synced set are keyed by
/// `info.store_path`, and `wopQueryValidPaths` reports anything not in
/// the still-missing set as valid. A peer that echoed a *different*
/// path would otherwise get its answer recorded under the wrong key —
/// the requested path would be reported valid without ever being
/// stored, and the client would silently skip its push.
fn check_probe_info(requested: &str, info: &ValidatedPathInfo) -> Result<(), ProbeDecline> {
    if info.store_path.as_str() != requested {
        return Err(ProbeDecline::PathMismatch {
            got: info.store_path.to_string(),
        });
    }
    if info.nar_size > MAX_NAR_SIZE {
        return Err(ProbeDecline::TooLarge(info.nar_size));
    }
    Ok(())
}

/// The per-path capability probe: `QueryPathInfo` + `GetNarIndex`
/// against the remote. See the module docs for why this is the
/// dispatch mechanism rather than an advertised capability.
async fn probe_path(
    remote_store: &mut StoreServiceClient<Channel>,
    jwt: Option<&str>,
    path: &str,
) -> Result<SyncCandidate, ProbeDecline> {
    let md = crate::handler::jwt_metadata(jwt);
    let info =
        rio_proto::client::query_path_info_opt(remote_store, path, DEFAULT_GRPC_TIMEOUT, &md)
            .await
            .map_err(ProbeDecline::QueryPathInfo)?
            .ok_or(ProbeDecline::NotOnRemote)?;
    check_probe_info(path, &info)?;
    // GetNarIndex is builder-internal on the store side: it rejects
    // requests carrying an end-user tenant JWT. The path was already
    // tenant-authorized by the QueryPathInfo above, and the index
    // carries no content — only structure.
    // no-jwt: GetNarIndex rejects x-rio-tenant-token (builder-internal RPC).
    let mut req = tonic::Request::new(GetNarIndexRequest {
        nar_hash: info.nar_hash.to_vec(),
    });
    rio_proto::interceptor::inject_current(req.metadata_mut());
    let index = match rio_common::grpc::with_timeout_status(
        "GetNarIndex",
        DEFAULT_GRPC_TIMEOUT,
        remote_store.get_nar_index(req),
    )
    .await
    {
        Ok(resp) => resp.into_inner(),
        Err(s) if s.code() == tonic::Code::Unimplemented => return Err(ProbeDecline::NoNarIndex),
        Err(s) if s.code() == tonic::Code::NotFound => return Err(ProbeDecline::NotIndexed),
        Err(s) => return Err(ProbeDecline::GetNarIndex(s)),
    };
    if index.root_digest.is_empty() {
        return Err(ProbeDecline::NoRootDigest);
    }
    let root_digest = dag_sync::digest32(&index.root_digest, "NarIndex.root_digest")
        .map_err(|_| ProbeDecline::NoRootDigest)?;
    Ok(SyncCandidate {
        info,
        index,
        root_digest,
    })
}

/// Flush the per-sync counters into the process-wide Prometheus
/// counters. Done once per sync (not per increment) so the unit tests
/// of the walk don't need a metrics recorder.
// r[impl obs.metric.gateway]
fn emit_stats(stats: &SyncStats) {
    metrics::counter!("rio_gateway_dagsync_subtrees_pruned_total").increment(stats.subtrees_pruned);
    metrics::counter!("rio_gateway_dagsync_dirs_fetched_total").increment(stats.dirs_fetched);
    metrics::counter!("rio_gateway_dagsync_blobs_fetched_total").increment(stats.blobs_fetched);
    metrics::counter!("rio_gateway_dagsync_bytes_saved_total").increment(stats.bytes_saved);
    metrics::counter!("rio_gateway_dagsync_bytes_fetched_total").increment(stats.bytes_fetched);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ValidatedPathInfo claiming to be `path` with an in-bounds
    /// nar_size.
    fn info_for(path: &str) -> ValidatedPathInfo {
        ValidatedPathInfo {
            store_path: rio_nix::store_path::StorePath::parse(path).expect("valid test path"),
            store_path_hash: Vec::new(),
            deriver: None,
            nar_hash: [0u8; 32],
            nar_size: 1024,
            references: Vec::new(),
            registration_time: 0,
            ultimate: false,
            signatures: Vec::new(),
            content_address: None,
        }
    }

    const REQUESTED: &str = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-requested";
    const OTHER: &str = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-other";

    /// A peer that answers `QueryPathInfo(requested)` with a DIFFERENT
    /// store path must be declined at probe time. Candidates and the
    /// synced set are keyed by `info.store_path`; accepting the
    /// mismatched answer would let the requested path be reported
    /// "valid" to the client without ever being stored.
    #[test]
    fn probe_declines_a_path_identity_mismatch() {
        let err = check_probe_info(REQUESTED, &info_for(OTHER))
            .expect_err("mismatched store_path must be declined");
        assert!(
            matches!(&err, ProbeDecline::PathMismatch { got } if got == OTHER),
            "got {err:?}"
        );
        // The matching answer passes.
        check_probe_info(REQUESTED, &info_for(REQUESTED)).expect("matching path is accepted");
    }

    /// Oversize NARs are declined before any castore RPC fires.
    #[test]
    fn probe_declines_an_oversize_nar() {
        let mut info = info_for(REQUESTED);
        info.nar_size = MAX_NAR_SIZE + 1;
        let err = check_probe_info(REQUESTED, &info).expect_err("oversize must be declined");
        assert!(matches!(err, ProbeDecline::TooLarge(_)), "got {err:?}");
    }

    /// Transport-level probe failures short-circuit the batch; per-path
    /// answers (NotFound, Unimplemented, mismatch, oversize) do not.
    #[test]
    fn peer_unreachable_classification() {
        for code in [
            tonic::Code::Unavailable,
            tonic::Code::DeadlineExceeded,
            tonic::Code::Unknown,
        ] {
            assert!(
                ProbeDecline::QueryPathInfo(tonic::Status::new(code, "x")).is_peer_unreachable(),
                "{code:?} on QueryPathInfo is transport-level"
            );
            assert!(
                ProbeDecline::GetNarIndex(tonic::Status::new(code, "x")).is_peer_unreachable(),
                "{code:?} on GetNarIndex is transport-level"
            );
        }
        for decline in [
            ProbeDecline::NotOnRemote,
            ProbeDecline::NoNarIndex,
            ProbeDecline::NotIndexed,
            ProbeDecline::NoRootDigest,
            ProbeDecline::TooLarge(1),
            ProbeDecline::PathMismatch {
                got: OTHER.to_string(),
            },
            ProbeDecline::QueryPathInfo(tonic::Status::permission_denied("x")),
            ProbeDecline::GetNarIndex(tonic::Status::internal("x")),
        ] {
            assert!(
                !decline.is_peer_unreachable(),
                "{decline:?} is a per-path answer, not peer-down"
            );
        }
    }
}
