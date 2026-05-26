//! S3 Express eviction sweeper — size-bounded MRU for the per-AZ cache
//! tier (design overview §9, ADR-023, P0585).
//!
//! Each per-AZ S3 Express directory bucket is a bounded read-through
//! cache over authoritative S3 standard ([`super::TieredChunkBackend`]),
//! not a mirror. Directory buckets only support *age-based* lifecycle
//! expiration, so this application-level sweep is what actually enforces
//! the byte budget: every `sweep_interval_secs` the elected sweeper lists
//! the bucket, publishes `rio_store_express_bytes{az_id}`, and — when the
//! total exceeds `target_bytes × evict_high_watermark` — deletes
//! oldest-by-`LastModified` objects until back under
//! `target_bytes × evict_low_watermark`, counting deletions into
//! `rio_store_express_evicted_total{az_id}`. An evicted chunk is never
//! data loss: S3 standard still holds it, and the next cold miss refills
//! Express.
//!
//! ## `LastModified` is write time, not read time
//!
//! S3 exposes no read-time signal, so evicting by `LastModified` is
//! FIFO-by-fill rather than true MRU. Because Express is filled *only* by
//! read-through (`put` never touches it), `LastModified` ≈ the last time
//! any replica in this AZ cold-missed the chunk — the closest cheap
//! proxy, and chunks that stay moka-hot (old `LastModified`, never
//! re-fetched) are exactly the ones that are correct to evict. The design
//! accepts this approximation; the cache is disposable.
//!
//! ## Coordination and degradation
//!
//! One sweeper per AZ is sufficient; concurrent sweepers waste LIST/
//! DELETE requests but stay correct (deletes are idempotent). Inside
//! Kubernetes (`KUBERNETES_SERVICE_HOST` present) each replica runs a
//! [`rio_lease`] loop on the per-AZ Lease
//! `rio-store-express-sweep-{az_id}` and only sweeps while it holds it.
//! If the kube client cannot be built or the Lease cannot be reached
//! (no service-account token mounted, apiserver egress denied, RBAC
//! missing — see `infra/helm/rio-build/templates/store-rbac.yaml`), the
//! lease loop logs a warning and exits; that replica then **never
//! sweeps** and the bucket's age-based lifecycle expiration
//! (`infra/eks/s3-express.tf`) is the only growth bound. Outside
//! Kubernetes (VM tests, dev, standalone single-replica) there is no
//! lease infrastructure and the replica sweeps unconditionally.
//!
//! ## Memory envelope
//!
//! At the 8 TiB design point the bucket holds tens of millions of
//! objects, so the sweep never materializes the full listing. Pass 1
//! only sums sizes (no per-object allocation); only when the high
//! watermark is exceeded does pass 2 re-list, keeping just the eviction
//! set (oldest objects whose sizes cover `total − low_watermark`) in a
//! size-bounded heap.

use std::collections::BinaryHeap;
use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use aws_sdk_s3::Client;
use aws_sdk_s3::types::{Delete, ObjectIdentifier};
use tracing::{debug, info, warn};

use crate::config::ExpressConfig;

/// Hard cap on keys per `DeleteObjects` request (S3 API limit).
const DELETE_BATCH_MAX: usize = 1000;

/// One object selected for eviction. Derived `Ord` is lexicographic over
/// the field order: `last_modified` first (the eviction criterion), then
/// size/key purely as deterministic tie-breakers.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct EvictionCandidate {
    /// `LastModified` as epoch seconds (missing → 0, i.e. oldest).
    last_modified_epoch_secs: i64,
    /// Object size in bytes.
    size: u64,
    /// Full object key, passed verbatim to `DeleteObjects`.
    key: String,
}

/// Streaming "oldest set covering N bytes" selector.
///
/// Push every listed object; the internal max-heap (newest on top) keeps
/// only as many of the oldest objects as needed for their sizes to cover
/// `bytes_to_evict`, so memory is bounded by the eviction set — not the
/// full listing.
struct EvictionSelector {
    bytes_to_evict: u64,
    /// Max-heap by `last_modified` — the newest candidate is on top and
    /// is the first to be discarded when the rest already cover the
    /// budget.
    heap: BinaryHeap<EvictionCandidate>,
    heap_bytes: u64,
}

