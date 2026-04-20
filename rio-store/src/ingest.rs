//! Write-ahead NAR ingest core, shared by PutPath/PutPathBatch (gRPC)
//! and `Substituter` (upstream binary-cache fetch).
//!
//! Both flows walk the same state machine:
//!
//! 1. [`claim_placeholder`] — idempotency check, insert
//!    `status='uploading'` row, hot-path stale-reclaim
//! 2. caller acquires NAR bytes (gRPC stream / HTTP download)
//! 3. [`persist_nar`] — branch on size: inline (`manifests.inline_blob`)
//!    or chunked (`cas::put_chunked`)
//! 4. on any error after step 1: [`abort_placeholder`]
//!
//! Before this module existed, `Substituter::ingest` open-coded steps
//! 1/3/4 and had already drifted from `grpc/put_path/common.rs` once
//! (substitution lacked the `chunk_dedup_ratio` gauge). Factoring here
//! keeps the write-ahead invariants in one place; gRPC/substitute keep
//! their transport-specific bits (HMAC, sig_mode, error mapping) in
//! thin wrappers.

use std::sync::Arc;

use bytes::Bytes;
use sqlx::PgPool;
use tracing::{debug, warn};

use rio_proto::validated::ValidatedPathInfo;

use crate::backend::ChunkBackend;
use crate::cas;
use crate::metadata::{self, MetadataError};
use crate::substitute::SUBSTITUTE_STALE_THRESHOLD;

/// Result of [`claim_placeholder`].
pub enum PlaceholderClaim {
    /// Path is already `status='complete'`. Caller returns
    /// `created=false` (gRPC) or fetches the existing row (substitute)
    /// without writing anything.
    AlreadyComplete,
    /// We inserted (or stale-reclaimed-then-inserted) the
    /// `status='uploading'` placeholder. Caller now OWNS it and MUST
    /// [`abort_placeholder`] on any error path.
    Owned,
    /// Another uploader holds a live (heartbeating) placeholder. Caller
    /// returns `aborted` so the client retries.
    Concurrent,
}

/// Per-caller observability hooks. The two ingest entry points emit
/// different metric names for the same events (stale-reclaim on the
/// PutPath hot path vs the substitution hot path are tracked
/// separately because they indicate different upstream-health
/// problems).
#[derive(Clone, Copy)]
pub struct IngestHooks {
    /// `metrics::counter!` name incremented when a stale `'uploading'`
    /// placeholder is reaped on the hot path. e.g.
    /// `rio_store_putpath_stale_reclaimed_total`.
    pub stale_reclaimed_metric: &'static str,
    /// Prefix for `warn!`/`debug!` log lines (e.g. `"PutPath"`,
    /// `"substitute"`).
    pub ctx_label: &'static str,
}

