//! Binary-cache compat reconciler (P0582): backfills the object pairs
//! the inline writer never produced.
//!
//! The inline compat write only covers the buffered upload RPCs. Every
//! other way a path becomes `'complete'` — `PutPathChunked` (builder
//! uploads), upstream substitution, paths ingested while compat was
//! OFF, and inline writes that failed or crashed mid-way — leaves
//! `narinfo.compat_file_hash IS NULL`. This loop drains that set:
//! list a bounded batch (oldest first), reassemble each NAR from the
//! chunk store (the same per-chunk-verified reassembly GetPath and the
//! NAR indexer use), publish the pair through the same
//! [`CompatWriter`], and let `write()` record `compat_file_hash`.
//!
//! Best-effort by construction: a per-path failure is logged and
//! counted, the path stays pending (NULL), and the loop moves on — it
//! never crashes the process and never blocks an upload. Multiple
//! replicas may run the loop concurrently; a double-publish is
//! idempotent (same content, same keys) and the column update is
//! last-writer-wins on an identical value, so no leader gate is
//! needed — duplicate work during a backlog drain is the only cost.
//!
//! Memory bound: one NAR at a time per replica (the batch is processed
//! serially), so peak extra RSS is one decompressed NAR plus its
//! compressed copy — the same envelope as one buffered upload.

use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use sqlx::PgPool;
use tracing::{debug, info, warn};

use crate::cas::ChunkCache;
use crate::metadata;
use crate::nar_index;

use super::writer::CompatWriter;

/// Paths fetched per batch. Bounds one batch's PG round-trip and the
/// granularity at which the shutdown signal is honored; the loop keeps
/// pulling batches within a tick until the backlog is empty.
const RECONCILE_BATCH: i64 = 64;

/// Outcome of one [`run_once`] batch.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ReconcileStats {
    /// Paths whose compat pair was published and recorded this batch.
    pub published: usize,
    /// Paths that failed (reassembly, integrity, upload, bookkeeping)
    /// and remain pending for a later pass.
    pub failed: usize,
    /// Backlog size *before* this batch ran (what the gauge shows).
    pub backlog: u64,
}

impl ReconcileStats {
    /// True when the batch found nothing to do — the caller sleeps for
    /// the configured interval instead of immediately re-polling.
    pub fn idle(&self) -> bool {
        self.published == 0 && self.failed == 0
    }
}

// r[impl store.compat.reconcile]
/// Process one batch of pending paths: refresh the backlog gauge, list
/// up to [`RECONCILE_BATCH`] pending paths, and publish each one.
/// Failures are per-path; this function only errors if the *listing*
/// itself fails (PG unreachable), in which case the caller just waits
/// for the next tick.
pub async fn run_once(
    pool: &PgPool,
    cache: &Arc<ChunkCache>,
    writer: &CompatWriter,
) -> anyhow::Result<ReconcileStats> {
    let backlog = metadata::count_compat_pending(pool).await?;
    // r[impl obs.metric.compat]
    metrics::gauge!("rio_store_compat_backlog").set(backlog as f64);

    let pending = metadata::list_compat_pending(pool, RECONCILE_BATCH).await?;
    let mut stats = ReconcileStats {
        backlog: u64::try_from(backlog).unwrap_or(0),
        ..ReconcileStats::default()
    };
    if pending.is_empty() {
        return Ok(stats);
    }
    debug!(count = pending.len(), backlog, "compat reconciler batch");

    for store_path in pending {
        match reconcile_one(pool, cache, writer, &store_path).await {
            Ok(file_size) => {
                stats.published += 1;
                metrics::counter!("rio_store_compat_reconcile_total", "result" => "ok")
                    .increment(1);
                debug!(%store_path, file_size, "compat pair backfilled");
            }
            Err(e) => {
                stats.failed += 1;
                metrics::counter!("rio_store_compat_reconcile_total", "result" => "error")
                    .increment(1);
                warn!(%store_path, error = %e, "compat backfill failed; path stays pending");
            }
        }
    }
    Ok(stats)
}

/// Publish the compat pair for one pending path: read its committed
/// metadata + chunk manifest, reassemble the NAR through the shared
/// chunk cache, re-verify the whole-NAR SHA-256 against the narinfo
/// row (the per-chunk BLAKE3 verify catches bitrot, this catches
/// manifest/narinfo drift — the same gate GetPath applies before
/// streaming), then hand the bytes to the same [`CompatWriter::write`]
/// the inline path uses.
async fn reconcile_one(
    pool: &PgPool,
    cache: &Arc<ChunkCache>,
    writer: &CompatWriter,
    store_path: &str,
) -> anyhow::Result<u64> {
    let info = metadata::query_path_info(pool, store_path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("path no longer complete (GC'd since listing)"))?;
    let manifest = metadata::get_manifest(pool, store_path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no chunk manifest for complete path"))?;

    let total = manifest.total_size();
    let nar = nar_index::reassemble(cache, manifest, total).await?;

    // Whole-NAR integrity gate. Publishing a NAR whose hash doesn't
    // match the narinfo we are about to write would hand stock Nix an
    // object it rejects after download — fail here (counted + logged)
    // so the row stays pending and the corruption is investigable.
    let computed: [u8; 32] = crate::cas::cpu_bound(|| Sha256::digest(&nar).into());
    if computed != info.nar_hash {
        anyhow::bail!(
            "reassembled NAR hash mismatch: narinfo says {}, chunks reassemble to {}",
            hex::encode(info.nar_hash),
            hex::encode(computed),
        );
    }

    let file_size = writer.write(&info, &Bytes::from(nar)).await?;
    metrics::counter!("rio_store_compat_write_bytes_total").increment(file_size);
    Ok(file_size)
}

/// Spawn the reconciler loop: drain the backlog batch-by-batch, then
/// idle-poll every `interval`. Spawned from `main.rs` only when
/// `binary_cache_compat.enabled` and `reconcile_interval_secs > 0`;
/// same panic/shutdown disposition as the GC and NAR-indexer loops
/// (panics are logged by `spawn_monitored`, shutdown is honored
/// between batches).
pub fn spawn_reconciler_loop(
    pool: PgPool,
    cache: Arc<ChunkCache>,
    writer: Arc<CompatWriter>,
    interval: std::time::Duration,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    info!(
        interval_secs = interval.as_secs(),
        "compat reconciler enabled"
    );
    rio_common::task::spawn_periodic("compat-reconciler", interval, shutdown.clone(), move || {
        let pool = pool.clone();
        let cache = Arc::clone(&cache);
        let writer = Arc::clone(&writer);
        let shutdown = shutdown.clone();
        async move {
            // Drain continuously within one tick: a fresh-enabled
            // deployment may have the entire store as backlog, and one
            // batch per 30 s would never catch up. The shutdown check
            // between batches keeps SIGTERM responsive mid-drain.
            loop {
                if shutdown.is_cancelled() {
                    return;
                }
                match run_once(&pool, &cache, &writer).await {
                    Ok(stats) if stats.idle() => return,
                    Ok(_) => {}
                    Err(e) => {
                        warn!(error = %e, "compat reconciler batch listing failed; retrying next tick");
                        return;
                    }
                }
            }
        }
    })
}