impl EvictionSelector {
    fn new(bytes_to_evict: u64) -> Self {
        Self {
            bytes_to_evict,
            heap: BinaryHeap::new(),
            heap_bytes: 0,
        }
    }

    fn push(&mut self, candidate: EvictionCandidate) {
        self.heap_bytes += candidate.size;
        self.heap.push(candidate);
        // Drop the newest candidates while the remainder still covers the
        // budget. Keeps the heap minimal (within one object of the
        // budget) at every step.
        while let Some(newest) = self.heap.peek() {
            if self.heap_bytes - newest.size >= self.bytes_to_evict {
                self.heap_bytes -= newest.size;
                self.heap.pop();
            } else {
                break;
            }
        }
    }

    /// Selected candidates, oldest first.
    fn into_sorted(self) -> Vec<EvictionCandidate> {
        let mut v = self.heap.into_vec();
        v.sort();
        v
    }
}

/// Outcome of one sweep pass — logged by the loop and asserted by tests.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SweepStats {
    /// Sum of object sizes from the listing pass.
    pub total_bytes: u64,
    /// Object count from the listing pass.
    pub total_objects: u64,
    /// Objects successfully deleted this sweep.
    pub evicted_objects: u64,
    /// Bytes reclaimed this sweep (sizes of successfully deleted keys).
    pub evicted_bytes: u64,
}

/// `target_bytes × factor`, in bytes. f64 has 53 mantissa bits — exact
/// for any realistic bucket budget (8 TiB ≈ 2^43); sub-byte rounding is
/// irrelevant at this scale.
fn watermark_bytes(target_bytes: u64, factor: f64) -> u64 {
    (target_bytes as f64 * factor) as u64
}

/// Parse the AZ id out of an S3 Express directory-bucket name
/// (`{anything}--{az-id}--x-s3`, e.g.
/// `rio-prod-chunk-cache--use2-az1--x-s3` → `use2-az1`).
///
/// Returns `"unknown"` when the name doesn't follow the directory-bucket
/// convention (e.g. the Garage stand-in bucket in `vm-store-tiered`) so
/// the metric label and lease name stay well-formed, just not AZ-scoped.
/// Deriving the AZ from the bucket suffix avoids a second config knob
/// until P0554's per-pod AZ selection makes the AZ explicit.
pub(crate) fn az_id_from_bucket(bucket: &str) -> String {
    bucket
        .strip_suffix("--x-s3")
        .and_then(|prefix| prefix.rsplit_once("--"))
        .map(|(_, az)| az.to_owned())
        .filter(|az| !az.is_empty())
        .unwrap_or_else(|| "unknown".to_owned())
}

/// Pass 1: paginated `ListObjectsV2`, summing sizes only. No per-object
/// allocation — this is the steady-state path (most sweeps end here).
async fn list_totals(client: &Client, bucket: &str) -> anyhow::Result<(u64, u64)> {
    let mut total_bytes = 0u64;
    let mut total_objects = 0u64;
    let mut continuation: Option<String> = None;
    loop {
        let page = list_page(client, bucket, continuation.as_deref()).await?;
        for obj in page.contents() {
            total_objects += 1;
            total_bytes += obj.size().unwrap_or(0).max(0) as u64;
        }
        match next_continuation(&page)? {
            Some(token) => continuation = Some(token),
            None => break,
        }
    }
    Ok((total_bytes, total_objects))
}

/// Pass 2: paginated `ListObjectsV2` feeding the bounded
/// [`EvictionSelector`]. Only runs when pass 1 found the bucket over the
/// high watermark.
async fn list_eviction_candidates(
    client: &Client,
    bucket: &str,
    bytes_to_evict: u64,
) -> anyhow::Result<Vec<EvictionCandidate>> {
    let mut selector = EvictionSelector::new(bytes_to_evict);
    let mut continuation: Option<String> = None;
    loop {
        let page = list_page(client, bucket, continuation.as_deref()).await?;
        for obj in page.contents() {
            let Some(key) = obj.key() else { continue };
            selector.push(EvictionCandidate {
                // Missing LastModified (never observed on real S3) sorts
                // as oldest — eviction of an unidentifiable-age object is
                // harmless (S3 standard is authoritative).
                last_modified_epoch_secs: obj.last_modified().map(|t| t.secs()).unwrap_or(0),
                size: obj.size().unwrap_or(0).max(0) as u64,
                key: key.to_owned(),
            });
        }
        match next_continuation(&page)? {
            Some(token) => continuation = Some(token),
            None => break,
        }
    }
    Ok(selector.into_sorted())
}

