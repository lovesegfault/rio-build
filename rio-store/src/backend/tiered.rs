//! Two-tier chunk backend: per-AZ S3 Express read-through cache over
//! authoritative S3 standard. ADR-022 §9, ADR-023.
//!
//! `put` goes to S3 standard only. Express is filled solely by `get`'s
//! write-through — a freshly uploaded chunk is moka-hot in the writing
//! replica anyway, and warm-on-put would add ~7 ms PUT latency per
//! chunk for a benefit moka already provides. `get` reads Express
//! first; on miss reads S3 standard and writes through to Express.
//! `local = None` degrades to a pass-through wrapper around `remote` —
//! a replica scheduled in an AZ without Express still functions, just
//! without the cache tier.
//!
//! GC tracks `PutPath` refcounts, which is a put-path concern, so
//! `key_for` and `delete_by_key` operate on the remote tier only. A
//! deleted chunk's Express copy is unreachable (no surviving manifest
//! references it) and ages out via the bucket lifecycle policy
//! (`expiration.days = 30`, `infra/eks/s3-express.tf`) and the P0585
//! sweeper. Serving a stale-but-valid copy is harmless — content
//! addressing means the bytes are correct or BLAKE3 verification fails
//! at the cache layer.
//!
//! Both tiers are `S3ChunkBackend`. The SDK routes by the `--x-s3`
//! bucket-name suffix and handles `s3express:CreateSession`, so there
//! is no new backend type and no new put-idempotence reasoning. The
//! cache tier must not add a failure mode the pass-through shape
//! doesn't have: Express read errors fall through to S3 standard,
//! write-through failures are swallowed, and the local tier's `Client`
//! gets a 2-attempt retry budget (vs the remote's default 10, see
//! `init_chunk_backend` in `config.rs`) so a throttling Express bucket
//! fails fast instead of eating the full backoff budget on every read.

use std::time::Instant;

use bytes::Bytes;
use tracing::{debug, warn};

use super::{ChunkBackend, S3ChunkBackend};

/// Record one served chunk read into the per-tier latency histogram.
///
/// The hit/miss counters prove WHERE reads are served from but not how
/// fast; this is the direct measurement of the Express-vs-S3-standard
/// TTFB gap the tier exists to buy. Each arm times only its own tier's
/// GET (the `standard` arm excludes the preceding Express probe and the
/// write-through), so the two series compare raw per-tier read latency.
fn record_get_duration(tier: &'static str, start: Instant) {
    metrics::histogram!("rio_store_tiered_get_duration_seconds", "tier" => tier)
        .record(start.elapsed().as_secs_f64());
}

pub struct TieredChunkBackend {
    /// Per-AZ S3 Express directory bucket. `None` = pass-through.
    local: Option<S3ChunkBackend>,
    /// Regional S3 standard bucket. Authoritative.
    remote: S3ChunkBackend,
}

impl TieredChunkBackend {
    pub fn new(local: Option<S3ChunkBackend>, remote: S3ChunkBackend) -> Self {
        Self { local, remote }
    }

    /// Remote (S3-standard) GET with per-tier latency recording.
    /// Records only when bytes are served — a `NotFound` latency would
    /// pollute the distribution with non-serves, and the miss/error
    /// counters already cover those outcomes.
    async fn remote_get_recorded(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
        let start = Instant::now();
        let got = self.remote.get(hash).await?;
        if got.is_some() {
            record_get_duration("standard", start);
        }
        Ok(got)
    }
}

#[async_trait::async_trait]
impl ChunkBackend for TieredChunkBackend {
    // r[impl store.backend.tiered-put-remote-first]
    async fn put(&self, hash: &[u8; 32], data: Bytes) -> anyhow::Result<()> {
        self.remote.put(hash, data).await
    }

