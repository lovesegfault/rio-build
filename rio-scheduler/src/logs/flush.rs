//! Async S3 flush of log ring buffers.
//!
//! The flusher runs on its own task, driven by two triggers:
//!   1. **Completion** — actor `try_send`s a [`FlushRequest`] when a
//!      derivation hits a terminal state (success OR permanent failure).
// r[impl obs.log.periodic-flush]
//!      This drains the buffer (`LogBuffers::drain`) and uploads the final
//!      blob to `logs/{drv_hash}/{exec_id}.log.zst` with `is_complete=true`.
//!   2. **Periodic (30s)** — tick scans all active buffers and uploads
//!      snapshots to `logs/{drv_hash}/{exec_id}.partial.log.zst` with
//!      `is_complete=false`. Does NOT drain: the derivation is still
//!      running, live serving via the ring buffer must continue. Per
//!      `observability.typ` this bounds log loss on scheduler crash to
//!      ≤30s. The `.partial` is best-effort deleted by the final flush
//!      (or swept by the TTL GC at expiry if that delete fails).
//!
//! Both flush kinds write **one** `drv_logs` row per execution, UPSERTed on
//! `(exec_id)` — a periodic snapshot inserts the row at `is_complete=false`,
//! the final flush flips it to `is_complete=true` and stamps `finished_at`.
//! The `exec_id` (per-execution UUIDv7 minted by `assign_to_worker`) lives
//! on the `LogBuffers` ring-buffer entry; the flusher reads it from there
//! because it has no actor `FlushRequest` for the periodic path. A flush
//! with no `exec_id` (entry never `set_exec`'d — recovery gap or
//! test-construction artifact) is **dropped**, not written under a garbage
//! key.
//!
//! The flusher NEVER blocks the actor. It's mpsc-fed (`try_send`, bounded
//! channel); if the channel is full, the actor's completion flush is
//! dropped. The buffer stays in `LogBuffers.buffers` (sealed) and the next
//! periodic tick still snapshots it — so the content survives at the
//! `.partial` key with an `is_complete=false` PG row. `CleanupTerminalBuild`
//! (~30s later) reaps the DAG node and discards the buffer
//! (`LogBuffers::discard`), bounding the leak.
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

/// How often to snapshot active buffers to S3. Per `observability.typ`:
/// bounds log loss on crash to ≤30s; lower = more S3 PUTs + CPU.
const PERIODIC_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// Request to flush one derivation's logs. Sent by the actor from
/// `handle_completion_success` and `terminal_failure_epilogue` (both paths
/// flush — failed builds still have useful logs).
#[derive(Debug)]
pub struct FlushRequest {
    /// The buffer key — full `/nix/store/{hash}-{name}.drv` path. Also the
    /// source for the S3 key `logs/{drv_hash}/{exec_id}.log.zst` and PG
    /// `drv_logs.drv_hash` column via [`drv_log_hash`].
    pub drv_path: String,
    /// Build outcome for `drv_logs.status` (`"succeeded"` / `"failed"`).
    /// `None` for periodic snapshots (build still running). Recorded in PG
    /// so `rio-cli logs` and the dashboard can show outcome alongside the
    /// log without a join against `derivations`.
    pub status: Option<String>,
}

/// S3 log flusher. Owns no state except what's passed in — `Arc<LogBuffers>`
/// is shared with `SchedulerGrpc` (writes), `AdminServiceImpl` (reads),
/// and the actor (nobody — actor doesn't touch buffers, just sends flush reqs).
pub struct LogFlusher {
    s3: S3Client,
    bucket: String,
    pool: PgPool,
    buffers: Arc<LogBuffers>,
}

