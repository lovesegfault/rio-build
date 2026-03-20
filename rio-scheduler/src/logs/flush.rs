//! Async S3 flush of log ring buffers.
//!
//! The flusher runs on its own task, driven by two triggers:
//!   1. **Completion** — actor `try_send`s a [`FlushRequest`] when a
//!      derivation hits a terminal state (success OR permanent failure).
// r[impl obs.log.periodic-flush]
//!      This drains the buffer (`LogBuffers::drain`) and uploads the final
//!      blob with `is_complete=true`.
//!   2. **Periodic (30s)** — tick scans all active buffers and uploads
//!      snapshots with `is_complete=false`. Does NOT drain: the derivation
//!      is still running, live serving via the ring buffer must continue.
//!      Per `observability.md:38-40` this bounds log loss on scheduler
//!      crash to ≤30s.
//!
//! The flusher NEVER blocks the actor. It's mpsc-fed (`try_send`, bounded
//! channel); if the channel is full, the actor's completion flush is
//! dropped and the next periodic tick catches it. The only cost of a
//! dropped completion-flush is that `is_complete` stays `false` until
//! `CleanupTerminalBuild` evicts the buffer (~30s later) and then there's
//! no buffer to snapshot either → the log is the last periodic snapshot,
//! which is slightly stale but still useful.
//!
//! Gzip is CPU-bound; runs in `spawn_blocking` so it doesn't hog a tokio
//! worker thread during the typical 10-100ms compression of a few-MB log.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;
use flate2::Compression;
use flate2::write::GzEncoder;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::LogBuffers;
use crate::state::DrvHash;

/// How often to snapshot active buffers to S3. Per `observability.md:38-40`:
/// bounds log loss on crash to ≤30s; lower = more S3 PUTs + CPU.
const PERIODIC_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Request to flush one derivation's logs. Sent by the actor from
/// `handle_completion_success` and `handle_permanent_failure` (both paths
/// flush — failed builds still have useful logs).
#[derive(Debug)]
pub struct FlushRequest {
    /// The buffer key.
    pub drv_path: String,
    /// For the S3 key (`logs/{build_id}/{drv_hash}.log.gz`) and PG row.
    pub drv_hash: DrvHash,
    /// One PG row per build, all pointing at the same S3 blob. The derivation
    /// builds exactly once even if N builds want it (DAG merging).
    pub interested_builds: Vec<Uuid>,
}

/// S3 log flusher. Owns no state except what's passed in — `Arc<LogBuffers>`
/// is shared with `SchedulerGrpc` (writes), `AdminServiceImpl` (reads),
/// and the actor (nobody — actor doesn't touch buffers, just sends flush reqs).
pub struct LogFlusher {
    s3: S3Client,
    bucket: String,
    /// Key prefix, no trailing slash. Typically `"logs"`.
    prefix: String,
    pool: PgPool,
    buffers: Arc<LogBuffers>,
}

impl LogFlusher {
    pub fn new(
        s3: S3Client,
        bucket: String,
        prefix: String,
        pool: PgPool,
        buffers: Arc<LogBuffers>,
    ) -> Self {
        Self {
            s3,
            bucket,
            prefix,
            pool,
            buffers,
        }
    }

    /// Spawn the flusher task. Returns a `JoinHandle` so callers can abort
    /// on shutdown (not awaited in production — `main()` just lets it die
    /// when the process exits).
    ///
    /// `flush_rx`: the actor's completion-flush channel. When this closes
    /// (actor died), the flusher exits.
    pub fn spawn(self, mut flush_rx: mpsc::Receiver<FlushRequest>) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(PERIODIC_FLUSH_INTERVAL);
            // Skip-behind: if a flush takes > 30s (shouldn't happen, but
            // S3 tail latencies exist), don't fire N missed ticks in a burst.
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // First tick fires immediately; consume it so the first real
            // periodic waits the full interval (no point snapshotting an
            // empty buffer set 0ms after startup).
            tick.tick().await;

            info!(
                bucket = %self.bucket,
                prefix = %self.prefix,
                interval = ?PERIODIC_FLUSH_INTERVAL,
                "log flusher started"
            );

