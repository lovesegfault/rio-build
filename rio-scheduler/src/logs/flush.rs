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
//! dropped. The buffer stays in `LogBuffers.buffers` (sealed) and the next
//! periodic tick still snapshots it to `logs/periodic/{hash}` — so the
//! content survives at the periodic key, but no `build_logs` PG row is
//! written and the dashboard sees only the last `is_complete=false`
//! snapshot. `CleanupTerminalBuild` (~30s later) reaps the DAG node and
//! discards the buffer (`LogBuffers::discard`), bounding the leak.
//!
//! Compression is CPU-bound; runs in `spawn_blocking` so it doesn't hog a
//! tokio worker thread during the typical 10-100ms compression of a few-MB log.

use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

use aws_sdk_s3::Client as S3Client;
use aws_sdk_s3::primitives::ByteStream;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};
use uuid::Uuid;

use super::{LogBuffers, drv_log_hash, log_s3_key};

/// How often to snapshot active buffers to S3. Per `observability.md:38-40`:
/// bounds log loss on crash to ≤30s; lower = more S3 PUTs + CPU.
const PERIODIC_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Request to flush one derivation's logs. Sent by the actor from
/// `handle_completion_success` and `handle_permanent_failure` (both paths
/// flush — failed builds still have useful logs).
#[derive(Debug)]
pub struct FlushRequest {
    /// The buffer key — full `/nix/store/{hash}-{name}.drv` path. Also
    /// the source for the S3 key (`logs/{build_id}/{drv_hash}.log.zst`)
    /// and PG `drv_hash` column via [`drv_log_hash`].
    pub drv_path: String,
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