// r[impl store.put.idempotent]
// r[impl store.put.stale-reclaim]
/// Idempotency check + `status='uploading'` placeholder insert +
/// hot-path stale-reclaim. The shared step-1 of the write-ahead flow.
///
/// Flow:
/// 1. `check_manifest_complete` → [`PlaceholderClaim::AlreadyComplete`]
/// 2. `insert_manifest_uploading` → if inserted: [`PlaceholderClaim::Owned`]
/// 3. ON CONFLICT no-op: try `reap_one` with the stale threshold
///    (I-207 — a fetcher that died mid-upload leaves a placeholder
///    the orphan scanner won't reap for 15min, but the scheduler
///    retries within seconds). If reap succeeded, re-insert.
/// 4. Still not inserted → [`PlaceholderClaim::Concurrent`] (live
///    uploader's heartbeat keeps `updated_at` fresh, so reap_one's
///    threshold check protected it).
///
/// Per-caller metrics (`exists` / `concurrent_upload` on
/// `rio_store_put_path_total`) are NOT emitted here — that's a
/// PutPath-specific counter. Only the stale-reclaim counter (whose
/// name the caller supplies) is emitted.
pub async fn claim_placeholder(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    store_path_hash: &[u8],
    store_path: &str,
    refs: &[String],
    hooks: IngestHooks,
) -> Result<PlaceholderClaim, MetadataError> {
    if metadata::check_manifest_complete(pool, store_path_hash).await? {
        return Ok(PlaceholderClaim::AlreadyComplete);
    }

    // STRUCTURAL: insert_manifest_uploading takes references and writes
    // them into the placeholder narinfo. Mark's CTE walks them from
    // commit → the closure is GC-protected without holding a session
    // lock for the full upload.
    let mut inserted =
        metadata::insert_manifest_uploading(pool, store_path_hash, store_path, refs).await?;

    if !inserted {
        // r[impl store.substitute.stale-reclaim]
        // I-040: chunk-aware reap (reads manifest_data.chunk_list and
        // decrements refcounts) — the inline-only delete leaks chunk
        // refcounts when the stale placeholder is from an interrupted
        // `cas::put_chunked`.
        let threshold = SUBSTITUTE_STALE_THRESHOLD.as_secs() as i64;
        match crate::gc::orphan::reap_one(pool, store_path_hash, Some(threshold), chunk_backend)
            .await
        {
            Ok(true) => {
                warn!(
                    %store_path,
                    threshold = ?SUBSTITUTE_STALE_THRESHOLD,
                    "{}: stale 'uploading' placeholder — reclaimed", hooks.ctx_label,
                );
                metrics::counter!(hooks.stale_reclaimed_metric).increment(1);
                // Propagate (?) — after reap_one Ok(true) the
                // placeholder is gone; collapsing Err into the
                // Concurrent path here would silently swallow a DB
                // failure with no log (asymmetric with line 101).
                inserted =
                    metadata::insert_manifest_uploading(pool, store_path_hash, store_path, refs)
                        .await?;
            }
            Ok(false) => {} // not stale → live concurrent uploader
            Err(e) => warn!(error = %e,
                "{}: stale-reclaim failed (proceeding to concurrent-abort)", hooks.ctx_label),
        }
    }

    if !inserted {
        return Ok(PlaceholderClaim::Concurrent);
    }
    Ok(PlaceholderClaim::Owned)
}

/// How [`persist_nar`] failed. The caller maps this to its own error
/// domain (`tonic::Status` for gRPC, `SubstituteError` for
/// substitution) and decides whether to [`abort_placeholder`]: the
/// chunked path already rolled back internally.
#[derive(Debug)]
pub enum PersistError {
    /// `cas::put_chunked` failed. Its internal rollback
    /// (`delete_manifest_chunked_uploading`) already ran; the
    /// placeholder is GONE (best-effort). Caller's `abort_placeholder`
    /// is a harmless no-op but not required.
    Chunked(anyhow::Error),
    /// `complete_manifest_inline` failed. Caller still OWNS the
    /// placeholder and MUST `abort_placeholder`.
    Inline(MetadataError),
}

/// Persist a validated, hash-verified NAR for ONE output. Branches on
/// `nar_data.len()` vs [`cas::INLINE_THRESHOLD`]: inline goes to
/// `manifests.inline_blob` in one tx; chunked goes through
/// [`cas::put_chunked`] (FastCDC + S3 + refcounts, own write-ahead +
/// rollback).
///
/// Caller must hold a [`PlaceholderClaim::Owned`] for
/// `info.store_path_hash`. Emits `rio_store_chunk_dedup_ratio` on the
/// chunked branch.
pub async fn persist_nar(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    info: &ValidatedPathInfo,
    nar_data: Vec<u8>,
    chunk_upload_max_concurrent: usize,
    hooks: IngestHooks,
) -> Result<(), PersistError> {
    if let Some(backend) = cas::should_chunk(chunk_backend, nar_data.len()) {
        let stats = cas::put_chunked(pool, backend, info, &nar_data, chunk_upload_max_concurrent)
            .await
            .map_err(PersistError::Chunked)?;
        debug!(
            store_path = %info.store_path.as_str(),
            total_chunks = stats.total_chunks,
            deduped = stats.deduped_chunks,
            ratio = stats.dedup_ratio(),
            "{}: chunked upload completed", hooks.ctx_label,
        );
        metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
    } else {
        metadata::complete_manifest_inline(pool, info, Bytes::from(nar_data))
            .await
            .map_err(PersistError::Inline)?;
        debug!(store_path = %info.store_path.as_str(), "{}: inline upload completed", hooks.ctx_label);
    }
    Ok(())
}