            // Not spawn_periodic: the select is recv-vs-tick, not
            // shutdown-vs-tick (exits when flush_rx closes). The
            // `biased;` below is about completion-over-periodic
            // priority, not shutdown-over-tick.
            loop {
                tokio::select! {
                    // `biased` gives completion-flush priority over the tick.
                    // A completion landing right at a tick boundary should get
                    // the final `is_complete=true` flush, not a snapshot.
                    biased;

                    maybe = flush_rx.recv() => {
                        match maybe {
                            Some(req) => self.flush_final(req).await,
                            None => {
                                // Actor died. No more completions coming. One
                                // last periodic sweep to save whatever's in
                                // the buffers, then exit.
                                debug!("flush channel closed; final periodic sweep then exit");
                                self.flush_periodic().await;
                                break;
                            }
                        }
                    }

                    _ = tick.tick() => {
                        self.flush_periodic().await;
                    }
                }
            }

            info!("log flusher exited");
        })
    }

    /// On-completion flush: drain the buffer (derivation is done, no more
    /// writes coming) and upload with `is_complete=true`.
    async fn flush_final(&self, req: FlushRequest) {
        let Some((line_count, raw_bytes, lines)) = self.buffers.drain(&req.drv_path) else {
            // Buffer doesn't exist. Two legitimate causes:
            // (a) Derivation produced zero log output. Rare but possible
            //     (e.g., a silent `cp` FOD). No blob to write.
            // (b) A prior flush_final for the same drv_path already drained it
            //     (duplicate FlushRequest — actor retries are rare but possible).
            debug!(drv_path = %req.drv_path, "no buffer to flush (silent build or dup)");
            return;
        };
        self.upload_and_record(req, line_count, raw_bytes, lines, true)
            .await;
    }

    /// Periodic flush: snapshot all active buffers (non-draining) and upload
    /// with `is_complete=false`. Builds still running → buffer stays for
    /// live serving.
    async fn flush_periodic(&self) {
        let keys = self.buffers.active_keys();
        if keys.is_empty() {
            return; // no active derivations, nothing to do
        }
        debug!(active = keys.len(), "periodic log snapshot");

        for drv_path in keys {
            let Some((line_count, raw_bytes, lines)) = self.buffers.snapshot(&drv_path) else {
                // Buffer vanished between active_keys() and snapshot() —
                // drained by a concurrent flush_final. Fine, skip.
                continue;
            };

            // Periodic flush doesn't know drv_hash or interested_builds —
            // those live in the actor's DAG, which we deliberately don't
            // touch. So periodic snapshots use drv_path as the S3 key
            // component and write NO PG rows. The on-completion flush
            // (which DOES know hash+builds) writes the authoritative PG
            // rows + the canonical S3 key.
            //
            // Periodic snapshots are for crash recovery only: "the
            // scheduler died, but here's what was in the ring buffer ~30s
            // ago". A human operator can find them by S3 list on
            // `logs/periodic/`. They're NOT served by AdminService.
            //
            // drv_path has chars that aren't S3-safe (/nix/store/...), so
            // use the basename (hash-name.drv part).
            let basename = drv_path.rsplit('/').next().unwrap_or(&drv_path).to_string();
            let req = FlushRequest {
                drv_path,
                drv_hash: format!("periodic/{basename}").into(),
                interested_builds: Vec::new(), // no PG rows for periodic
            };
            self.upload_and_record(req, line_count, raw_bytes, lines, false)
                .await;
        }
    }

    /// Gzip → S3 PUT → PG insert (if `interested_builds` is non-empty).
    ///
    /// Errors are logged, not propagated. The flusher must never die on a
    /// transient S3/PG error — if it did, ALL future logs would be lost,
    /// not just this one derivation's. Failed flushes are retried on the
    /// next periodic tick for PERIODIC flushes (the buffer is still there).
    /// For FINAL flushes, `drain()` already removed the buffer — a failed
    /// S3 PUT after drain = lost log. This is an accepted risk (the
    /// alternative — re-inserting on fail — was considered and rejected
    /// as it complicates the buffer lifecycle for a rare edge case;
    /// is_complete would stay false anyway).
    async fn upload_and_record(
        &self,
        req: FlushRequest,
        line_count: u64,
        _raw_bytes: u64,
        lines: Vec<Vec<u8>>,
        is_final: bool,
    ) {
        // S3 key: for finals, `{prefix}/{first_build_id}/{drv_hash}.log.gz`.
        // Per spec (observability.md:12-14). We use the FIRST interested
        // build's id for the key path (arbitrary but deterministic — the
        // spec doesn't say which build_id when N>1, and PG rows all point
        // at the same s3_key anyway).
        //
        // For periodic: drv_hash already has the "periodic/{basename}"
        // prefix and interested_builds is empty → key is
        // `{prefix}/periodic/{basename}.log.gz`.
        let s3_key = if let Some(bid) = req.interested_builds.first() {
            format!("{}/{}/{}.log.gz", self.prefix, bid, req.drv_hash)
        } else {
            format!("{}/{}.log.gz", self.prefix, req.drv_hash)
        };

        // Gzip in spawn_blocking. ~10 MiB of log compresses in ~50ms on
        // modern hardware; not long enough to matter for latency, but long
        // enough to hog a tokio worker thread under heavy log volume
        // (50 active derivations × 50ms = 2.5s of worker-thread time per
        // periodic tick, spread across tokio's NUM_CPU workers).
        let gzipped = match tokio::task::spawn_blocking(move || gzip_lines(&lines)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => {
                error!(s3_key = %s3_key, error = %e, "gzip failed; log dropped");
                return;
            }
            Err(e) => {
                error!(s3_key = %s3_key, error = %e, "gzip task panicked; log dropped");
                return;
            }
        };
        let gzipped_size = gzipped.len() as u64;

        // S3 PUT. No retry loop here — the AWS SDK already retries
        // internally (RetryConfig::standard, set by main.rs). If the
        // SDK's retries are exhausted, log and move on.
        let put = self
            .s3
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(ByteStream::from(gzipped))
            .content_type("application/gzip")
            .send()
            .await;

        if let Err(e) = put {
            error!(
                s3_key = %s3_key,
                error = %e,
                is_final,
                "S3 PutObject failed; log for {} is lost (buffer already drained)",
                req.drv_path
            );
            metrics::counter!("rio_scheduler_log_flush_failures_total", "phase" => "s3")
                .increment(1);
            // We COULD re-push the lines back into the buffer here, but
            // we already moved `lines` into spawn_blocking. Caching a
            // clone before the spawn would double peak memory during
            // flush. Instead: accept the loss, alert on the metric above.
            // This is no worse than the pre-flusher state (no S3 at all).
            return;
        }

        debug!(
            s3_key = %s3_key,
            line_count,
            gzipped_size,
            is_final,
            "log flushed to S3"
        );
        metrics::counter!("rio_scheduler_log_flush_total", "kind" => if is_final { "final" } else { "periodic" }).increment(1);

        // PG rows — only for finals (periodic snapshots aren't dashboard-
        // servable, see flush_periodic comment).
        if !req.interested_builds.is_empty()
            && let Err(e) = insert_log_rows(
                &self.pool,
                &req.interested_builds,
                &req.drv_hash,
                &s3_key,
                line_count,
                gzipped_size,
                is_final,
            )
            .await
        {
            warn!(
                s3_key = %s3_key,
                error = %e,
                "PG build_logs insert failed; S3 blob exists but dashboard \
                 won't find it. Manual: SELECT from build_logs or S3 list."
            );
            metrics::counter!("rio_scheduler_log_flush_failures_total", "phase" => "pg")
                .increment(1);
        }
    }
}