    /// Spawn the flusher task. Shutdown is channel-driven: when
    /// `flush_rx` closes (actor dropped its `flush_tx`), the select
    /// loop's `recv()` returns `None` and the task exits. No
    /// `JoinHandle` is returned — callers never abort this task
    /// directly, and the previous handle was always `let _ =`-dropped.
    pub fn spawn(self, mut flush_rx: mpsc::Receiver<FlushRequest>) {
        rio_common::task::spawn_monitored("log-flusher", async move {
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
            // shutdown-vs-tick (exits when flush_rx closes). NOT
            // `biased;`: with biased, a sustained completion burst
            // (arrival > ~7/s drain rate) would starve `tick.tick()`
            // indefinitely — a concurrent long-running build would get
            // zero periodic snapshots, defeating the ≤30s log-loss
            // bound at r[obs.log.periodic-flush]. Fair (random) select
            // guarantees the tick fires within O(1) iterations once
            // ready; the worst case is one redundant periodic PUT just
            // before a final, which is harmless.
            loop {
                tokio::select! {
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
        });
    }

    /// On-completion flush: drain the buffer (derivation is done, no more
    /// writes coming) and upload with `is_complete=true`.
    async fn flush_final(&self, req: FlushRequest) {
        let drained = self.buffers.drain(&req.drv_path);
        // Seal bridged completion→drain; that window is now closed.
        // Clear so `sealed` stays bounded even if the recv task is
        // still running (or never saw a LogBatch — silent build).
        self.buffers.unseal(&req.drv_path);
        let Some((first_line, line_count, raw_bytes, lines)) = drained else {
            // Buffer doesn't exist. Two legitimate causes:
            // (a) Derivation produced zero log output. Rare but possible
            //     (e.g., a silent `cp` FOD). No blob to write.
            // (b) A prior flush_final for the same drv_path already drained it
            //     (duplicate FlushRequest — actor retries are rare but possible).
            debug!(drv_path = %req.drv_path, "no buffer to flush (silent build or dup)");
            return;
        };
        self.upload_and_record(req, first_line, line_count, raw_bytes, lines, true)
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
            let Some((first_line, line_count, raw_bytes, lines)) = self.buffers.snapshot(&drv_path)
            else {
                // Buffer vanished between active_keys() and snapshot() —
                // drained by a concurrent flush_final. Fine, skip.
                continue;
            };

            // Periodic flush doesn't know interested_builds — that lives
            // in the actor's DAG, which we deliberately don't touch. So
            // periodic snapshots write NO PG rows. The on-completion
            // flush (which DOES know builds) writes the authoritative PG
            // rows + the canonical S3 key.
            //
            // Periodic snapshots are for crash recovery only: "the
            // scheduler died, but here's what was in the ring buffer ~30s
            // ago". A human operator can find them by S3 list on
            // `logs/periodic/`. They're NOT served by AdminService.
            let req = FlushRequest {
                drv_path,
                interested_builds: Vec::new(), // no PG rows for periodic
            };
            self.upload_and_record(req, first_line, line_count, raw_bytes, lines, false)
                .await;
        }
    }

    /// Compress → S3 PUT → PG insert (if `interested_builds` is non-empty).
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
        first_line: u64,
        line_count: u64,
        _raw_bytes: u64,
        lines: Vec<Vec<u8>>,
        is_final: bool,
    ) {
        // S3 key: for finals, `{prefix}/{min_build_id}/{drv_hash}.log.zst`
        // via `log_s3_key()`. Per spec (observability.md:12-14). We use the
        // MIN interested build's id for the key path (deterministic:
        // `get_interested_builds()` sorts, so `.first()` is min(UUID) — the
        // spec doesn't say which build_id when N>1, and PG rows all point
        // at the same s3_key anyway).
        //
        // For periodic: interested_builds is empty → key is
        // `{prefix}/periodic/{drv_hash}.log.zst`.
        let drv_hash = drv_log_hash(&req.drv_path);
        debug_assert!(
            !drv_hash.is_empty(),
            "drv_log_hash({:?}) yielded empty hash — drv_path not store-path-shaped",
            req.drv_path
        );
        let s3_key = if let Some(bid) = req.interested_builds.first() {
            log_s3_key(&self.prefix, bid, &req.drv_path)
        } else {
            format!("{}/periodic/{drv_hash}.log.zst", self.prefix)
        };

        // Compress in spawn_blocking. ~10 MiB of log compresses in ~50ms on
        // modern hardware; not long enough to matter for latency, but long
        // enough to hog a tokio worker thread under heavy log volume
        // (50 active derivations × 50ms = 2.5s of worker-thread time per
        // periodic tick, spread across tokio's NUM_CPU workers).
        let compressed = match tokio::task::spawn_blocking(move || compress_lines(&lines)).await {
            Ok(Ok(bytes)) => bytes,
            Ok(Err(e)) => {
                flush_failure(is_final, "compress", &s3_key, &e, &req.drv_path);
                return;
            }
            Err(e) => {
                flush_failure(is_final, "compress", &s3_key, &e, &req.drv_path);
                return;
            }
        };
        let compressed_size = compressed.len() as u64;

        // S3 PUT. No retry loop here — the AWS SDK already retries
        // internally (RetryConfig::standard, set by main.rs). If the
        // SDK's retries are exhausted, log and move on.
        let put = self
            .s3
            .put_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .body(ByteStream::from(compressed))
            .content_type("application/zstd")
            .send()
            .await;

        if let Err(e) = put {
            // Re-push on final-flush failure was considered: we already
            // moved `lines` into spawn_blocking, and caching a clone
            // before the spawn would double peak memory. Accept the loss
            // for finals (alert on the metric); periodic retries itself.
            flush_failure(is_final, "s3", &s3_key, &e, &req.drv_path);
            return;
        }

        debug!(
            s3_key = %s3_key,
            first_line,
            line_count,
            compressed_size,
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
                &drv_hash,
                &s3_key,
                first_line,
                line_count,
                is_final,
            )
            .await
        {
            warn!(
                s3_key = %s3_key,
                "PG build_logs insert failed; S3 blob exists but dashboard \
                 won't find it. Manual: SELECT from build_logs or S3 list."
            );
            // Route through the chokepoint helper so the counter has a
            // consistent {phase, is_final} label set (bug_018: the
            // inline emit had `phase` only → `{is_final="true"}` queries
            // silently excluded PG failures).
            flush_failure(is_final, "pg", &s3_key, &e, &req.drv_path);
        }
    }
}