    // r[impl store.backend.tiered-get-fallback]
    async fn get(&self, hash: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
        let Some(local) = &self.local else {
            return self.remote_get_recorded(hash).await;
        };

        let local_start = Instant::now();
        match local.get(hash).await {
            Ok(Some(data)) => {
                metrics::counter!("rio_store_tiered_local_hits_total").increment(1);
                record_get_duration("express", local_start);
                return Ok(Some(data));
            }
            Ok(None) => {
                metrics::counter!("rio_store_tiered_local_misses_total").increment(1);
            }
            Err(e) => {
                // Swallowed — never propagates, so this warn is the
                // only trace. {:#} renders the full anyhow chain (the
                // SDK-level cause is what tells timeout from 503 from
                // expired creds); plain {} would show only the
                // outermost context.
                metrics::counter!("rio_store_tiered_local_errors_total").increment(1);
                warn!(
                    error = %format_args!("{e:#}"),
                    hash = %hex::encode(hash),
                    "Express read failed; falling back to S3 standard"
                );
            }
        }

        let Some(data) = self.remote_get_recorded(hash).await? else {
            return Ok(None);
        };

        // Fire-and-wait, not fire-and-forget. tokio::spawn would buy
        // ~7 ms (Express PUT p50) on a path that just paid an
        // S3-standard RTT, at the cost of a Bytes clone outliving the
        // request span and an unbounded backlog when Express is slow.
        if let Err(e) = local.put(hash, data.clone()).await {
            metrics::counter!("rio_store_tiered_writethrough_errors_total").increment(1);
            warn!(
                error = %format_args!("{e:#}"),
                hash = %hex::encode(hash),
                "Express write-through failed; chunk served from S3 standard"
            );
        } else {
            debug!(hash = %hex::encode(hash), "Express write-through filled");
        }

        Ok(Some(data))
    }

    /// Authoritative tier only. Express warmth is a cache question, not
    /// an existence question, and the live caller (VerifyChunks) wants
    /// the latter.
    async fn exists_batch(&self, hashes: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
        self.remote.exists_batch(hashes).await
    }

    fn key_for(&self, hash: &[u8; 32]) -> String {
        self.remote.key_for(hash)
    }

    /// Remote only — the Express copy ages out via bucket lifecycle and
    /// the P0585 sweeper (see module doc).
    async fn delete_by_key(&self, key: &str) -> anyhow::Result<()> {
        self.remote.delete_by_key(key).await
    }

    /// Remote only, same rationale as [`ChunkBackend::delete_by_key`]
    /// — and delegating (rather than inheriting the default per-key
    /// loop) keeps the remote tier's batched `DeleteObjects` in play
    /// for the prod tiered configuration.
    async fn delete_by_keys(
        &self,
        keys: &[String],
    ) -> anyhow::Result<Vec<super::BatchDeleteFailure>> {
        self.remote.delete_by_keys(keys).await
    }

    // Blobs are the stock-Nix binary-cache surface (narinfo / NAR /
    // nix-cache-info). Stock Nix reads them straight from the regional
    // bucket — Express never sees them — so all three ops are
    // remote-only.

    async fn put_blob(&self, key: &str, data: Bytes) -> anyhow::Result<()> {
        self.remote.put_blob(key, data).await
    }

    async fn get_blob(&self, key: &str) -> anyhow::Result<Option<Bytes>> {
        self.remote.get_blob(key).await
    }

    async fn delete_blob(&self, key: &str) -> anyhow::Result<()> {
        self.remote.delete_blob(key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::Client;
    use aws_sdk_s3::error::ErrorMetadata;
    use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_sdk_s3::primitives::ByteStream;
    use aws_sdk_s3::types::error::NoSuchKey;
    use aws_smithy_mocks::{Rule, RuleMode, mock, mock_client};

    const HASH: [u8; 32] = [0xAB; 32];

    /// Mock-backed `S3ChunkBackend` that fires `rules` in sequence.
    ///
    /// Don't use `--x-s3` test bucket names: the SDK detects the
    /// directory-bucket suffix and inserts an `s3express:CreateSession`
    /// hop the mock orchestrator can't answer (panics in
    /// `OrchestratorError::into_sdk_error`). That detection IS the SDK
    /// feature this backend relies on in prod, but the mock layer
    /// doesn't model the extra round-trip.
    fn s3(bucket: &str, rules: &[&Rule]) -> S3ChunkBackend {
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, rules);
        S3ChunkBackend::new(client, bucket.into(), "p".into())
    }

    /// Mock with no rules — any request panics. Used to assert a tier
    /// is never touched.
    fn must_not_touch(bucket: &str) -> S3ChunkBackend {
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[]);
        S3ChunkBackend::new(client, bucket.into(), "p".into())
    }

    fn body(b: &'static [u8]) -> impl Fn() -> GetObjectOutput {
        move || {
            GetObjectOutput::builder()
                .body(ByteStream::from_static(b))
                .build()
        }
    }

