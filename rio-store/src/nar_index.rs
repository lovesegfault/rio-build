//! Derived per-path NAR file/dir/symlink listing (`nar_index` table).
//!
//! `compute()` reassembles the NAR, runs `nar_ls`, and persists the
//! encoded `rio.types.NarIndex`. Non-authoritative: regenerable from
//! the stored NAR, so a missing row triggers sync-on-miss recompute
//! (capped) or [`spawn_indexer_loop`] (uncapped).

use std::io::Cursor;
use std::sync::Arc;
use std::time::Duration;

use prost::Message;
use sqlx::PgPool;
use tracing::{debug, instrument, warn};

use rio_nix::nar::{NarEntryKind, NarLsEntry, nar_ls};
use rio_nix::store_path::StorePath;
use rio_proto::types::{NarEntryKind as ProtoNarEntryKind, NarIndex, NarIndexEntry};

use crate::cas::ChunkCache;
use crate::castore::{self, DirectoryDag};
use crate::metadata::{self, ChunkManifest};

/// Poll interval; worst-case PutPath → index latency when eager
/// indexing was skipped.
const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Default for `Config::nar_index_concurrency`: max concurrent eager
/// index computations spawned by the legacy PutPath/PutPathBatch
/// paths. Sizes the `try_acquire`-only semaphore — a 5th concurrent
/// upload skips eager indexing and falls back to [`spawn_indexer_loop`]
/// instead of queueing. Also bounds the post-handler RSS of retained
/// NAR buffers to 4 × the accumulation buffer's CAPACITY — the `Bytes`
/// keeps the upload Vec's whole allocation alive, which growth-doubling
/// can leave at up to ~2× the NAR length, so the worst case is ~8 ×
/// NAR bytes, not 4 ×.
pub const DEFAULT_NAR_INDEX_CONCURRENCY: usize = 4;

/// Drain batch size. `compute()` runs serially so peak RAM is one NAR;
/// 32 just amortizes the PG round-trip.
const POLL_BATCH: i64 = 32;

/// Synchronous (RPC-path) recompute size cap. Sync-on-miss reassembles
/// the NAR before replying; oversize paths return `RESOURCE_EXHAUSTED`
/// and the `indexer_loop` (no cap) picks them up within ~5 s.
// r[impl store.index.sync-on-miss]
pub const NAR_INDEX_SYNC_MAX_BYTES: u64 = 4 * 1024 * 1024 * 1024;

/// Marker error for over-cap NARs: the RPC path downcasts this to
/// `RESOURCE_EXHAUSTED` so clients retry against the indexer loop
/// instead of treating it as a non-retryable `INTERNAL`.
#[derive(Debug, thiserror::Error)]
#[error("NAR is {total} bytes (cap {cap}); deferred to indexer_loop")]
pub struct OverSyncCap {
    pub total: u64,
    pub cap: u64,
}

/// Compute, persist, and return the encoded `NarIndex` for one path.
/// `max_bytes` is [`NAR_INDEX_SYNC_MAX_BYTES`] on the RPC path,
/// `u64::MAX` from the background loop. `budget` is the process-global
/// NAR-bytes semaphore (the one PutPath/substitute share); the RPC path
/// passes it so a burst of cache misses can't allocate `N × 4 GiB`
/// outside the budget. The serial indexer loop passes `None`.
// r[impl store.index.non-authoritative]
#[instrument(skip(pool, cache, budget))]
pub async fn compute(
    pool: &PgPool,
    cache: &Arc<ChunkCache>,
    store_path: &str,
    max_bytes: u64,
    budget: Option<&Arc<tokio::sync::Semaphore>>,
) -> anyhow::Result<Vec<u8>> {
    let hash = StorePath::parse(store_path)?.sha256_digest();
    // `claim_id` is read with the manifest contents so `set_nar_index`
    // can fence against a GC + re-upload racing the reassemble window
    // below.
    let (manifest, claim_id) = metadata::get_manifest_for_index(pool, store_path)
        .await?
        .ok_or_else(|| anyhow::anyhow!("no complete manifest for {store_path}"))?;
    let total = manifest.total_size();
    if total > max_bytes {
        return Err(OverSyncCap {
            total,
            cap: max_bytes,
        }
        .into());
    }
    // Hold permits for the lifetime of `nar` (the reassembly buffer).
    // `min(u32::MAX)` underaccounts by 1 byte for a NAR exactly at the
    // 4 GiB cap — negligible against a 32 GiB default budget.
    let _permit = match budget {
        Some(s) => Some(
            s.acquire_many(u32::try_from(total).unwrap_or(u32::MAX))
                .await?,
        ),
        None => None,
    };

    let nar = reassemble(cache, manifest, total).await?;
    // nar_ls (BLAKE3 over all file content) + castore::build is
    // multi-second CPU work for a 4 GiB NAR; run it off the async
    // worker.
    let (encoded, dag) = crate::cas::cpu_bound(|| -> anyhow::Result<_> {
        let entries = nar_ls(Cursor::new(&nar))?;
        let dag = castore::build(&entries);
        Ok((encode_entries(&entries, &dag), dag))
    })?;
    metadata::set_nar_index(pool, &hash, claim_id, &encoded, &dag).await?;
    metrics::counter!("rio_store_nar_index_compute_total").increment(1);
    Ok(encoded)
}