/// Log + metric for a compress/S3 failure. Level depends on `is_final`:
/// final flushes already drained the buffer (data is gone → `error!`);
/// periodic flushes snapshotted (buffer intact, next tick retries →
/// `warn!`). Prevents an S3 blip from emitting N false "log is lost"
/// `error!`s every 30s when nothing was lost.
fn flush_failure(
    is_final: bool,
    phase: &'static str,
    s3_key: &str,
    error: &dyn std::fmt::Display,
    drv_path: &str,
) {
    metrics::counter!(
        "rio_scheduler_log_flush_failures_total",
        "phase" => phase,
        "is_final" => if is_final { "true" } else { "false" },
    )
    .increment(1);
    if is_final {
        error!(
            s3_key = %s3_key, error = %error, is_final, phase,
            "log flush failed; log for {drv_path} is lost (buffer already drained)"
        );
    } else {
        warn!(
            s3_key = %s3_key, error = %error, is_final, phase,
            "log flush failed; periodic snapshot for {drv_path} will retry next tick"
        );
    }
}

/// Zstd-compress lines, joined by `\n`. Returns the compressed bytes.
///
/// Standalone fn so spawn_blocking can take it without capturing `self`.
fn compress_lines(lines: &[Vec<u8>]) -> std::io::Result<Vec<u8>> {
    // Level 6 (NOT the crate default 3): log text is already highly
    // compressible (~10:1 on typical build output), and the periodic
    // flush re-uploads ever-growing prefixes — the extra ratio at 6 is
    // worth the CPU on a path that's already off-thread in spawn_blocking.
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 6)?;
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
    drv_hash: &str,
    s3_key: &str,
    first_line: u64,
    line_count: u64,
    is_complete: bool,
) -> sqlx::Result<()> {
    // One query per build. Could be a bulk UNNEST, but N is typically 1-2
    // and the S3 PUT above already dominates latency.
    for bid in build_ids {
        sqlx::query(
            "INSERT INTO build_logs
                 (build_id, drv_hash, s3_key, first_line, line_count, is_complete)
             VALUES ($1, $2, $3, $4, $5, $6)
             ON CONFLICT (build_id, drv_hash) DO UPDATE SET
                 s3_key = EXCLUDED.s3_key,
                 first_line = EXCLUDED.first_line,
                 line_count = EXCLUDED.line_count,
                 is_complete = EXCLUDED.is_complete,
                 created_at = now()",
        )
        .bind(bid)
        .bind(drv_hash)
        .bind(s3_key)
        .bind(first_line as i64)
        .bind(line_count as i64)
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
            executor_id: "test-worker".into(),
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
    fn compress_lines_is_zstd_roundtrippable() -> anyhow::Result<()> {
        let lines: Vec<Vec<u8>> = vec![b"hello".to_vec(), b"world".to_vec(), b"!".to_vec()];
        let zst = compress_lines(&lines)?;
        // Magic bytes for a zstd frame: 28 b5 2f fd.
        assert_eq!(&zst[..4], &[0x28, 0xb5, 0x2f, 0xfd], "zstd magic");
        // Decode and verify content.
        let out = zstd::decode_all(&zst[..])?;
        assert_eq!(out, b"hello\nworld\n!\n");
        Ok(())
    }

    #[test]
    fn compress_lines_empty_produces_valid_zstd() -> anyhow::Result<()> {
        // Edge case: zero lines. Still want a valid (empty) zstd frame, not
        // an error — a silent build's log is empty, not absent.
        let zst = compress_lines(&[])?;
        assert_eq!(&zst[..4], &[0x28, 0xb5, 0x2f, 0xfd]);
        let out = zstd::decode_all(&zst[..])?;
        assert_eq!(out, b"");
        Ok(())
    }

    #[tokio::test]
    async fn flush_final_drains_buffer_and_uploads() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let build_id = Uuid::new_v4();
        seed_build(&db.pool, build_id).await?;

        // Use a realistic full store path — regression guard for the bug
        // where the S3 key embedded the entire `/nix/store/{hash}-{name}.drv`
        // (producing `logs/{bid}//nix/store/...`) instead of just `{hash}`.
        let drv_path = "/nix/store/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-firefox-unwrapped-149.0.drv";
        let drv_hash = "amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2";

        let (s3, captured) = mock_s3_capturing_puts();
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch(drv_path, 0, &[b"line0", b"line1", b"line2"]));
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
                drv_path: drv_path.into(),
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
        assert_eq!(key, &format!("logs/{build_id}/{drv_hash}.log.zst"));
        assert!(
            !key.contains("/nix/store/"),
            "S3 key must not embed the store prefix"
        );
        assert_eq!(&body[..4], &[0x28, 0xb5, 0x2f, 0xfd], "should be zstd");

        // PG row inserted with is_complete=true.
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM build_logs \
             WHERE build_id = $1 AND drv_hash = $2",
        )
        .bind(build_id)
        .bind(drv_hash)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, 0, "first_line (no eviction)");
        assert_eq!(row.1, 3, "line_count");
        assert!(row.2, "is_complete should be true for final flush");
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
        assert_eq!(
            buffers
                .read_since("/nix/store/aaaa-test.drv", 0)
                .unwrap()
                .len(),
            2
        );

        // S3 PUT happened under periodic/ key, NO PG row.
        let puts: Vec<CapturedPut> = captured.lock().unwrap().clone();
        assert_eq!(puts.len(), 1);
        let (key, _body) = &puts[0];
        assert_eq!(key, "logs/periodic/aaaa.log.zst");

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

        insert_log_rows(&db.pool, &[build_id], "hash", "key-v1", 0, 10, false).await?;
        insert_log_rows(&db.pool, &[build_id], "hash", "key-v2", 7, 50, true).await?;

        let row: (String, i64, i64, bool) = sqlx::query_as(
            "SELECT s3_key, first_line, line_count, is_complete FROM build_logs
             WHERE build_id = $1 AND drv_hash = $2",
        )
        .bind(build_id)
        .bind("hash")
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, "key-v2", "UPSERT should overwrite");
        assert_eq!(row.1, 7, "first_line UPSERTed");
        assert_eq!(row.2, 50);
        assert!(row.3);

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

        let drv_path = "/nix/store/ssssssssssssssssssssssssssssssss-shared.drv";
        let (s3, captured) = mock_s3_capturing_puts();
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch(drv_path, 0, &[b"shared-line"]));

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            "logs".into(),
            db.pool.clone(),
            buffers,
        );

        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                interested_builds: vec![b1, b2, b3],
            })
            .await;

        // ONE S3 PUT.
        assert_eq!(captured.lock().unwrap().len(), 1);
        let expected_key = format!("logs/{b1}/ssssssssssssssssssssssssssssssss.log.zst");
        assert_eq!(captured.lock().unwrap()[0].0, expected_key);

        // THREE PG rows, all same s3_key.
        let rows: Vec<(Uuid, String)> = sqlx::query_as(
            "SELECT build_id, s3_key FROM build_logs \
             WHERE drv_hash = 'ssssssssssssssssssssssssssssssss'",
        )
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

    // bug_032 regression (S3-key determinism via sorted interested_builds)
    // is at `actor::tests::misc::get_interested_builds_is_sorted` — it
    // exercises the production `get_interested_builds()` path; testing
    // here would require re-implementing the sort locally (vacuous).

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
        // Keys with no `-` so `drv_log_hash` leaves them distinct.
        buffers.push(&mk_batch("drvfail", 0, &[b"will-be-lost"]));
        buffers.push(&mk_batch("drvok", 0, &[b"will-survive"]));

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
                drv_path: "drvfail".into(),
                interested_builds: vec![build_id],
            })
            .await;
        assert_eq!(
            buffers.active_count(),
            1,
            "drvfail drained (even though S3 failed), drvok still there"
        );

        // Second flush → S3 succeeds. Proves the flusher is still alive.
        flusher
            .flush_final(FlushRequest {
                drv_path: "drvok".into(),
                interested_builds: vec![build_id],
            })
            .await;

        // Only the second PG row exists (first flush failed before PG insert).
        let rows: Vec<(String,)> = sqlx::query_as("SELECT drv_hash FROM build_logs")
            .fetch_all(&db.pool)
            .await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "drvok");
        Ok(())
    }

    /// Regression for bug_367: a periodic-flush S3 failure used to
    /// `error!("log is lost (buffer already drained)")` — false: periodic
    /// `snapshot()`s, doesn't drain; the buffer is intact and retries next
    /// tick. Only final flushes (which `drain()`) lose data on S3 fail.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn s3_failure_periodic_warns_not_errors() -> anyhow::Result<()> {
        use aws_sdk_s3::error::ErrorMetadata;
        use aws_sdk_s3::operation::put_object::PutObjectError;
        let db = TestDb::new(&crate::MIGRATOR).await;
        let rule_fail = mock!(S3Client::put_object).then_error(|| {
            PutObjectError::generic(
                ErrorMetadata::builder()
                    .code("InternalError")
                    .message("simulated S3 500")
                    .build(),
            )
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule_fail]);

        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch(
            "/nix/store/pppppppppppppppppppppppppppppppp-periodic.drv",
            0,
            &[b"still-running"],
        ));
        let flusher = LogFlusher::new(
            client,
            "test-bucket".into(),
            "logs".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        );

        flusher.flush_periodic().await;

        // Buffer is intact — snapshot, not drain.
        assert_eq!(buffers.active_count(), 1);
        // warn-level retry message, NOT the error-level "is lost" claim.
        assert!(logs_contain("will retry next tick"));
        assert!(!logs_contain("is lost"));
        assert!(!logs_contain("buffer already drained"));
        Ok(())
    }

    /// Smoke for bug_365: the spawn loop's periodic tick must fire on
    /// schedule. The actual starvation (`biased;` + recv-never-empty)
    /// requires arrival_rate ≥ drain_rate, which depends on real
    /// ~150ms S3 latency — with a mock client each flush_final is μs,
    /// so the channel always empties before the tick deadline and the
    /// starvation can't be reproduced without injecting artificial
    /// latency. The fix (drop `biased;`) is correct by `select!`
    /// semantics; this test guards the surrounding loop wiring.
    #[tokio::test]
    async fn periodic_tick_fires_in_spawn_loop() -> anyhow::Result<()> {
        // TestDb before pause(): sqlx pool acquire uses tokio::time
        // for its timeout, which PoolTimedOuts under paused time.
        let db = TestDb::new(&crate::MIGRATOR).await;
        tokio::time::pause();

        let (s3, captured) = mock_s3_capturing_puts();
        let buffers = Arc::new(LogBuffers::new());
        let ongoing_hash = "ongoingxxxxxxxxxxxxxxxxxxxxxxxxx";
        buffers.push(&mk_batch(
            &format!("/nix/store/{ongoing_hash}-llvm.drv"),
            0,
            &[b"compiling"],
        ));

        let (tx, rx) = mpsc::channel::<FlushRequest>(8);
        LogFlusher::new(
            s3,
            "b".into(),
            "logs".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        )
        .spawn(rx);

        // Auto-advance drives the interval; tx open so the loop is
        // recv-vs-tick (not the channel-close final sweep).
        tokio::time::sleep(Duration::from_secs(65)).await;

        let ongoing_key = format!("logs/periodic/{ongoing_hash}.log.zst");
        let periodic = captured
            .lock()
            .unwrap()
            .iter()
            .filter(|(k, _)| k == &ongoing_key)
            .count();
        assert!(
            periodic >= 2,
            "two ticks in 65s → ≥2 periodic PUTs, got {periodic}"
        );
        drop(tx);
        Ok(())
    }
}