async fn list_page(
    client: &Client,
    bucket: &str,
    continuation: Option<&str>,
) -> anyhow::Result<aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output> {
    metrics::counter!("rio_store_s3_requests_total", "operation" => "list_objects_v2").increment(1);
    let mut req = client.list_objects_v2().bucket(bucket);
    if let Some(token) = continuation {
        req = req.continuation_token(token);
    }
    req.send().await.map_err(|e| {
        super::classify_s3_error(e, format!("S3 ListObjectsV2 failed for s3://{bucket}"))
    })
}

/// Continuation token for the next page, or `None` on the last page.
/// A truncated page without a token is a malformed response — error out
/// rather than silently re-listing page 1 forever.
fn next_continuation(
    page: &aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output,
) -> anyhow::Result<Option<String>> {
    if page.is_truncated() == Some(true) {
        let token = page.next_continuation_token().map(str::to_owned);
        anyhow::ensure!(
            token.is_some(),
            "S3 ListObjectsV2 returned a truncated page without a continuation token"
        );
        Ok(token)
    } else {
        Ok(None)
    }
}

/// One `DeleteObjects` call for `batch` (≤ [`DELETE_BATCH_MAX`] keys).
/// Returns (deleted object count, deleted bytes); per-key failures are
/// logged and simply not counted — they stay in the bucket and the next
/// sweep retries them.
async fn delete_batch(
    client: &Client,
    bucket: &str,
    batch: &[EvictionCandidate],
) -> anyhow::Result<(u64, u64)> {
    let mut objects = Vec::with_capacity(batch.len());
    for candidate in batch {
        objects.push(
            ObjectIdentifier::builder()
                .key(&candidate.key)
                .build()
                .map_err(|e| anyhow::anyhow!("building ObjectIdentifier: {e}"))?,
        );
    }
    let delete = Delete::builder()
        .set_objects(Some(objects))
        .build()
        .map_err(|e| anyhow::anyhow!("building Delete: {e}"))?;

    metrics::counter!("rio_store_s3_requests_total", "operation" => "delete_objects").increment(1);
    let out = client
        .delete_objects()
        .bucket(bucket)
        .delete(delete)
        .send()
        .await
        .map_err(|e| {
            super::classify_s3_error(e, format!("S3 DeleteObjects failed for s3://{bucket}"))
        })?;

    for err in out.errors() {
        warn!(
            key = err.key().unwrap_or("<unknown>"),
            code = err.code().unwrap_or("<unknown>"),
            message = err.message().unwrap_or(""),
            "express eviction: DeleteObjects entry failed; will retry next sweep"
        );
    }

    let mut deleted_objects = 0u64;
    let mut deleted_bytes = 0u64;
    for deleted in out.deleted() {
        deleted_objects += 1;
        if let Some(key) = deleted.key()
            && let Some(candidate) = batch.iter().find(|c| c.key == key)
        {
            deleted_bytes += candidate.size;
        }
    }
    Ok((deleted_objects, deleted_bytes))
}