/// [`compute`] minus the manifest read and chunk fetch: index a NAR
/// whose bytes the caller already holds (the PutPath accumulation
/// buffer). Same `nar_ls` → `castore::build` → `metadata::set_nar_index`
/// pipeline, so the persisted rows are byte-identical to what the
/// indexer loop would have written.
///
/// `claim_id` is the caller's placeholder-ownership token (the one its
/// `complete_manifest_*` just flipped to `'complete'` with) —
/// `set_nar_index`'s `claim_id IS NOT DISTINCT FROM` fence then
/// no-ops if GC + a re-upload raced us between the commit and this
/// write, exactly like the indexer loop's read-then-fence.
#[instrument(skip(pool, nar), fields(nar_len = nar.len()))]
pub async fn compute_from_bytes(
    pool: &PgPool,
    nar: &[u8],
    store_path: &str,
    claim_id: Option<uuid::Uuid>,
) -> anyhow::Result<()> {
    let hash = StorePath::parse(store_path)?.sha256_digest();
    // Same off-worker treatment as `compute()`: nar_ls BLAKE3s every
    // file's content.
    let (encoded, dag) = crate::cas::cpu_bound(|| -> anyhow::Result<_> {
        let entries = nar_ls(Cursor::new(nar))?;
        let dag = castore::build(&entries);
        Ok((encode_entries(&entries, &dag), dag))
    })?;
    metadata::set_nar_index(pool, &hash, claim_id, &encoded, &dag).await?;
    metrics::counter!("rio_store_nar_index_compute_total").increment(1);
    Ok(())
}

// r[impl store.index.putpath-eager]
/// Best-effort eager index after a successful legacy `PutPath` /
/// `PutPathBatch` commit, while the NAR bytes are still in RAM.
///
/// `try_acquire` — never waits. A free permit → spawn
/// [`compute_from_bytes`] on the shared buffer; at capacity → skip and
/// let [`spawn_indexer_loop`] pick the path up within `POLL_INTERVAL`
/// (≤5 s; it re-fetches the NAR from storage). Either way the upload's
/// response latency is unaffected. The permit rides inside the spawned
/// task so it releases when the computation finishes, not when the
/// handler returns.
///
/// Failures are logged + counted, never propagated: the index is
/// non-authoritative and `nar_indexed` stays FALSE on error, so the
/// indexer loop retries from storage.
///
/// Observability: `rio_store_nar_index_eager_total{outcome=
/// spawned|skipped|error}`. `spawned - error` = paths indexed without
/// touching the work queue; sustained `skipped` means
/// `nar_index_concurrency` is undersized for the upload rate.
///
/// `nar` is the upload's refcounted accumulation buffer; the spawned
/// task holds a `Bytes` clone (refcount bump, no copy) until the
/// compute finishes.
pub fn maybe_spawn_eager(
    pool: &PgPool,
    sem: &Arc<tokio::sync::Semaphore>,
    store_path: &str,
    claim_id: uuid::Uuid,
    nar: &bytes::Bytes,
) {
    let Ok(permit) = Arc::clone(sem).try_acquire_owned() else {
        metrics::counter!("rio_store_nar_index_eager_total", "outcome" => "skipped").increment(1);
        debug!(%store_path, "eager nar_index skipped (at concurrency cap); deferring to indexer_loop");
        return;
    };
    metrics::counter!("rio_store_nar_index_eager_total", "outcome" => "spawned").increment(1);
    let pool = pool.clone();
    let nar = nar.clone();
    let store_path = store_path.to_owned();
    rio_common::task::spawn_monitored("nar-index-eager", async move {
        let _permit = permit;
        let timer = std::time::Instant::now();
        match compute_from_bytes(&pool, &nar, &store_path, Some(claim_id)).await {
            Ok(()) => {
                // `source="eager"`: in-RAM bytes, so this population is
                // hash+persist only (no chunk fetch / reassemble).
                metrics::histogram!("rio_store_nar_index_compute_seconds", "source" => "eager")
                    .record(timer.elapsed().as_secs_f64());
            }
            Err(e) => {
                metrics::counter!("rio_store_nar_index_eager_total", "outcome" => "error")
                    .increment(1);
                warn!(%store_path, error = %e, "eager nar_index compute failed; indexer_loop will retry");
            }
        }
    });
}