    fn no_such_key() -> GetObjectError {
        GetObjectError::NoSuchKey(NoSuchKey::builder().build())
    }

    const GET_HISTOGRAM: &str = "rio_store_tiered_get_duration_seconds";

    /// Rendered recorder key for one tier arm of the latency histogram.
    fn tier_key(tier: &str) -> String {
        format!("{GET_HISTOGRAM}{{tier={tier}}}")
    }

    /// Assert the latency histogram was recorded under exactly `tier`
    /// and NOT the other arm. Guards both failure modes of the metric:
    /// emission never wired (nothing recorded) and the wrong label arm
    /// (hit counted as standard or vice versa).
    fn assert_recorded_tier(rec: &rio_test_support::metrics::CountingRecorder, tier: &str) {
        assert!(
            rec.histogram_key_touched(&tier_key(tier)),
            "expected {} recorded, saw {:?}",
            tier_key(tier),
            rec.histogram_keys()
        );
        let other = if tier == "express" {
            "standard"
        } else {
            "express"
        };
        assert!(
            !rec.histogram_key_touched(&tier_key(other)),
            "latency must not be recorded under tier={other}, saw {:?}",
            rec.histogram_keys()
        );
    }

    // r[verify store.backend.tiered-put-remote-first]
    #[tokio::test]
    async fn put_remote_only() -> anyhow::Result<()> {
        let put_rule = mock!(Client::put_object).then_output(|| PutObjectOutput::builder().build());
        let backend =
            TieredChunkBackend::new(Some(must_not_touch("express")), s3("std", &[&put_rule]));
        backend.put(&HASH, Bytes::from_static(b"chunk")).await?;
        assert_eq!(put_rule.num_calls(), 1);
        Ok(())
    }

    // r[verify store.backend.tiered-get-fallback]
    #[tokio::test]
    async fn get_local_hit_short_circuits() -> anyhow::Result<()> {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let hit = mock!(Client::get_object).then_output(body(b"hot"));
        let backend = TieredChunkBackend::new(Some(s3("express", &[&hit])), must_not_touch("std"));
        assert_eq!(backend.get(&HASH).await?.unwrap().as_ref(), b"hot");
        assert_recorded_tier(&recorder, "express");
        Ok(())
    }

    // r[verify store.backend.tiered-get-fallback]
    #[tokio::test]
    async fn get_local_miss_falls_back_and_fills() -> anyhow::Result<()> {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let local_miss = mock!(Client::get_object).then_error(no_such_key);
        let local_fill =
            mock!(Client::put_object).then_output(|| PutObjectOutput::builder().build());
        let remote_get = mock!(Client::get_object).then_output(body(b"cold"));
        let backend = TieredChunkBackend::new(
            Some(s3("express", &[&local_miss, &local_fill])),
            s3("std", &[&remote_get]),
        );
        assert_eq!(backend.get(&HASH).await?.unwrap().as_ref(), b"cold");
        assert_eq!(local_fill.num_calls(), 1, "write-through filled Express");
        assert_recorded_tier(&recorder, "standard");
        Ok(())
    }

    /// Express 5xx must not turn a working S3-standard read into an error.
    // r[verify store.backend.tiered-get-fallback]
    #[tokio::test]
    async fn get_local_error_falls_back() -> anyhow::Result<()> {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let local_err = mock!(Client::get_object).then_error(|| {
            GetObjectError::generic(ErrorMetadata::builder().code("InternalError").build())
        });
        // Write-through still attempted after the error fallback.
        let local_fill =
            mock!(Client::put_object).then_output(|| PutObjectOutput::builder().build());
        let remote_get = mock!(Client::get_object).then_output(body(b"cold"));
        let backend = TieredChunkBackend::new(
            Some(s3("express", &[&local_err, &local_fill])),
            s3("std", &[&remote_get]),
        );
        assert_eq!(backend.get(&HASH).await?.unwrap().as_ref(), b"cold");
        // Express errored, never served — latency lands on the
        // standard arm only.
        assert_recorded_tier(&recorder, "standard");
        Ok(())
    }