/// One full sweep: list, publish the size gauge, and — only when over the
/// high watermark — evict oldest objects until under the low watermark.
// r[impl infra.express.bounded-eviction]
// r[impl obs.metric.express-eviction]
pub async fn sweep_once(
    client: &Client,
    bucket: &str,
    az_id: &str,
    cfg: &ExpressConfig,
) -> anyhow::Result<SweepStats> {
    let (total_bytes, total_objects) = list_totals(client, bucket).await?;
    metrics::gauge!("rio_store_express_bytes", "az_id" => az_id.to_owned()).set(total_bytes as f64);

    let high = watermark_bytes(cfg.target_bytes, cfg.evict_high_watermark);
    let low = watermark_bytes(cfg.target_bytes, cfg.evict_low_watermark);
    let mut stats = SweepStats {
        total_bytes,
        total_objects,
        ..SweepStats::default()
    };

    if total_bytes <= high {
        debug!(
            bucket,
            az_id,
            total_bytes,
            total_objects,
            high_watermark_bytes = high,
            "express bucket under high watermark; nothing to evict"
        );
        return Ok(stats);
    }

    let bytes_to_evict = total_bytes.saturating_sub(low);
    info!(
        bucket,
        az_id,
        total_bytes,
        total_objects,
        high_watermark_bytes = high,
        low_watermark_bytes = low,
        bytes_to_evict,
        "express bucket over high watermark; evicting oldest objects"
    );

    let candidates = list_eviction_candidates(client, bucket, bytes_to_evict).await?;
    for batch in candidates.chunks(DELETE_BATCH_MAX) {
        let (deleted_objects, deleted_bytes) = delete_batch(client, bucket, batch).await?;
        stats.evicted_objects += deleted_objects;
        stats.evicted_bytes += deleted_bytes;
        metrics::counter!("rio_store_express_evicted_total", "az_id" => az_id.to_owned())
            .increment(deleted_objects);
    }

    info!(
        bucket,
        az_id,
        evicted_objects = stats.evicted_objects,
        evicted_bytes = stats.evicted_bytes,
        remaining_bytes = total_bytes.saturating_sub(stats.evicted_bytes),
        "express eviction sweep complete"
    );
    Ok(stats)
}

/// No-op [`rio_lease::LeaseHooks`]: `run_lease_loop` already logs
/// acquire/lose transitions, and the sweeper has no actor to notify — the
/// periodic body simply checks [`rio_lease::LeaderState::is_leader`] each
/// tick.
#[derive(Clone, Copy, Default)]
struct SweepLeaseHooks;

impl rio_lease::LeaseHooks for SweepLeaseHooks {
    fn on_acquire(&self) {}
    fn on_lose(&self) {}
}

/// Decide which replica sweeps (see the module docs):
///
/// - In Kubernetes (`KUBERNETES_SERVICE_HOST` set by the kubelet): the
///   per-AZ Lease `rio-store-express-sweep-{az_id}` elects one sweeper;
///   this replica sweeps only while it holds it. If the kube client/Lease
///   is unreachable the lease loop exits with a warning and this replica
///   never sweeps.
/// - Outside Kubernetes: sole replica by assumption — always leader.
fn sweep_leader_state(az_id: &str, shutdown: rio_common::signal::Token) -> rio_lease::LeaderState {
    let generation = Arc::new(AtomicU64::new(1));
    if std::env::var_os("KUBERNETES_SERVICE_HOST").is_none() {
        info!("not running in Kubernetes; express sweeper runs without lease election");
        return rio_lease::LeaderState::always_leader(generation);
    }

    let lease_name = format!("rio-store-express-sweep-{az_id}");
    // `from_parts(Some(_), None)` always returns Some; the namespace falls
    // back to the in-cluster service-account namespace mount.
    let Some(lease_cfg) = rio_lease::LeaseConfig::from_parts(Some(lease_name), None) else {
        return rio_lease::LeaderState::always_leader(generation);
    };
    info!(
        lease = %lease_cfg.lease_name,
        namespace = %lease_cfg.namespace,
        holder = %lease_cfg.holder_id,
        "express sweep lease election enabled"
    );
    let state = rio_lease::LeaderState::pending(generation);
    // The sweeper has no recovery step — mark it complete so LeaderState
    // reads stay coherent (mirrors rio-controller's nodeclaim_pool use).
    state.set_recovery_complete();
    rio_common::task::spawn_monitored(
        "express-sweep-lease",
        rio_lease::run_lease_loop(lease_cfg, state.clone(), SweepLeaseHooks, shutdown),
    );
    state
}