/// Gzip-compress lines, joined by `\n`. Returns the compressed bytes.
///
/// Standalone fn so spawn_blocking can take it without capturing `self`.
fn gzip_lines(lines: &[Vec<u8>]) -> std::io::Result<Vec<u8>> {
    // Compression::default() is level 6. Higher levels (9) give marginally
    // smaller output at much higher CPU; log text is already highly
    // compressible at 6 (~10:1 on typical build output).
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    for line in lines {
        encoder.write_all(line)?;
        encoder.write_all(b"\n")?;
    }
    encoder.finish()
}

/// UPSERT build_logs rows (one per build_id). ON CONFLICT updates the
/// existing row — this handles the periodic-then-final sequence where a
/// snapshot row already exists and the final flush should overwrite it.
async fn insert_log_rows(
    pool: &PgPool,
    build_ids: &[Uuid],
    drv_hash: &DrvHash,
    s3_key: &str,
    line_count: u64,
    byte_size: u64,
    is_complete: bool,
) -> sqlx::Result<()> {
    // One query per build. Could be a bulk UNNEST, but N is typically 1-2
    // and the S3 PUT above already dominates latency.
    for bid in build_ids {
        sqlx::query(
            "INSERT INTO build_logs (build_id, drv_hash, s3_key, line_count, byte_size, is_complete)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (build_id, drv_hash) DO UPDATE SET
                 s3_key = EXCLUDED.s3_key,
                 line_count = EXCLUDED.line_count,
                 byte_size = EXCLUDED.byte_size,
                 is_complete = EXCLUDED.is_complete,
                 created_at = now()",
        )
        .bind(bid)
        .bind(drv_hash.as_str())
        .bind(s3_key)
        .bind(line_count as i64)
        .bind(byte_size as i64)
        .bind(is_complete)
        .execute(pool)
        .await?;
    }
    Ok(())
}