    // r[verify store.backend.tiered-get-fallback]
    #[tokio::test]
    async fn get_writethrough_failure_swallowed() -> anyhow::Result<()> {
        let local_miss = mock!(Client::get_object).then_error(no_such_key);
        let local_fill_err = mock!(Client::put_object).then_error(|| {
            aws_sdk_s3::operation::put_object::PutObjectError::generic(
                ErrorMetadata::builder().code("SlowDown").build(),
            )
        });
        let remote_get = mock!(Client::get_object).then_output(body(b"cold"));
        let backend = TieredChunkBackend::new(
            Some(s3("express", &[&local_miss, &local_fill_err])),
            s3("std", &[&remote_get]),
        );
        assert_eq!(backend.get(&HASH).await?.unwrap().as_ref(), b"cold");
        Ok(())
    }

    /// Both tiers miss → `None`. Caller treats this as data loss.
    #[tokio::test]
    async fn get_both_miss_none() -> anyhow::Result<()> {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let local_miss = mock!(Client::get_object).then_error(no_such_key);
        let remote_miss = mock!(Client::get_object).then_error(no_such_key);
        let backend = TieredChunkBackend::new(
            Some(s3("express", &[&local_miss])),
            s3("std", &[&remote_miss]),
        );
        assert!(backend.get(&HASH).await?.is_none());
        // No bytes served → no latency observation on either arm; a
        // NotFound timing would pollute the serve-latency distribution.
        assert!(
            !recorder.histogram_touched(GET_HISTOGRAM),
            "no serve must record no latency, saw {:?}",
            recorder.histogram_keys()
        );
        Ok(())
    }

    // r[verify store.backend.tiered-get-fallback]
    #[tokio::test]
    async fn local_none_passes_through() -> anyhow::Result<()> {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let remote_get = mock!(Client::get_object).then_output(body(b"data"));
        let backend = TieredChunkBackend::new(None, s3("std", &[&remote_get]));
        assert_eq!(backend.get(&HASH).await?.unwrap().as_ref(), b"data");
        // Pass-through reads ARE S3-standard serves — same arm.
        assert_recorded_tier(&recorder, "standard");
        Ok(())
    }

    #[tokio::test]
    async fn exists_batch_remote_only() -> anyhow::Result<()> {
        use aws_sdk_s3::operation::head_object::{HeadObjectError, HeadObjectOutput};
        use aws_sdk_s3::types::error::NotFound;
        let r1 = mock!(Client::head_object).then_output(|| HeadObjectOutput::builder().build());
        let r2 = mock!(Client::head_object)
            .then_error(|| HeadObjectError::NotFound(NotFound::builder().build()));
        let backend =
            TieredChunkBackend::new(Some(must_not_touch("express")), s3("std", &[&r1, &r2]));
        assert_eq!(
            backend.exists_batch(&[[0x01; 32], [0x02; 32]]).await?,
            vec![true, false]
        );
        Ok(())
    }

    #[tokio::test]
    async fn delete_remote_only() -> anyhow::Result<()> {
        use aws_sdk_s3::operation::delete_object::DeleteObjectOutput;
        let del_rule =
            mock!(Client::delete_object).then_output(|| DeleteObjectOutput::builder().build());
        let backend =
            TieredChunkBackend::new(Some(must_not_touch("express")), s3("std", &[&del_rule]));
        let key = backend.key_for(&HASH);
        assert_eq!(key, format!("p/chunks/ab/{}", "ab".repeat(32)));
        backend.delete_by_key(&key).await?;
        assert_eq!(del_rule.num_calls(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn blob_ops_remote_only() -> anyhow::Result<()> {
        use aws_sdk_s3::operation::delete_object::DeleteObjectOutput;
        let put_r = mock!(Client::put_object).then_output(|| PutObjectOutput::builder().build());
        let get_r = mock!(Client::get_object).then_output(body(b"narinfo"));
        let del_r =
            mock!(Client::delete_object).then_output(|| DeleteObjectOutput::builder().build());
        let backend = TieredChunkBackend::new(
            Some(must_not_touch("express")),
            s3("std", &[&put_r, &get_r, &del_r]),
        );
        backend
            .put_blob("abc.narinfo", Bytes::from_static(b"x"))
            .await?;
        assert_eq!(
            backend.get_blob("abc.narinfo").await?.as_deref(),
            Some(b"narinfo".as_slice())
        );
        backend.delete_blob("abc.narinfo").await?;
        assert_eq!(put_r.num_calls(), 1);
        assert_eq!(get_r.num_calls(), 1);
        assert_eq!(del_r.num_calls(), 1);
        Ok(())
    }
}