/// Spawn the periodic express eviction sweep for `bucket` (and, inside
/// Kubernetes, the per-AZ lease loop that elects which replica actually
/// sweeps). Called from `main.rs` only when the tiered chunk backend has
/// an Express bucket configured. `sweep_interval_secs == 0` disables the
/// sweeper entirely.
pub fn spawn_express_sweep(
    cfg: ExpressConfig,
    client: Client,
    bucket: String,
    shutdown: rio_common::signal::Token,
) {
    if cfg.sweep_interval_secs == 0 {
        info!(%bucket, "express eviction sweeper disabled (sweep_interval_secs = 0)");
        return;
    }
    let az_id = az_id_from_bucket(&bucket);
    let leader = sweep_leader_state(&az_id, shutdown.clone());
    let interval = Duration::from_secs(cfg.sweep_interval_secs);
    info!(
        %bucket,
        az_id,
        target_bytes = cfg.target_bytes,
        evict_high_watermark = cfg.evict_high_watermark,
        evict_low_watermark = cfg.evict_low_watermark,
        sweep_interval_secs = cfg.sweep_interval_secs,
        "express eviction sweeper starting"
    );
    rio_common::task::spawn_periodic("express-sweep", interval, shutdown, move || {
        let client = client.clone();
        let bucket = bucket.clone();
        let az_id = az_id.clone();
        let cfg = cfg.clone();
        let leader = leader.clone();
        async move {
            if !leader.is_leader() {
                debug!(%bucket, az_id, "standby for the express-sweep lease; skipping tick");
                return;
            }
            match sweep_once(&client, &bucket, &az_id, &cfg).await {
                Ok(stats) => debug!(
                    %bucket,
                    az_id,
                    total_bytes = stats.total_bytes,
                    evicted_objects = stats.evicted_objects,
                    "express sweep tick complete"
                ),
                Err(e) => warn!(
                    error = %format_args!("{e:#}"),
                    %bucket,
                    az_id,
                    "express eviction sweep failed; retrying next tick"
                ),
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::operation::delete_objects::DeleteObjectsOutput;
    use aws_sdk_s3::operation::list_objects_v2::ListObjectsV2Output;
    use aws_sdk_s3::primitives::DateTime;
    use aws_sdk_s3::types::{DeletedObject, Error as S3Error, Object};
    use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};
    use rio_test_support::metrics::CountingRecorder;

    fn obj(key: &str, epoch_secs: i64, size: i64) -> Object {
        Object::builder()
            .key(key)
            .size(size)
            .last_modified(DateTime::from_secs(epoch_secs))
            .build()
    }

    fn candidate(key: &str, epoch_secs: i64, size: u64) -> EvictionCandidate {
        EvictionCandidate {
            last_modified_epoch_secs: epoch_secs,
            size,
            key: key.to_owned(),
        }
    }

    /// ListObjectsV2 page rule. `next` = continuation token to hand out
    /// (None = final page).
    fn list_rule(objects: Vec<Object>, next: Option<&'static str>) -> Rule {
        mock!(Client::list_objects_v2).then_output(move || {
            let mut b = ListObjectsV2Output::builder().set_contents(Some(objects.clone()));
            if let Some(token) = next {
                b = b.is_truncated(true).next_continuation_token(token);
            } else {
                b = b.is_truncated(false);
            }
            b.build()
        })
    }

    fn test_cfg(target_bytes: u64) -> ExpressConfig {
        ExpressConfig {
            target_bytes,
            ..ExpressConfig::default()
        }
    }

    // ------------------------------------------------------------------
    // Pure selection logic
    // ------------------------------------------------------------------

    /// Oldest objects are selected until their sizes cover the budget;
    /// newer objects never displace older ones.
    #[test]
    fn selector_keeps_oldest_covering_budget() {
        let mut sel = EvictionSelector::new(500);
        // Push newest-first to prove ordering doesn't depend on input order.
        sel.push(candidate("d", 400, 300));
        sel.push(candidate("c", 300, 300));
        sel.push(candidate("b", 200, 300));
        sel.push(candidate("a", 100, 300));
        let picked: Vec<_> = sel.into_sorted().into_iter().map(|c| c.key).collect();
        // 500 bytes to evict → two oldest (a, b) cover 600 ≥ 500.
        assert_eq!(picked, vec!["a", "b"]);
    }

    /// Budget larger than everything listed → keep everything.
    #[test]
    fn selector_keeps_all_when_budget_exceeds_listing() {
        let mut sel = EvictionSelector::new(10_000);
        sel.push(candidate("a", 100, 300));
        sel.push(candidate("b", 200, 300));
        let picked: Vec<_> = sel.into_sorted().into_iter().map(|c| c.key).collect();
        assert_eq!(picked, vec!["a", "b"]);
    }

    /// Zero budget → nothing selected.
    #[test]
    fn selector_zero_budget_selects_nothing() {
        let mut sel = EvictionSelector::new(0);
        sel.push(candidate("a", 100, 300));
        assert!(sel.into_sorted().is_empty());
    }

    #[test]
    fn watermark_bytes_math() {
        // 8 TiB × 1.10 / 0.90 — the defaults.
        assert_eq!(watermark_bytes(8_796_093_022_208, 1.10), 9_675_702_324_428);
        assert_eq!(watermark_bytes(8_796_093_022_208, 0.90), 7_916_483_719_987);
        assert_eq!(watermark_bytes(1000, 1.10), 1100);
        assert_eq!(watermark_bytes(1000, 0.90), 900);
    }

    #[test]
    fn az_id_parsed_from_directory_bucket_name() {
        assert_eq!(
            az_id_from_bucket("rio-prod-chunk-cache--use2-az1--x-s3"),
            "use2-az1"
        );
        assert_eq!(
            az_id_from_bucket("rio-chunk-cache--apne1-az4--x-s3"),
            "apne1-az4"
        );
        // Not a directory-bucket name (e.g. the Garage stand-in in
        // vm-store-tiered) → "unknown", never a panic or empty label.
        assert_eq!(az_id_from_bucket("rio-express-standin"), "unknown");
        assert_eq!(az_id_from_bucket("weird--x-s3"), "unknown");
        assert_eq!(az_id_from_bucket(""), "unknown");
    }

    // ------------------------------------------------------------------
    // sweep_once end-to-end (mocked S3 + CountingRecorder)
    // ------------------------------------------------------------------

    /// Over the high watermark: the sweep deletes the oldest objects
    /// (and only those) until the projected total is under the low
    /// watermark, sets the byte gauge to the listed total and counts the
    /// deletions — the bounded-MRU contract plus its observability.
    // r[verify infra.express.bounded-eviction]
    // r[verify obs.metric.express-eviction]
    #[tokio::test]
    async fn sweep_evicts_oldest_until_low_watermark() -> anyhow::Result<()> {
        // target 1000 → high 1100, low 900. Four 300-byte objects =
        // 1200 > 1100; bytes_to_evict = 1200 - 900 = 300 → exactly the
        // oldest object ("old-a") goes.
        let pages = || {
            vec![
                vec![obj("old-a", 100, 300), obj("mid-b", 200, 300)],
                vec![obj("mid-c", 300, 300), obj("new-d", 400, 300)],
            ]
        };
        // Pass 1 (totals) pages, then pass 2 (candidates) pages.
        let p1 = pages();
        let p2 = pages();
        let pass1_a = list_rule(p1[0].clone(), Some("tok1"));
        let pass1_b = list_rule(p1[1].clone(), None);
        let pass2_a = list_rule(p2[0].clone(), Some("tok2"));
        let pass2_b = list_rule(p2[1].clone(), None);
        // DeleteObjects must be called with exactly ["old-a"]; any other
        // key set fails the request matcher and the sweep errors out.
        let delete = mock!(Client::delete_objects)
            .match_requests(|req| {
                let keys: Vec<_> = req
                    .delete()
                    .map(|d| d.objects().iter().map(|o| o.key()).collect())
                    .unwrap_or_default();
                keys == vec!["old-a"]
            })
            .then_output(|| {
                DeleteObjectsOutput::builder()
                    .deleted(DeletedObject::builder().key("old-a").build())
                    .build()
            });
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&pass1_a, &pass1_b, &pass2_a, &pass2_b, &delete]
        );

        let recorder = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let stats = sweep_once(
            &client,
            "cache--use2-az1--x-s3",
            "use2-az1",
            &test_cfg(1000),
        )
        .await?;

        assert_eq!(stats.total_bytes, 1200);
        assert_eq!(stats.total_objects, 4);
        assert_eq!(stats.evicted_objects, 1);
        assert_eq!(stats.evicted_bytes, 300);
        assert_eq!(delete.num_calls(), 1, "one DeleteObjects batch");

        // Byte gauge reflects the listed total; evicted counter moved by
        // exactly the deleted count — both labeled with the AZ id.
        assert_eq!(
            recorder.gauge_value("rio_store_express_bytes{az_id=use2-az1}"),
            Some(1200.0)
        );
        assert_eq!(
            recorder.get("rio_store_express_evicted_total{az_id=use2-az1}"),
            1
        );
        Ok(())
    }

    /// Under the high watermark: gauge still published, but no second
    /// listing pass and no DeleteObjects call (the mock has no rules for
    /// them — any attempt would error the sweep).
    // r[verify infra.express.bounded-eviction]
    // r[verify obs.metric.express-eviction]
    #[tokio::test]
    async fn sweep_under_high_watermark_deletes_nothing() -> anyhow::Result<()> {
        // target 1000 → high 1100; total 600 ≤ 1100.
        let only_pass = list_rule(vec![obj("a", 100, 300), obj("b", 200, 300)], None);
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&only_pass]);

        let recorder = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let stats = sweep_once(
            &client,
            "cache--use2-az1--x-s3",
            "use2-az1",
            &test_cfg(1000),
        )
        .await?;

        assert_eq!(stats.total_bytes, 600);
        assert_eq!(stats.evicted_objects, 0);
        assert_eq!(
            recorder.gauge_value("rio_store_express_bytes{az_id=use2-az1}"),
            Some(600.0)
        );
        assert_eq!(
            recorder.get("rio_store_express_evicted_total{az_id=use2-az1}"),
            0,
            "evicted counter must not move when nothing was deleted"
        );
        Ok(())
    }

    /// Exactly at the high watermark is NOT over it — eviction triggers
    /// strictly above (`total > target × high`).
    #[tokio::test]
    async fn sweep_at_exact_high_watermark_deletes_nothing() -> anyhow::Result<()> {
        // target 1000 → high 1100; total exactly 1100.
        let only_pass = list_rule(vec![obj("a", 100, 600), obj("b", 200, 500)], None);
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&only_pass]);
        let stats = sweep_once(
            &client,
            "cache--use2-az1--x-s3",
            "use2-az1",
            &test_cfg(1000),
        )
        .await?;
        assert_eq!(stats.total_bytes, 1100);
        assert_eq!(stats.evicted_objects, 0);
        Ok(())
    }

    /// Per-key DeleteObjects failures are not counted as evictions; the
    /// failed key stays for the next sweep.
    // r[verify obs.metric.express-eviction]
    #[tokio::test]
    async fn sweep_counts_only_successfully_deleted_keys() -> anyhow::Result<()> {
        // target 100 → high 110, low 90. Two 300-byte objects = 600 >
        // 110; bytes_to_evict = 600 - 90 = 510 → both selected.
        let pages = || vec![obj("old-a", 100, 300), obj("new-b", 200, 300)];
        let pass1 = list_rule(pages(), None);
        let pass2 = list_rule(pages(), None);
        // One key deletes, the other errors (e.g. transient 500 on that key).
        let delete = mock!(Client::delete_objects).then_output(|| {
            DeleteObjectsOutput::builder()
                .deleted(DeletedObject::builder().key("old-a").build())
                .errors(
                    S3Error::builder()
                        .key("new-b")
                        .code("InternalError")
                        .message("we encountered an internal error")
                        .build(),
                )
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&pass1, &pass2, &delete]);

        let recorder = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        let stats =
            sweep_once(&client, "cache--use2-az1--x-s3", "use2-az1", &test_cfg(100)).await?;
        assert_eq!(stats.evicted_objects, 1, "only the Deleted entry counts");
        assert_eq!(stats.evicted_bytes, 300);
        assert_eq!(
            recorder.get("rio_store_express_evicted_total{az_id=use2-az1}"),
            1
        );
        Ok(())
    }

    /// `spawn_express_sweep` with `sweep_interval_secs = 0` is a no-op
    /// (nothing spawned, nothing panics) — the documented disable knob.
    #[tokio::test]
    async fn spawn_disabled_by_zero_interval() {
        let cfg = aws_sdk_s3::Config::builder()
            .behavior_version(aws_sdk_s3::config::BehaviorVersion::latest())
            .build();
        let client = Client::from_conf(cfg);
        spawn_express_sweep(
            ExpressConfig {
                sweep_interval_secs: 0,
                ..ExpressConfig::default()
            },
            client,
            "rio-chunk-cache--use2-az1--x-s3".into(),
            rio_common::signal::Token::new(),
        );
    }
}