// r[verify obs.log.periodic-flush]
#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_smithy_mocks::{RuleMode, mock, mock_client};
    use rio_test_support::TestDb;

    /// `(key, body_bytes)` captured from a PutObject call.
    type CapturedPut = (String, Vec<u8>);
    /// Shared sink the `.match_requests` closure fills.
    type CapturedPuts = Arc<std::sync::Mutex<Vec<CapturedPut>>>;

    /// Build a mock S3 client that captures PutObject calls. Returns
    /// `(client, captured_requests)` where captured is the (key, body_bytes)
    /// pairs the client observed.
    ///
    /// aws-smithy-mocks doesn't have a direct "capture request" API, so we
    /// use an Arc<Mutex<Vec>> that the `.match_requests` closure fills.
    fn mock_s3_capturing_puts() -> (S3Client, CapturedPuts) {
        let captured: CapturedPuts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let cap = Arc::clone(&captured);
        // match_requests gets the parsed request struct. We can read the key
        // directly, but the body is a ByteStream which needs async to drain.
        // aws-smithy-mocks' closure is sync. Workaround: the body for our
        // test is small and in-memory; `.bytes()` on the inner returns it
        // synchronously for Bytes-backed streams (which ByteStream::from(Vec)
        // produces).
        let rule = mock!(S3Client::put_object)
            .match_requests(move |req| {
                let key = req.key().unwrap_or("<no-key>").to_string();
                // Body introspection via bytes(): works for in-memory bodies.
                // For file-backed streams this would be None; our tests always
                // use in-memory ByteStream::from(Vec<u8>).
                let body_bytes = req
                    .body()
                    .bytes()
                    .map(|b| b.to_vec())
                    .unwrap_or_else(|| b"<streaming-body-not-introspectable>".to_vec());
                cap.lock().unwrap().push((key, body_bytes));
                true
            })
            .then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);
        (client, captured)
    }

    fn mk_batch(
        drv_path: &str,
        first_line: u64,
        lines: &[&[u8]],
    ) -> rio_proto::types::BuildLogBatch {
        rio_proto::types::BuildLogBatch {
            derivation_path: drv_path.to_string(),
            lines: lines.iter().map(|l| l.to_vec()).collect(),
            first_line_number: first_line,
            worker_id: "test-worker".into(),
        }
    }

    /// Seed a build row so build_logs.build_id FK has something to reference.
    async fn seed_build(pool: &PgPool, build_id: Uuid) -> anyhow::Result<()> {
        sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
            .bind(build_id)
            .execute(pool)
            .await?;
        Ok(())
    }

    #[test]
    fn gzip_lines_is_gunzip_roundtrippable() -> anyhow::Result<()> {
        let lines: Vec<Vec<u8>> = vec![b"hello".to_vec(), b"world".to_vec(), b"!".to_vec()];
        let gz = gzip_lines(&lines)?;
        // Magic bytes for gzip: 1f 8b.
        assert_eq!(&gz[..2], &[0x1f, 0x8b], "gzip magic");
        // Gunzip and verify content.
        let mut decoder = flate2::read::GzDecoder::new(&gz[..]);
        let mut out = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut out)?;
        assert_eq!(out, "hello\nworld\n!\n");
        Ok(())
    }

    #[test]
    fn gzip_lines_empty_produces_valid_gzip() -> anyhow::Result<()> {
        // Edge case: zero lines. Still want a valid (empty) gzip blob, not
        // an error — a silent build's log is empty, not absent.
        let gz = gzip_lines(&[])?;
        assert_eq!(&gz[..2], &[0x1f, 0x8b]);
        let mut decoder = flate2::read::GzDecoder::new(&gz[..]);
        let mut out = String::new();
        std::io::Read::read_to_string(&mut decoder, &mut out)?;
        assert_eq!(out, "");
        Ok(())
    }

    #[tokio::test]
    async fn flush_final_drains_buffer_and_uploads() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let build_id = Uuid::new_v4();
        seed_build(&db.pool, build_id).await?;

        let (s3, captured) = mock_s3_capturing_puts();
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch("drv-a", 0, &[b"line0", b"line1", b"line2"]));
        assert_eq!(buffers.active_count(), 1);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            "logs".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        );

        flusher
            .flush_final(FlushRequest {
                drv_path: "drv-a".into(),
                drv_hash: "abc123hash".into(),
                interested_builds: vec![build_id],
            })
            .await;

        // Buffer drained.
        assert_eq!(buffers.active_count(), 0, "final flush should drain");

        // S3 PUT happened with the right key. Clone out of the lock —
        // holding the MutexGuard across the sqlx .await below would
        // deadlock if the mock client were called from another task
        // on this same thread (it isn't here, but clippy is right to
        // flag it as a footgun).
        let puts: Vec<CapturedPut> = captured.lock().unwrap().clone();
        assert_eq!(puts.len(), 1);
        let (key, body) = &puts[0];
        assert_eq!(key, &format!("logs/{build_id}/abc123hash.log.gz"));
        assert_eq!(&body[..2], &[0x1f, 0x8b], "should be gzipped");

        // PG row inserted with is_complete=true.
        let row: (i64, bool) = sqlx::query_as(
            "SELECT line_count, is_complete FROM build_logs WHERE build_id = $1 AND drv_hash = $2",
        )
        .bind(build_id)
        .bind("abc123hash")
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, 3, "line_count");
        assert!(row.1, "is_complete should be true for final flush");
        Ok(())
    }

    #[tokio::test]
    async fn flush_final_no_buffer_is_noop() -> anyhow::Result<()> {
        // Silent build (zero log output) or duplicate flush req — buffer
        // doesn't exist. Should not panic, not S3-PUT, not PG-insert.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, captured) = mock_s3_capturing_puts();
        let buffers = Arc::new(LogBuffers::new());
        // DON'T push anything.

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            "logs".into(),
            db.pool.clone(),
            buffers,
        );

        flusher
            .flush_final(FlushRequest {
                drv_path: "nonexistent".into(),
                drv_hash: "hash".into(),
                interested_builds: vec![Uuid::new_v4()],
            })
            .await;

        assert!(captured.lock().unwrap().is_empty(), "no S3 PUT");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM build_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 0, "no PG row");
        Ok(())
    }

    #[tokio::test]
    async fn flush_periodic_snapshots_not_drains() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, captured) = mock_s3_capturing_puts();
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch(
            "/nix/store/aaaa-test.drv",
            0,
            &[b"running", b"still-running"],
        ));

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            "logs".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        );

        flusher.flush_periodic().await;

        // Buffer NOT drained — derivation still running, live serving must work.
        assert_eq!(
            buffers.active_count(),
            1,
            "periodic flush must snapshot, not drain"
        );
        assert_eq!(buffers.read_since("/nix/store/aaaa-test.drv", 0).len(), 2);

        // S3 PUT happened under periodic/ key, NO PG row.
        let puts: Vec<CapturedPut> = captured.lock().unwrap().clone();
        assert_eq!(puts.len(), 1);
        let (key, _body) = &puts[0];
        assert_eq!(key, "logs/periodic/aaaa-test.drv.log.gz");

        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM build_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 0, "periodic flush writes no PG rows");
        Ok(())
    }

    #[tokio::test]
    async fn upsert_overwrites_snapshot_with_final() -> anyhow::Result<()> {
        // The periodic-then-final sequence: a periodic snapshot wrote a row
        // with is_complete=false (it doesn't in current code, but ON CONFLICT
        // must still handle the general case), then the final flush UPSERTs.
        // Actually wait — periodic doesn't write PG rows. So the only way to
        // hit ON CONFLICT is: two finals for the same (build, drv). That
        // happens if the actor sends a dup FlushRequest. The first one
        // drains + inserts; the second finds no buffer → noop. So ON CONFLICT
        // is unreachable in the current code path.
        //
        // BUT: it's still the right SQL for future-proofing (e.g., if we add
        // "retry final flush on S3 failure" later). Test it directly.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let build_id = Uuid::new_v4();
        seed_build(&db.pool, build_id).await?;

        let hash = DrvHash::from("hash");
        insert_log_rows(&db.pool, &[build_id], &hash, "key-v1", 10, 100, false).await?;
        insert_log_rows(&db.pool, &[build_id], &hash, "key-v2", 50, 500, true).await?;

        let row: (String, i64, bool) = sqlx::query_as(
            "SELECT s3_key, line_count, is_complete FROM build_logs
             WHERE build_id = $1 AND drv_hash = $2",
        )
        .bind(build_id)
        .bind("hash")
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, "key-v2", "UPSERT should overwrite");
        assert_eq!(row.1, 50);
        assert!(row.2);

        // Still exactly one row (UPSERT, not duplicate insert).
        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM build_logs WHERE build_id = $1 AND drv_hash = $2")
                .bind(build_id)
                .bind("hash")
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(count.0, 1);
        Ok(())
    }

    #[tokio::test]
    async fn flush_final_multiple_interested_builds_one_blob_n_rows() -> anyhow::Result<()> {
        // The DAG-merging case: 3 builds all want the same derivation.
        // One S3 blob, 3 PG rows, all with the same s3_key.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let b1 = Uuid::new_v4();
        let b2 = Uuid::new_v4();
        let b3 = Uuid::new_v4();
        for b in [b1, b2, b3] {
            seed_build(&db.pool, b).await?;
        }

        let (s3, captured) = mock_s3_capturing_puts();
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch("drv-shared", 0, &[b"shared-line"]));

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            "logs".into(),
            db.pool.clone(),
            buffers,
        );

        flusher
            .flush_final(FlushRequest {
                drv_path: "drv-shared".into(),
                drv_hash: "sharedhash".into(),
                interested_builds: vec![b1, b2, b3],
            })
            .await;

        // ONE S3 PUT.
        assert_eq!(captured.lock().unwrap().len(), 1);
        let expected_key = format!("logs/{b1}/sharedhash.log.gz");
        assert_eq!(captured.lock().unwrap()[0].0, expected_key);

        // THREE PG rows, all same s3_key.
        let rows: Vec<(Uuid, String)> =
            sqlx::query_as("SELECT build_id, s3_key FROM build_logs WHERE drv_hash = 'sharedhash'")
                .fetch_all(&db.pool)
                .await?;
        assert_eq!(rows.len(), 3);
        for (_bid, key) in &rows {
            assert_eq!(key, &expected_key, "all rows point at same blob");
        }
        let bids: std::collections::HashSet<Uuid> = rows.iter().map(|(b, _)| *b).collect();
        assert_eq!(bids, [b1, b2, b3].into_iter().collect());
        Ok(())
    }

    #[tokio::test]
    async fn s3_failure_logs_error_but_flusher_survives() -> anyhow::Result<()> {
        // S3 returns 500. Flusher must NOT panic or hang — it logs, increments
        // the failure metric, and returns. The NEXT flush_final for a different
        // drv should still work. (If the flusher died, all future logs lost.)
        let db = TestDb::new(&crate::MIGRATOR).await;
        let build_id = Uuid::new_v4();
        seed_build(&db.pool, build_id).await?;

        // Two rules: first PUT → generic server error, second PUT → OK.
        // (Same error-modeling approach as rio-store/src/backend/s3.rs:200 —
        // aws-smithy-mocks has no http-layer mock; `then_error` with a
        // generic ErrorMetadata is the supported way to simulate 5xx.)
        use aws_sdk_s3::error::ErrorMetadata;
        use aws_sdk_s3::operation::put_object::PutObjectError;
        let rule_fail = mock!(S3Client::put_object).then_error(|| {
            PutObjectError::generic(
                ErrorMetadata::builder()
                    .code("InternalError")
                    .message("simulated S3 500")
                    .build(),
            )
        });
        let rule_ok =
            mock!(S3Client::put_object).then_output(|| PutObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::Sequential, &[&rule_fail, &rule_ok]);

        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch("drv-fail", 0, &[b"will-be-lost"]));
        buffers.push(&mk_batch("drv-ok", 0, &[b"will-survive"]));

        let flusher = LogFlusher::new(
            client,
            "test-bucket".into(),
            "logs".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        );

        // First flush → S3 fails. Buffer is drained but upload fails → log
        // is lost. NOT a panic.
        flusher
            .flush_final(FlushRequest {
                drv_path: "drv-fail".into(),
                drv_hash: "failhash".into(),
                interested_builds: vec![build_id],
            })
            .await;
        assert_eq!(
            buffers.active_count(),
            1,
            "drv-fail drained (even though S3 failed), drv-ok still there"
        );

        // Second flush → S3 succeeds. Proves the flusher is still alive.
        flusher
            .flush_final(FlushRequest {
                drv_path: "drv-ok".into(),
                drv_hash: "okhash".into(),
                interested_builds: vec![build_id],
            })
            .await;

        // Only the second PG row exists (first flush failed before PG insert).
        let rows: Vec<(String,)> = sqlx::query_as("SELECT drv_hash FROM build_logs")
            .fetch_all(&db.pool)
            .await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "okhash");
        Ok(())
    }
}