impl LogFlusher {
    pub fn new(s3: S3Client, bucket: String, pool: PgPool, buffers: Arc<LogBuffers>) -> Self {
        Self {
            s3,
            bucket,
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
        // Read exec_id BEFORE drain() removes the entry. The accessor
        // returns None if the entry doesn't exist (silent build, dup
        // request) or was never set_exec'd — both make the flush a no-op
        // anyway, so the ordering is safe.
        let exec_id = self.buffers.exec_id(&req.drv_path);
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
        self.upload_and_record(req, exec_id, first_line, line_count, raw_bytes, lines, true)
            .await;
    }

    /// Periodic flush: snapshot all active buffers (non-draining) and upload
    /// with `is_complete=false`. Builds still running → buffer stays for
    /// live serving. No actor `FlushRequest` is involved — the periodic
    /// flush is self-driven off `LogBuffers::active_keys()` and reads
    /// `exec_id` from the ring-buffer entry (the carrier `set_exec` stamps).
    async fn flush_periodic(&self) {
        let keys = self.buffers.active_keys();
        if keys.is_empty() {
            return; // no active derivations, nothing to do
        }
        debug!(active = keys.len(), "periodic log snapshot");

        for drv_path in keys {
            let exec_id = self.buffers.exec_id(&drv_path);
            let Some((first_line, line_count, raw_bytes, lines)) = self.buffers.snapshot(&drv_path)
            else {
                // Buffer vanished between active_keys() and snapshot() —
                // drained by a concurrent flush_final. Fine, skip.
                continue;
            };

            let req = FlushRequest {
                drv_path,
                // Build still running — outcome unknown. Stays NULL in
                // PG until the final flush sets it.
                status: None,
            };
            self.upload_and_record(
                req, exec_id, first_line, line_count, raw_bytes, lines, false,
            )
            .await;
        }
    }

    /// Compress → S3 PUT → PG UPSERT (one row per execution).
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
    ///
    /// `exec_id` is read from the ring-buffer entry by the caller (before
    /// `drain()` removes it for finals). `None` ⇒ skip: there is no
    /// meaningful S3 key without it. `set_exec` is always called at
    /// `assign_to_worker` and re-stamped by recovery for active
    /// assignments, so a `None` here is a recovery gap or a
    /// test-construction artifact — drop loudly rather than write under a
    /// garbage key the read path will never find.
    #[allow(clippy::too_many_arguments)] // call-site-local; threading a struct would just rename the args
    async fn upload_and_record(
        &self,
        req: FlushRequest,
        exec_id: Option<Uuid>,
        first_line: u64,
        line_count: u64,
        raw_bytes: u64,
        lines: Vec<Vec<u8>>,
        is_final: bool,
    ) {
        let Some(exec_id) = exec_id else {
            warn!(
                drv = %req.drv_path,
                is_final,
                "skipping flush: no exec_id (set_exec never called — recovery gap or test artifact)"
            );
            return;
        };
        // set_exec creates an empty ring-buffer entry; the periodic tick
        // would otherwise flush a zero-line `.partial` blob and PG row for
        // the window between dispatch and the worker's first batch (overlay
        // setup, FUSE warm — easily >30s). Skip — there's nothing to
        // store. flush_final's caller already early-returns on a None
        // drain (silent build), so this only fires for the periodic path's
        // snapshot of a stamped-but-empty entry.
        if line_count == 0 {
            return;
        }

        let drv_hash = drv_log_hash(&req.drv_path);
        debug_assert!(
            !drv_hash.is_empty(),
            "drv_log_hash({:?}) yielded empty hash — drv_path not store-path-shaped",
            req.drv_path
        );
        let s3_key = log_s3_key(&req.drv_path, &exec_id, !is_final);

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

        // PG UPSERT — one row per execution, keyed on exec_id. Periodic
        // and final flushes UPSERT the same row (is_complete flips
        // false→true on the final).
        if let Err(e) = upsert_drv_log(
            &self.pool,
            exec_id,
            &drv_hash,
            &s3_key,
            first_line,
            line_count,
            raw_bytes,
            is_final,
            req.status.as_deref(),
        )
        .await
        {
            warn!(
                s3_key = %s3_key,
                "PG drv_logs upsert failed; S3 blob exists but read path \
                 won't find it. Manual: SELECT from drv_logs or S3 list."
            );
            // Route through the chokepoint helper so the counter has a
            // consistent {phase, is_final} label set (bug_018: the
            // inline emit had `phase` only → `{is_final="true"}` queries
            // silently excluded PG failures).
            flush_failure(is_final, "pg", &s3_key, &e, &req.drv_path);
            return;
        }

        // Best-effort delete the `.partial` snapshot AFTER the final blob's
        // PG row landed — the final supersedes it. A failed delete leaves a
        // stale `.partial` that the TTL GC sweep catches at expiry; that's
        // an accepted residual leak (bounded by retention), not an error.
        if is_final {
            let partial_key = log_s3_key(&req.drv_path, &exec_id, true);
            if let Err(e) = self
                .s3
                .delete_object()
                .bucket(&self.bucket)
                .key(&partial_key)
                .send()
                .await
            {
                debug!(
                    key = %partial_key,
                    error = %e,
                    "best-effort .partial delete failed (TTL sweep will catch it at expiry)"
                );
            }
        }
    }
}

/// Log + metric for a compress/S3/PG failure. Level depends on `is_final`:
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

/// UPSERT one `drv_logs` row keyed on `(exec_id)`.
///
/// The periodic→final sequence UPSERTs the same row: a periodic snapshot
/// inserts at `is_complete=false`, the final flush flips it `true`, swaps
/// the `s3_key` from `.partial.log.zst` to `.log.zst`, and stamps
/// `finished_at`. `started_at` is intentionally NOT in `DO UPDATE SET` —
/// the first INSERT decodes it from the UUIDv7's embedded timestamp (the
/// dispatch instant, exact, no clock read), and subsequent UPSERTs preserve
/// the original. The workspace sqlx has no chrono/time feature (see
/// `db/history.rs:44`), so the timestamp is bound as f64 epoch seconds and
/// PG's `to_timestamp()` does the conversion server-side; `finished_at`
/// uses server-side `now()` matching the existing `builds` write path.
#[allow(clippy::too_many_arguments)] // one UPSERT with the full row — a struct param would just rename the args
async fn upsert_drv_log(
    pool: &PgPool,
    exec_id: Uuid,
    drv_hash: &str,
    s3_key: &str,
    first_line: u64,
    line_count: u64,
    total_bytes: u64,
    is_complete: bool,
    status: Option<&str>,
) -> sqlx::Result<()> {
    // Decode the dispatch instant from the UUIDv7's high 48 bits. A
    // non-v7 UUID has no embedded timestamp — should never happen since
    // `assign_to_worker` mints with `Uuid::now_v7()`. A naive
    // `Option<f64>` bind would `to_timestamp(NULL)` → NOT NULL violation
    // on `started_at`, so:
    let started_at_epoch: f64 = exec_id
        .get_timestamp()
        .map(|ts| {
            let (secs, nanos) = ts.to_unix();
            secs as f64 + f64::from(nanos) / 1e9
        })
        // Use 0.0 (1970) rather than panicking — the row is wrong but the
        // system stays up, and the GC sweep will expire it immediately,
        // which is what you'd want for a row that should never have
        // existed.
        .unwrap_or(0.0);

    sqlx::query(
        "INSERT INTO drv_logs
             (exec_id, drv_hash, s3_key, first_line, line_count, total_bytes,
              is_complete, status, started_at, finished_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, to_timestamp($9),
                 CASE WHEN $7 THEN now() ELSE NULL END)
         ON CONFLICT (exec_id) DO UPDATE SET
             s3_key = EXCLUDED.s3_key,
             first_line = EXCLUDED.first_line,
             line_count = EXCLUDED.line_count,
             total_bytes = EXCLUDED.total_bytes,
             is_complete = EXCLUDED.is_complete,
             status = EXCLUDED.status,
             finished_at = EXCLUDED.finished_at",
    )
    .bind(exec_id)
    .bind(drv_hash)
    .bind(s3_key)
    .bind(first_line as i64)
    .bind(line_count as i64)
    .bind(total_bytes as i64)
    .bind(is_complete)
    .bind(status)
    .bind(started_at_epoch)
    .execute(pool)
    .await?;
    Ok(())
}

// r[verify obs.log.periodic-flush]
// r[verify obs.log.exec-keyed]
#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_s3::operation::delete_object::DeleteObjectOutput;
    use aws_sdk_s3::operation::put_object::PutObjectOutput;
    use aws_smithy_mocks::{RuleMode, mock, mock_client};
    use rio_test_support::TestDb;