/// Reassemble a NAR from its manifest. Chunks fetch through the
/// shared moka cache (BLAKE3 verify per-chunk; cross-warm with GetPath).
///
/// `pub(crate)`: the binary-cache compat reconciler reuses this exact
/// reassembly (same cache, same per-chunk verification) to rebuild the
/// NARs it publishes as compat objects.
pub(crate) async fn reassemble(
    cache: &Arc<ChunkCache>,
    manifest: ChunkManifest,
    total: u64,
) -> anyhow::Result<Vec<u8>> {
    // unwrap_or(0): a >usize::MAX NAR can't exist on a 64-bit
    // host; on 32-bit (unsupported) degrade to organic growth
    // rather than panic on a usize::MAX allocation.
    let mut buf = Vec::with_capacity(usize::try_from(total).unwrap_or(0));
    for (h, _size) in manifest.0 {
        buf.extend_from_slice(&cache.get_verified(&h).await?);
    }
    Ok(buf)
}

/// Background work-queue drain: polls `manifests_nar_index_pending_idx`,
/// indexes each path, sleeps when empty.
// r[impl store.index.putpath-bg-warm]
pub fn spawn_indexer_loop(
    pool: PgPool,
    cache: Arc<ChunkCache>,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    rio_common::task::spawn_periodic("nar-indexer", POLL_INTERVAL, shutdown, move || {
        let pool = pool.clone();
        let cache = cache.clone();
        async move {
            let pending = match metadata::list_nar_index_pending(&pool, POLL_BATCH).await {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "nar_index pending poll failed");
                    return;
                }
            };
            if pending.is_empty() {
                return;
            }
            debug!(count = pending.len(), "indexing pending paths");
            for (hash, path) in pending {
                let timer = std::time::Instant::now();
                match compute(&pool, &cache, &path, u64::MAX, None).await {
                    Ok(_) => {
                        // `source="loop"`: full pipeline — chunk fetch +
                        // reassemble + nar_ls + persist.
                        metrics::histogram!("rio_store_nar_index_compute_seconds", "source" => "loop")
                            .record(timer.elapsed().as_secs_f64());
                    }
                    Err(e) => {
                        warn!(store_path = %path, error = %e, "nar_index compute failed");
                        // Rotate to the back of the `ORDER BY updated_at`
                        // queue so 32 permanently-failing paths can't
                        // starve everything behind them.
                        if let Err(e) = metadata::bump_nar_index_retry(&pool, &hash).await {
                            warn!(store_path = %path, error = %e, "nar_index retry bump failed");
                        }
                    }
                }
            }
        }
    })
}

/// Encode a `nar_ls` entry list as the `nar_index.entries` BYTEA, with
/// per-entry `dir_digest` from `dag`.
pub fn encode_entries(entries: &[NarLsEntry], dag: &DirectoryDag) -> Vec<u8> {
    NarIndex {
        entries: entries
            .iter()
            .zip(&dag.dir_digests)
            .map(|(e, d)| to_proto_entry(e, d))
            .collect(),
        root_digest: dag.root_digest.clone(),
    }
    .encode_to_vec()
}

/// Decode the `nar_index.entries` BYTEA back to a proto `NarIndex`.
pub fn decode_entries(bytes: &[u8]) -> anyhow::Result<NarIndex> {
    Ok(NarIndex::decode(bytes)?)
}

/// Distinct, sorted digest sets — the per-path contribution to
/// `directories.refcount` is one per unique digest.
pub struct IndexDigests {
    pub dirs: Vec<[u8; 32]>,
    pub files: Vec<[u8; 32]>,
}

/// Castore digest sets from `nar_index.entries`; the GC sweep reads
/// this before the CASCADE removes the row.
// r[impl store.castore.gc]
pub fn digests_from_index(bytes: &[u8]) -> anyhow::Result<IndexDigests> {
    let idx = decode_entries(bytes)?;
    let mut dirs: Vec<[u8; 32]> = Vec::new();
    let mut files: Vec<[u8; 32]> = Vec::new();
    for e in &idx.entries {
        if e.dir_digest.len() == 32 {
            dirs.push(e.dir_digest.as_slice().try_into().expect("len checked"));
        }
        if e.file_digest.len() == 32 {
            files.push(e.file_digest.as_slice().try_into().expect("len checked"));
        }
    }
    dirs.sort_unstable();
    dirs.dedup();
    files.sort_unstable();
    files.dedup();
    Ok(IndexDigests { dirs, files })
}