/// Heartbeat cadence for [`PlaceholderGuard`]. Matches
/// `cas::HEARTBEAT_TIME_INTERVAL` (the chunk-upload heartbeat) and is
/// ≪ `SUBSTITUTE_STALE_THRESHOLD` (300s), so a live owner survives ≥9
/// missed heartbeats before stale-reclaim takes it.
const PLACEHOLDER_HEARTBEAT_INTERVAL: std::time::Duration = std::time::Duration::from_secs(30);

/// RAII owner of an `'uploading'` placeholder: heartbeats while held,
/// reaps on drop. See [`spawn_placeholder_guard`].
// r[impl store.put.drop-cleanup+2]
pub(crate) struct PlaceholderGuard {
    heartbeat: tokio::task::JoinHandle<()>,
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    store_path_hash: Vec<u8>,
    defused: bool,
}

impl PlaceholderGuard {
    /// Stop heartbeating and skip the drop-path reap. Call after the
    /// placeholder has been flipped to `'complete'` (or explicitly
    /// `abort_upload`ed — the reap would be a no-op, but the spawn is
    /// wasted).
    pub(crate) fn defuse(mut self) {
        self.defused = true;
    }
}

impl Drop for PlaceholderGuard {
    fn drop(&mut self) {
        self.heartbeat.abort();
        if self.defused {
            return;
        }
        let pool = self.pool.clone();
        let chunk_backend = self.chunk_backend.take();
        let store_path_hash = std::mem::take(&mut self.store_path_hash);
        tokio::spawn(async move {
            if let Err(e) =
                crate::gc::orphan::reap_one(&pool, &store_path_hash, None, chunk_backend.as_ref())
                    .await
            {
                warn!(
                    store_path_hash = %hex::encode(&store_path_hash),
                    error = %e,
                    "drop-path placeholder cleanup failed; orphan scanner will reclaim",
                );
            }
        });
    }
}

// r[impl store.put.drop-cleanup+2]
/// Drop-safety + liveness for a [`PlaceholderClaim::Owned`] placeholder.
/// Returns a [`PlaceholderGuard`] that:
///
/// - **heartbeats** `manifests.updated_at` every 30s while held, so
///   `r[store.put.stale-reclaim]`'s `reap_one(SUBSTITUTE_STALE_
///   THRESHOLD)` never reaps a live owner during a long ingest
///   (6 GB/50 Mbps ≈ 16 min);
/// - **on Drop** (owning future dropped — tonic aborts on client
///   RST_STREAM; a `try_substitute` caller times out — without an
///   explicit [`abort_placeholder`] or `'complete'` flip), spawns
///   `reap_one`. `reap_one` filters `status='uploading'` so firing after
///   an explicit abort/complete is a harmless no-op.
///
/// Call [`PlaceholderGuard::defuse`] on success.
///
/// Shared by `PutPath` and `Substituter::try_upstream`; both run inline
/// in a request handler future and so share the same drop hazard.
pub fn spawn_placeholder_guard(
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    store_path_hash: Vec<u8>,
) -> PlaceholderGuard {
    let heartbeat = {
        let pool = pool.clone();
        let hash = store_path_hash.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PLACEHOLDER_HEARTBEAT_INTERVAL);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            tick.tick().await; // first tick fires immediately; skip it
            loop {
                tick.tick().await;
                cas::heartbeat_uploading(&pool, &hash).await;
            }
        })
    };
    PlaceholderGuard {
        heartbeat,
        pool,
        chunk_backend,
        store_path_hash,
        defused: false,
    }
}

/// Best-effort placeholder cleanup after a failed ingest. Chunk-aware
/// (reads `manifest_data.chunk_list` and decrements refcounts).
/// `threshold=None`: this is OUR placeholder, no stale check needed.
/// Safe to call even if `cas::put_chunked`'s internal rollback already
/// deleted it (no-op).
pub async fn abort_placeholder(
    pool: &PgPool,
    chunk_backend: Option<&Arc<dyn ChunkBackend>>,
    store_path_hash: &[u8],
) {
    if let Err(e) = crate::gc::orphan::reap_one(pool, store_path_hash, None, chunk_backend).await {
        warn!(
            store_path_hash = %hex::encode(store_path_hash),
            error = %e,
            "abort_placeholder: cleanup failed; orphan scanner will reclaim",
        );
    }
}