    /// `(key, body_bytes)` captured from a PutObject call.
    type CapturedPut = (String, Vec<u8>);
    /// Shared sink the `.match_requests` closure fills.
    type CapturedPuts = Arc<std::sync::Mutex<Vec<CapturedPut>>>;
    /// DeleteObject keys captured.
    type CapturedDeletes = Arc<std::sync::Mutex<Vec<String>>>;

    /// Build a mock S3 client that captures PutObject and DeleteObject
    /// calls. Returns `(client, captured_puts, captured_deletes)`.
    ///
    /// aws-smithy-mocks doesn't have a direct "capture request" API, so we
    /// use an `Arc<Mutex<Vec>>` that the `.match_requests` closure fills.
    fn mock_s3_capturing() -> (S3Client, CapturedPuts, CapturedDeletes) {
        let puts: CapturedPuts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deletes: CapturedDeletes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let pcap = Arc::clone(&puts);
        let dcap = Arc::clone(&deletes);
        // match_requests gets the parsed request struct. We can read the key
        // directly, but the body is a ByteStream which needs async to drain.
        // aws-smithy-mocks' closure is sync. Workaround: the body for our
        // test is small and in-memory; `.bytes()` on the inner returns it
        // synchronously for Bytes-backed streams (which ByteStream::from(Vec)
        // produces).
        let put_rule = mock!(S3Client::put_object)
            .match_requests(move |req| {
                let key = req.key().unwrap_or("<no-key>").to_string();
                let body_bytes = req
                    .body()
                    .bytes()
                    .map(|b| b.to_vec())
                    .unwrap_or_else(|| b"<streaming-body-not-introspectable>".to_vec());
                pcap.lock().unwrap().push((key, body_bytes));
                true
            })
            .then_output(|| PutObjectOutput::builder().build());
        let del_rule = mock!(S3Client::delete_object)
            .match_requests(move |req| {
                dcap.lock()
                    .unwrap()
                    .push(req.key().unwrap_or("<no-key>").to_string());
                true
            })
            .then_output(|| DeleteObjectOutput::builder().build());
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&put_rule, &del_rule]);
        (client, puts, deletes)
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

    /// Stamp + populate a buffer for `drv_path`. Returns the `exec_id`
    /// the flusher will key the S3 blob and PG row on. Mirrors the
    /// production order: `assign_to_worker` calls `set_exec` (which
    /// creates the entry), then the worker's first `BuildLogBatch`
    /// populates it.
    fn stamp_and_push(buffers: &LogBuffers, drv_path: &str, lines: &[&[u8]]) -> Uuid {
        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");
        if !lines.is_empty() {
            buffers.push(&mk_batch(drv_path, 0, lines));
        }
        exec_id
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

        // Use a realistic full store path — regression guard for the bug
        // where the S3 key embedded the entire `/nix/store/{hash}-{name}.drv`
        // (producing `logs/{hash}//nix/store/...`) instead of just `{hash}`.
        let drv_path = "/nix/store/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-firefox-unwrapped-149.0.drv";
        let drv_hash = "amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2";

        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"line0", b"line1", b"line2"]);
        assert_eq!(buffers.active_count(), 1);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        );

        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                status: Some("succeeded".into()),
            })
            .await;

        // Buffer drained.
        assert_eq!(buffers.active_count(), 0, "final flush should drain");

        // S3 PUT happened with the right key. Clone out of the lock —
        // holding the MutexGuard across the sqlx .await below would
        // deadlock if the mock client were called from another task
        // on this same thread (it isn't here, but clippy is right to
        // flag it as a footgun).
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let (key, body) = &captured[0];
        assert_eq!(key, &format!("logs/{drv_hash}/{exec_id}.log.zst"));
        assert!(
            !key.contains("/nix/store/"),
            "S3 key must not embed the store prefix"
        );
        assert_eq!(&body[..4], &[0x28, 0xb5, 0x2f, 0xfd], "should be zstd");

        // The final flush best-effort deletes the `.partial` snapshot.
        let dels: Vec<String> = deletes.lock().unwrap().clone();
        assert_eq!(
            dels,
            vec![format!("logs/{drv_hash}/{exec_id}.partial.log.zst")]
        );

        // ONE PG row, keyed on exec_id, with is_complete=true.
        let row: (i64, i64, i64, bool, Option<String>) = sqlx::query_as(
            "SELECT first_line, line_count, total_bytes, is_complete, status \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, 0, "first_line (no eviction)");
        assert_eq!(row.1, 3, "line_count");
        assert_eq!(
            row.2, 15,
            "total_bytes (raw line bytes from RingBuf — no newlines: 3 × 5)"
        );
        assert!(row.3, "is_complete should be true for final flush");
        assert_eq!(row.4.as_deref(), Some("succeeded"));
        // Exactly one row total — no fan-out.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 1);
        Ok(())
    }

    #[tokio::test]
    async fn flush_final_no_buffer_is_noop() -> anyhow::Result<()> {
        // Silent build (zero log output) or duplicate flush req — buffer
        // doesn't exist. Should not panic, not S3-PUT, not PG-insert.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        // DON'T set_exec or push anything.

        let flusher = LogFlusher::new(s3, "test-bucket".into(), db.pool.clone(), buffers);

        flusher
            .flush_final(FlushRequest {
                drv_path: "nonexistent".into(),
                status: Some("failed".into()),
            })
            .await;

        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 0, "no PG row");
        Ok(())
    }

    #[tokio::test]
    async fn flush_skipped_without_exec_id() -> anyhow::Result<()> {
        // A push() without set_exec creates an entry with exec_id = None.
        // The flusher MUST skip it — there's no meaningful S3 key without
        // exec_id. This guards the load-bearing invariant in the
        // `RingBuf.exec_id` doc.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        // Legacy push() without set_exec → entry exists but unstamped.
        buffers.push(&mk_batch("/nix/store/aaaa-nostamp.drv", 0, &[b"line"]));

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        );

        // Both paths should skip.
        flusher.flush_periodic().await;
        flusher
            .flush_final(FlushRequest {
                drv_path: "/nix/store/aaaa-nostamp.drv".into(),
                status: Some("succeeded".into()),
            })
            .await;

        assert!(
            puts.lock().unwrap().is_empty(),
            "no S3 PUT for unstamped entry"
        );
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 0, "no PG row for unstamped entry");
        Ok(())
    }

    #[tokio::test]
    async fn zero_line_flush_is_skipped() -> anyhow::Result<()> {
        // set_exec creates an empty entry. flush_periodic must not produce
        // an S3 PUT or PG row for it — there's nothing to store, and a
        // zero-line `.partial` blob/row for every dispatched-but-not-yet-
        // streaming drv would be noise.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        buffers.set_exec("/nix/store/aaaa-empty.drv", Uuid::now_v7(), "test-worker");
        assert_eq!(buffers.active_count(), 1, "set_exec creates the entry");

        let flusher = LogFlusher::new(s3, "test-bucket".into(), db.pool.clone(), buffers);

        flusher.flush_periodic().await;

        assert!(
            puts.lock().unwrap().is_empty(),
            "no S3 PUT for empty buffer"
        );
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 0, "no PG row for empty buffer");
        Ok(())
    }

    #[tokio::test]
    async fn flush_periodic_snapshots_not_drains() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/aaaa-test.drv";
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"running", b"still-running"]);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
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
        assert_eq!(buffers.read_since(drv_path, 0).unwrap().len(), 2);

        // S3 PUT happened under the `.partial` key, no delete.
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let (key, _body) = &captured[0];
        assert_eq!(key, &format!("logs/aaaa/{exec_id}.partial.log.zst"));
        assert!(
            deletes.lock().unwrap().is_empty(),
            "periodic must not delete"
        );

        // ONE PG row with is_complete=false, status NULL.
        let row: (bool, Option<String>) =
            sqlx::query_as("SELECT is_complete, status FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert!(!row.0, "periodic flush is is_complete=false");
        assert!(row.1.is_none(), "periodic flush has no status");
        Ok(())
    }

    #[tokio::test]
    async fn periodic_then_final_upserts_same_row() -> anyhow::Result<()> {
        // The full periodic → final lifecycle: a snapshot row is created at
        // is_complete=false, the final flush UPSERTs it to is_complete=true,
        // swaps the s3_key from `.partial` to the canonical key, and stamps
        // status + finished_at. started_at is preserved from the first
        // INSERT (NOT in DO UPDATE SET).
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, _puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/aaaa-lifecycle.drv";
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"compiling"]);
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        );

        flusher.flush_periodic().await;
        let snap: (bool, String, f64) = sqlx::query_as(
            "SELECT is_complete, s3_key, EXTRACT(EPOCH FROM started_at)::float8 \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(!snap.0);
        assert!(snap.1.ends_with(".partial.log.zst"));

        // More lines arrive, then the build finishes.
        buffers.push(&mk_batch(drv_path, 1, &[b"linking", b"done"]));
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                status: Some("succeeded".into()),
            })
            .await;

        let fin: (bool, String, Option<String>, i64, f64, Option<f64>) = sqlx::query_as(
            "SELECT is_complete, s3_key, status, line_count, \
                    EXTRACT(EPOCH FROM started_at)::float8, \
                    EXTRACT(EPOCH FROM finished_at)::float8 \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(fin.0, "final flips is_complete");
        assert!(fin.1.ends_with(".log.zst") && !fin.1.contains(".partial"));
        assert_eq!(fin.2.as_deref(), Some("succeeded"));
        assert_eq!(fin.3, 3, "line_count from the final drain");
        // started_at preserved across the UPSERT (not overwritten).
        assert!(
            (fin.4 - snap.2).abs() < 1e-3,
            "started_at preserved: {} vs {}",
            fin.4,
            snap.2
        );
        assert!(fin.5.is_some(), "finished_at stamped on final");

        // Still exactly one row.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 1);
        Ok(())
    }

    #[tokio::test]
    async fn started_at_decoded_from_uuidv7_timestamp() -> anyhow::Result<()> {
        // started_at must come from the UUIDv7's embedded dispatch
        // timestamp, NOT a fresh clock read at flush time. Construct a
        // UUIDv7 with a known epoch and assert the PG column matches.
        let db = TestDb::new(&crate::MIGRATOR).await;
        // 2024-01-01 00:00:00 UTC = 1704067200.
        let known_epoch_ms: u64 = 1_704_067_200_000;
        let ts = uuid::Timestamp::from_unix_time(known_epoch_ms / 1000, 0, 0, 0);
        let exec_id = Uuid::new_v7(ts);

        upsert_drv_log(&db.pool, exec_id, "h", "k", 0, 1, 5, false, None).await?;

        let row: (f64,) = sqlx::query_as(
            "SELECT EXTRACT(EPOCH FROM started_at)::float8 FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        // UUIDv7 has ~ms precision. Accept up to 1s drift to stay
        // robust to PG's TIMESTAMPTZ μs rounding.
        assert!(
            (row.0 - known_epoch_ms as f64 / 1000.0).abs() < 1.0,
            "started_at {} should be ≈ {}",
            row.0,
            known_epoch_ms as f64 / 1000.0,
        );
        Ok(())
    }

    #[tokio::test]
    async fn s3_failure_logs_error_but_flusher_survives() -> anyhow::Result<()> {
        // S3 returns 500. Flusher must NOT panic or hang — it logs, increments
        // the failure metric, and returns. The NEXT flush_final for a different
        // drv should still work. (If the flusher died, all future logs lost.)
        let db = TestDb::new(&crate::MIGRATOR).await;

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
        let rule_del =
            mock!(S3Client::delete_object).then_output(|| DeleteObjectOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::Sequential,
            &[&rule_fail, &rule_ok, &rule_del]
        );

        let buffers = Arc::new(LogBuffers::new());
        // Keys with no `-` so `drv_log_hash` leaves them distinct.
        let exec_fail = stamp_and_push(&buffers, "drvfail", &[b"will-be-lost"]);
        let exec_ok = stamp_and_push(&buffers, "drvok", &[b"will-survive"]);
        let _ = exec_fail; // S3 PUT fails; never lands in PG.

        let flusher = LogFlusher::new(
            client,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
        );

        // First flush → S3 fails. Buffer is drained but upload fails → log
        // is lost. NOT a panic.
        flusher
            .flush_final(FlushRequest {
                drv_path: "drvfail".into(),
                status: Some("failed".into()),
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
                status: Some("succeeded".into()),
            })
            .await;

        // Only the second PG row exists (first flush failed before PG insert).
        let rows: Vec<(Uuid,)> = sqlx::query_as("SELECT exec_id FROM drv_logs")
            .fetch_all(&db.pool)
            .await?;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, exec_ok);
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
        let _ = stamp_and_push(
            &buffers,
            "/nix/store/pppppppppppppppppppppppppppppppp-periodic.drv",
            &[b"still-running"],
        );
        let flusher = LogFlusher::new(
            client,
            "test-bucket".into(),
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
    ///
    /// The observable is the per-tick "skipping flush: no exec_id" warn
    /// rather than S3 PUTs: a stamped (`set_exec`'d) entry would trigger
    /// the new `drv_logs` UPSERT, and PG socket I/O does not coexist
    /// with `tokio::time::pause()` auto-advance — the PG await has its
    /// own `tokio::time` timers that never fire under paused time, so
    /// the flusher's loop blocks on the first PG round-trip and the
    /// second tick never fires (the same class of issue the "TestDb
    /// before pause()" guard already documents). An *unstamped* entry
    /// makes `upload_and_record` skip before any I/O — pure CPU per
    /// tick, paused-time-safe — and still proves the tick wiring.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn periodic_tick_fires_in_spawn_loop() -> anyhow::Result<()> {
        // TestDb before pause(): sqlx pool acquire uses tokio::time
        // for its timeout, which PoolTimedOuts under paused time.
        let db = TestDb::new(&crate::MIGRATOR).await;
        tokio::time::pause();

        let (s3, _puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        // Legacy push() — entry exists, no exec_id. The flusher visits
        // it every tick, warns, and skips before any I/O.
        buffers.push(&mk_batch("/nix/store/ongoing-llvm.drv", 0, &[b"compiling"]));

        let (tx, rx) = mpsc::channel::<FlushRequest>(8);
        LogFlusher::new(s3, "b".into(), db.pool.clone(), Arc::clone(&buffers)).spawn(rx);

        // Auto-advance drives the interval; tx open so the loop is
        // recv-vs-tick (not the channel-close final sweep).
        tokio::time::sleep(Duration::from_secs(65)).await;

        // Two ticks in 65s (T=30, T=60) → ≥2 per-tick warns. Counting
        // warn lines proves the tick fired; a stamped entry would
        // hit the PG path which hangs under paused time (see doc above).
        logs_assert(|lines: &[&str]| {
            let n = lines
                .iter()
                .filter(|l| l.contains("skipping flush: no exec_id"))
                .count();
            if n >= 2 {
                Ok(())
            } else {
                Err(format!(
                    "two ticks in 65s → ≥2 per-tick skip warns, got {n}"
                ))
            }
        });
        drop(tx);
        Ok(())
    }
}