/// `NarLsEntry` → wire `NarIndexEntry`. The in-memory `[0; 32]`
/// sentinel maps to the proto's empty-bytes sentinel.
fn to_proto_entry(e: &NarLsEntry, dir_digest: &[u8; 32]) -> NarIndexEntry {
    NarIndexEntry {
        path: e.path.clone(),
        kind: match e.kind {
            NarEntryKind::Regular => ProtoNarEntryKind::Regular,
            NarEntryKind::Directory => ProtoNarEntryKind::Directory,
            NarEntryKind::Symlink => ProtoNarEntryKind::Symlink,
        }
        .into(),
        size: e.size,
        executable: e.executable,
        nar_offset: e.nar_offset,
        target: e.target.clone(),
        file_digest: if e.kind == NarEntryKind::Regular {
            e.file_digest.to_vec()
        } else {
            Vec::new()
        },
        // r[impl store.index.dir-digest]
        dir_digest: if e.kind == NarEntryKind::Directory {
            dir_digest.to_vec()
        } else {
            Vec::new()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::nar::NarEntryKind;

    /// Proto contract: dirs/symlinks carry an EMPTY `file_digest` (not
    /// 32 zeros), files/symlinks carry an EMPTY `dir_digest`.
    #[test]
    fn proto_entry_empty_digest_for_nonregular() {
        let dir = NarLsEntry {
            path: b"sub".to_vec(),
            kind: NarEntryKind::Directory,
            size: 0,
            executable: false,
            nar_offset: 0,
            target: Vec::new(),
            file_digest: [0u8; 32],
        };
        let p = to_proto_entry(&dir, &[5u8; 32]);
        assert!(p.file_digest.is_empty());
        assert_eq!(p.dir_digest, vec![5u8; 32]);
        assert_eq!(p.kind, ProtoNarEntryKind::Directory as i32);

        let reg = NarLsEntry {
            path: b"f".to_vec(),
            kind: NarEntryKind::Regular,
            size: 4,
            executable: true,
            nar_offset: 96,
            target: Vec::new(),
            file_digest: [7u8; 32],
        };
        let p = to_proto_entry(&reg, &[5u8; 32]);
        assert_eq!(p.file_digest, vec![7u8; 32]);
        assert!(p.dir_digest.is_empty());
        assert_eq!(p.kind, ProtoNarEntryKind::Regular as i32);
        assert_eq!(p.nar_offset, 96);
    }

    /// Encode/decode round-trip preserves order, content, and the
    /// per-entry `dir_digest` + `root_digest`.
    #[test]
    fn encode_decode_roundtrip() {
        let entries = vec![
            NarLsEntry {
                path: b"".to_vec(),
                kind: NarEntryKind::Directory,
                size: 0,
                executable: false,
                nar_offset: 0,
                target: Vec::new(),
                file_digest: [0u8; 32],
            },
            NarLsEntry {
                path: b"a".to_vec(),
                kind: NarEntryKind::Regular,
                size: 3,
                executable: false,
                nar_offset: 100,
                target: Vec::new(),
                file_digest: [1u8; 32],
            },
            NarLsEntry {
                path: b"b".to_vec(),
                kind: NarEntryKind::Symlink,
                size: 0,
                executable: false,
                nar_offset: 0,
                target: b"a".to_vec(),
                file_digest: [0u8; 32],
            },
        ];
        let dag = castore::build(&entries);
        let bytes = encode_entries(&entries, &dag);
        let decoded = decode_entries(&bytes).unwrap();
        assert_eq!(decoded.entries.len(), 3);
        assert_eq!(decoded.entries[0].path, b"");
        assert_eq!(decoded.entries[0].dir_digest, dag.dir_digests[0].to_vec());
        assert_eq!(decoded.entries[1].file_digest, vec![1u8; 32]);
        assert_eq!(decoded.entries[1].nar_offset, 100);
        assert_eq!(decoded.entries[2].target, b"a");
        assert!(decoded.entries[0].file_digest.is_empty());
        assert!(decoded.entries[1].dir_digest.is_empty());
        assert!(decoded.entries[2].file_digest.is_empty());
        assert!(decoded.entries[2].dir_digest.is_empty());
        assert_eq!(decoded.root_digest, dag.root_digest);

        // r[verify store.castore.gc]
        let d = digests_from_index(&bytes).unwrap();
        assert_eq!(d.dirs, vec![dag.dir_digests[0]]);
        assert_eq!(d.files, vec![[1u8; 32]]);
    }
}
