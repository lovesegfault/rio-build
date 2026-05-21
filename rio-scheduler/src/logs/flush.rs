//! Async S3 flush of log ring buffers.
//!
//! The flusher runs on its own task, driven by two triggers:
//!   1. **Completion** — actor `try_send`s a [`FlushRequest`] when a
//!      derivation hits a terminal state with its log buffer still held
//!      by this leader (success, permanent failure, or build-level
//!      cancellation — see the scheduler actor's `terminal_log_epilogue`
//!      for the caller list).
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
//! (after `TERMINAL_CLEANUP_DELAY`, ~60s) reaps the DAG node and
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

/// How often to snapshot active buffers to S3. Per `observability.typ`:
/// bounds log loss on crash to ≤30s; lower = more S3 PUTs + CPU.
const PERIODIC_FLUSH_INTERVAL: Duration = Duration::from_secs(30);

/// How often the TTL GC sweep runs. ~1h matches `tick_sweep_event_log`'s
/// cadence (the actor's other periodic PG sweep). The sweep is one
/// `select!` arm — flushes are SERIALIZED against it, not interleaved
/// (a `select!` doesn't re-poll its other arms until the awaited future
/// returns; `yield_now()` only yields to sibling *tasks*). The delay is
/// bounded: [`LOG_GC_BATCH`]-row passes, each ~one PG round-trip + two
/// S3 `DeleteObjects`, and at a 1h cadence + 30d TTL the steady-state
/// pass count is ~1. Final flushes queue in `flush_rx` (1000-deep);
/// periodic ticks accumulate one `MissedTickBehavior::Skip` and recover.
const LOG_GC_INTERVAL: Duration = Duration::from_secs(3600);

/// Rows deleted per GC pass. Matches `tick_gc_orphan_derivations`'s
/// batch cap and aligns with S3 `DeleteObjects`' 1000-key limit (each
/// row produces 2 keys — `.log.zst` + `.partial.log.zst` — so a batch
/// of 1000 rows splits into 2 `DeleteObjects` calls).
const LOG_GC_BATCH: i64 = 1000;

/// Request to flush one derivation's logs. Sent by the actor from
/// `terminal_log_epilogue` (see its doc for the caller list — success,
/// permanent failure, and build-level cancellation all flush; failed
/// builds still have useful logs).
#[derive(Debug)]
pub struct FlushRequest {
    /// The buffer key — full `/nix/store/{hash}-{name}.drv` path. Also the
    /// source for the S3 key `logs/{drv_hash}/{exec_id}.log.zst` and PG
    /// `drv_logs.drv_hash` column via [`drv_log_hash`].
    pub drv_path: String,
    /// The execution this request is FOR. The actor reads
    /// `state.exec_id` at terminal time and pins it here so a stale
    /// request can't drain a re-dispatched execution's buffer.
    ///
    /// `flush_final` compares this against the live ring-buffer entry's
    /// `exec_id`. Mismatch ⇒ a re-dispatch (`discard` + `set_exec` with a
    /// new UUIDv7) happened between queuing and processing — the request
    /// is stale and is dropped without draining. Without the pin,
    /// `flush_final` would `drain()` the *current* exec's buffer with the
    /// *stale* exec's status, then `push_for` from the live executor
    /// would hit `no_assignment` (entry gone) and the whole log is lost.
    pub exec_id: Uuid,
    /// Build outcome for `drv_logs.status` (`"succeeded"` / `"failed"` /
    /// `"cancelled"`). `None` for periodic snapshots (build still running).
    /// Written on every final flush but not read by any production path yet —
    /// `DerivationLogChunk` has no `status` field, so `rio-cli logs` and the
    /// dashboard cannot surface it without a proto change. Available for ops
    /// queries.
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
    /// TTL for `drv_logs` rows + S3 blobs. Validated > 0 at config load
    /// (`Config::validate`). See [`Self::sweep_expired_logs`].
    log_retention_days: u32,
    /// Leader gate. Periodic snapshots and the GC sweep no-op on
    /// standbys and ex-leaders. Pre-`drv_logs` the periodic flush wrote
    /// no PG rows so it was harmless to run unconditionally; now both
    /// the periodic UPSERT and the GC DELETE+DeleteObjects are
    /// leader-only writes.
    ///
    /// The completion-flush arm stays un-gated, and the reason is
    /// narrower than it looks: everything in `flush_rx` was enqueued by
    /// the actor *while it held the lease*, for a derivation it fully
    /// observed reaching terminal. Processing that request post-flap
    /// still produces a correct `drv_logs` row — the data (the worker's
    /// lines, the actor's status) is accurate, the UPSERT is keyed on
    /// the request's pinned `exec_id`, and the new leader loads that
    /// derivation as already-terminal so it never re-dispatches it under
    /// that `exec_id` and never writes a competing row. A lease flap
    /// *during* a build enqueues nothing (no terminal happened here).
    ///
    /// Note the staleness guard in [`Self::flush_final`] is NOT what
    /// makes this safe: it is a *re-dispatch* cutoff (fires only when
    /// the ring-buffer entry was discarded and re-stamped with a fresh
    /// `exec_id`), not a "the lease moved on" cutoff. After a flap the
    /// ex-leader's `LogBuffers` is retained (`clear_persisted_state`
    /// wipes actor state on lease *acquisition* and explicitly classes
    /// `log_buffers` as retained) and still carries the same `exec_id`
    /// the request pinned, so the guard passes and the request is
    /// processed — correctly, for the reason above.
    is_leader: Arc<std::sync::atomic::AtomicBool>,
}

impl LogFlusher {
    pub fn new(
        s3: S3Client,
        bucket: String,
        pool: PgPool,
        buffers: Arc<LogBuffers>,
        log_retention_days: u32,
        is_leader: Arc<std::sync::atomic::AtomicBool>,
    ) -> Self {
        Self {
            s3,
            bucket,
            pool,
            buffers,
            log_retention_days,
            is_leader,
        }
    }

    /// Whether this replica currently holds the scheduler lease.
    /// Relaxed is sufficient: this is a periodic poll (every 30s/1h),
    /// not a synchronization point — a one-tick window where an
    /// ex-leader's flusher hasn't observed the flip is bounded and
    /// harmless (the GC DELETE is idempotent; a stale periodic UPSERT
    /// is overwritten by the new leader's next snapshot).
    fn is_leader(&self) -> bool {
        self.is_leader.load(std::sync::atomic::Ordering::Relaxed)
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

            // GC tick: lives here, not in the actor's housekeeping, because
            // it needs both PG (DELETE) and S3 (DeleteObjects) — and the only
            // S3Client in rio-scheduler is on LogFlusher. The actor's
            // `tick_sweep_event_log` / `tick_gc_orphan_derivations` are
            // PG-only. Same skip-behind + consume-first treatment as the
            // periodic tick.
            let mut gc_tick = tokio::time::interval(LOG_GC_INTERVAL);
            gc_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            gc_tick.tick().await;

            info!(
                bucket = %self.bucket,
                interval = ?PERIODIC_FLUSH_INTERVAL,
                gc_interval = ?LOG_GC_INTERVAL,
                retention_days = self.log_retention_days,
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
                        // Leader-gated, and the gate is load-bearing here (not
                        // just waste avoidance). A standby's `LogBuffers` is
                        // structurally empty (no worker streams connect to it),
                        // but an ex-leader after a lease flap retains its
                        // stamped buffers — and for drvs that were still
                        // Assigned|Running at failover, recovery deliberately
                        // re-stamps the *same* `exec_id` from `assignments`
                        // (so a reconnecting worker keeps streaming under its
                        // in-flight execution). An un-gated ex-leader periodic
                        // tick would therefore UPSERT a stale `.partial`
                        // snapshot into the same `(exec_id)`-keyed `drv_logs`
                        // row the new leader is writing — and could flip
                        // `is_complete` back to `false` over the new leader's
                        // completed final flush.
                        if self.is_leader() {
                            self.flush_periodic().await;
                        }
                    }

                    _ = gc_tick.tick() => {
                        // Leader-gated. The DELETE is idempotent so a redundant
                        // sweep on a standby is wasted PG/S3 traffic, not
                        // corruption — but with `replicas: 2` (chart default)
                        // it doubles every sweep for nothing.
                        if self.is_leader() {
                            self.sweep_expired_logs().await;
                        }
                    }
                }
            }

            info!("log flusher exited");
        });
    }

    /// On-completion flush: drain the buffer (derivation is done, no more
    /// writes coming) and upload with `is_complete=true`.
    async fn flush_final(&self, req: FlushRequest) {
        // Stale-request guard, atomic with the drain. The flusher's mpsc
        // is 1000-deep and a GC sweep or S3 burst can let requests queue
        // for seconds. In that window the actor can re-dispatch the same
        // drv (`discard` + `set_exec` with a fresh UUIDv7). Draining the
        // re-stamped entry with the stale request's status would (a)
        // record exec₂'s lines with exec₁'s outcome, marked
        // `is_complete=true`, and (b) leave `push_for` from exec₂'s
        // worker with no entry to land in — the entire re-dispatched
        // execution's log is silently lost. `drain_if_exec` runs the
        // exec_id comparison and the removal under one shard lock, so a
        // successful drain is guaranteed to have removed the entry this
        // request was pinned to. (The previous read-compare-drain shape
        // had a TOCTOU: a re-dispatch landing between the read and the
        // unconditional `drain()` removed the *freshly stamped* entry —
        // bug_004, round 8.)
        //
        // `Some` → this request's execution was the live one and its
        // buffer is now drained. Unseal: the seal bridged
        // completion→drain and that window is closed. Clearing it here
        // keeps `sealed` bounded even if the recv task is still running
        // (or never saw a LogBatch — silent build).
        if let Some((first_line, line_count, raw_bytes, lines)) =
            self.buffers.drain_if_exec(&req.drv_path, req.exec_id)
        {
            self.buffers.unseal(&req.drv_path);
            self.upload_and_record(req, first_line, line_count, raw_bytes, lines, true)
                .await;
            return;
        }

        // `None` → the entry is gone, was never stamped, or is stamped
        // with a different (newer) execution. The follow-up `exec_id()`
        // read below is *advisory only* — it picks the log line and
        // decides a defensive no-op unseal. It is racy with concurrent
        // discard/re-dispatch, and that is fine: the data-loss-critical
        // decision (whether to remove the entry) was already made
        // atomically above, and both arms below leave the buffers map
        // untouched.
        //
        // The `unseal()` is per-arm, NOT unconditional: the seal is keyed
        // by `drv_log_hash(drv_path)` only, with no per-execution
        // dimension, so in the `Some(live)` arm an unseal here would
        // remove the seal that the *live* execution's terminal handler
        // just set — re-opening `push_for` to post-terminal batches from
        // the live executor until the live request drains and re-unseals.
        // Leave the live exec's seal alone; its own `flush_final` clears
        // it after its drain.
        //
        // The `None` arm has no live owner of the seal (every re-dispatch
        // path calls `discard()`, which removes both entry and seal — the
        // only way to reach `None` with a leftover seal is a dup request,
        // and the first request's post-drain unseal already cleared it).
        // Keeping the unseal there is a free defensive bound; it's a
        // no-op in steady state. `CleanupTerminalBuild` is the actual
        // backstop if a `FlushRequest` was dropped (`try_send` full).
        match self.buffers.exec_id(&req.drv_path) {
            Some(live) => {
                warn!(
                    drv = %req.drv_path,
                    requested_exec = %req.exec_id,
                    live_exec = %live,
                    "dropping stale flush request: drv was re-dispatched"
                );
                metrics::counter!("rio_scheduler_log_flush_stale_total").increment(1);
            }
            None => {
                self.buffers.unseal(&req.drv_path);
                debug!(
                    drv_path = %req.drv_path,
                    requested_exec = %req.exec_id,
                    "no buffer to flush (silent build, dup request, or unstamped entry)"
                );
            }
        }
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
            // Periodic reads exec_id from the live entry — there is no
            // queued request to go stale, and the snapshot below is taken
            // under the same scan, so this is the same execution. (The
            // staleness guard is for `flush_final`, where the request can
            // sit in the mpsc across a re-dispatch.)
            let Some(exec_id) = self.buffers.exec_id(&drv_path) else {
                // Unstamped entry (legacy `push()` test fixture, or a
                // recovery gap). Nothing to key on — skip. `set_exec` is
                // always called at `assign_to_worker` and re-stamped by
                // recovery for active assignments, so this is never hit
                // in production; warn so the gap is visible if it is.
                warn!(
                    drv = %drv_path,
                    "skipping flush: no exec_id (set_exec never called — recovery gap or test artifact)"
                );
                continue;
            };
            let Some((first_line, line_count, raw_bytes, lines)) = self.buffers.snapshot(&drv_path)
            else {
                // Buffer vanished between active_keys() and snapshot() —
                // drained by a concurrent flush_final. Fine, skip.
                continue;
            };

            let req = FlushRequest {
                drv_path,
                exec_id,
                // Build still running — outcome unknown. Stays NULL in
                // PG until the final flush sets it.
                status: None,
            };
            self.upload_and_record(req, first_line, line_count, raw_bytes, lines, false)
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
    /// `req.exec_id` keys the S3 blob and PG row. The actor pins it at
    /// terminal time for finals; the periodic flusher reads it from the
    /// live entry (no staleness window). Both callers verify it before
    /// constructing the request — there is no `None` case here. `set_exec`
    /// is always called at `assign_to_worker` and re-stamped by recovery
    /// for active assignments.
    #[allow(clippy::too_many_arguments)] // call-site-local; threading a struct would just rename the args
    async fn upload_and_record(
        &self,
        req: FlushRequest,
        first_line: u64,
        line_count: u64,
        raw_bytes: u64,
        lines: Vec<Vec<u8>>,
        is_final: bool,
    ) {
        let exec_id = req.exec_id;
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

    /// TTL-based log GC: delete `drv_logs` rows (and their S3 blobs)
    /// older than [`Self::log_retention_days`]. **One TTL, no
    /// `is_complete` discriminator** — the age filter already excludes
    /// active builds (no build runs 30 days; the daemon timeout is ~2h),
    /// and a 30-day-old `is_complete=false` row is exactly as expired as
    /// a complete one — it indicates a flusher crash mid-write or a
    /// scheduler hard-kill, and the diagnostic value of a month-old
    /// truncated log is nil.
    ///
    /// **Why on `LogFlusher`, not the actor's housekeeping.** The sweep
    /// needs both PG (DELETE rows) and S3 (DeleteObjects). The actor's
    /// `tick_sweep_event_log` / `tick_gc_orphan_derivations` are PG-only;
    /// `DagActor` has no `S3Client`. The only `S3Client` in rio-scheduler
    /// lives here. The flusher also already runs on a dedicated tokio
    /// task off the actor's hot path.
    ///
    /// **Bounded delay, not interleaving.** The sweep is one `select!`
    /// arm alongside the periodic and final flushes — they are
    /// SERIALIZED against it, not interleaved. `tokio::select!` does
    /// NOT re-poll its other arms until the awaited future returns; the
    /// `yield_now()` between batches is cooperative scheduling for
    /// *sibling tasks* on the worker thread (admin gRPC, the actor),
    /// not arm fairness. While the sweep runs:
    ///   - final flushes queue in `flush_rx` (1000-deep, won't drop
    ///     unless the actor outruns the channel for the whole pass);
    ///   - periodic ticks accumulate one `MissedTickBehavior::Skip`
    ///     and recover on the next interval.
    ///
    /// The serialization is deliberate, not accepted: a `tokio::spawn`'d
    /// sweep could race a flush UPSERT against the GC DELETE on the same
    /// `drv_logs` row (a drv whose retention window expires mid-flush).
    /// The delay is bounded by `(expired_rows / LOG_GC_BATCH) × (PG
    /// round-trip + 2 S3 DeleteObjects)` — at a 1h cadence + 30d TTL the
    /// steady-state pass count is ~1, so a few seconds at worst.
    ///
    /// **`.partial` orphans.** Both `.log.zst` and `.partial.log.zst`
    /// keys are deleted per row, so a `.partial` blob orphaned by a
    /// failed best-effort delete during the final flush is caught at
    /// expiry. There is **no** separate fast-orphan sweep — that would
    /// need a watermark column to avoid re-sweeping `O(table)` rows per
    /// tick, and a `.partial` orphan is at most `log_retention_days`
    /// stale (acceptable residual leak; an S3 lifecycle rule on the
    /// `logs/` prefix can backstop it without code).
    ///
    /// **Failure modes.** PG DELETE failure aborts the pass (logged,
    /// retried next tick). S3 DeleteObjects failure is logged and the
    /// pass continues — the PG rows are already gone, and re-running the
    /// query won't re-find them, so the orphan blobs are unreachable
    /// from PG. They're bounded by the same lifecycle-rule backstop.
    async fn sweep_expired_logs(&self) {
        let mut total_swept = 0u64;
        loop {
            // Batch-bounded DELETE. The `IN (SELECT ... LIMIT N)`
            // sub-select is the standard Postgres idiom for limiting
            // a DELETE — DELETE itself has no LIMIT clause. RETURNING
            // gives us `drv_hash`/`exec_id` so we can construct the S3
            // keys directly (rather than parsing the row's `s3_key`,
            // which carries only ONE of `.log.zst` / `.partial.log.zst`
            // depending on whether the final flush ran). No ORDER BY:
            // any 1000 expired rows are equally good to delete. The
            // inner SELECT rides the `drv_logs_started_at` index so a
            // sub-LIMIT pass (including the 0-row terminal pass that
            // breaks the loop) stops at the first non-expired index
            // entry instead of seq-scanning the heap.
            let expired: Vec<(Uuid, String)> = match sqlx::query_as(
                "DELETE FROM drv_logs
                 WHERE exec_id IN (
                     SELECT exec_id FROM drv_logs
                     WHERE started_at < now() - $1 * interval '1 day'
                     LIMIT $2
                 )
                 RETURNING exec_id, drv_hash",
            )
            .bind(i64::from(self.log_retention_days))
            .bind(LOG_GC_BATCH)
            .fetch_all(&self.pool)
            .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    warn!(error = %e, "log GC sweep: PG DELETE failed (retried next tick)");
                    return;
                }
            };
            if expired.is_empty() {
                break;
            }

            // S3 batch delete: 2 keys per row (final + partial). LOG_GC_BATCH
            // rows × 2 keys = up to 2000 keys; chunk at 1000 to fit the S3
            // DeleteObjects limit. `quiet(true)` suppresses per-key success
            // entries in the response (we only care about errors). Use
            // `log_s3_key` — `drv_log_hash` is idempotent so passing the
            // already-normalized `drv_hash` produces the same key the flush
            // path wrote.
            let keys: Vec<aws_sdk_s3::types::ObjectIdentifier> = expired
                .iter()
                .flat_map(|(exec_id, drv_hash)| {
                    [
                        log_s3_key(drv_hash, exec_id, false),
                        log_s3_key(drv_hash, exec_id, true),
                    ]
                })
                .map(|key| {
                    aws_sdk_s3::types::ObjectIdentifier::builder()
                        .key(key)
                        .build()
                        // `key` is the only required field and is always
                        // set above — the builder cannot fail.
                        .expect("ObjectIdentifier: key is set")
                })
                .collect();
            for chunk in keys.chunks(1000) {
                let delete = aws_sdk_s3::types::Delete::builder()
                    .set_objects(Some(chunk.to_vec()))
                    .quiet(true)
                    .build()
                    .expect("Delete: objects is set");
                if let Err(e) = self
                    .s3
                    .delete_objects()
                    .bucket(&self.bucket)
                    .delete(delete)
                    .send()
                    .await
                {
                    // PG rows already gone — these blobs are unreachable
                    // from PG, so they won't be re-tried. Bounded by the
                    // S3 lifecycle-rule backstop. Log at warn (operator
                    // should know S3 deletes are failing) but don't abort
                    // the pass — there may be more expired rows to sweep.
                    warn!(
                        error = %e,
                        keys = chunk.len(),
                        "log GC sweep: S3 DeleteObjects failed \
                         (orphan blobs; lifecycle rule is the backstop)"
                    );
                }
            }

            total_swept += expired.len() as u64;
            // Cooperative scheduling for sibling TASKS on the worker
            // thread, NOT select!-arm fairness — the other arms don't
            // run until this fn returns (see fn doc, "Bounded delay").
            tokio::task::yield_now().await;
        }
        if total_swept > 0 {
            info!(
                deleted = total_swept,
                retention_days = self.log_retention_days,
                "log GC sweep"
            );
        }
        // .increment(0) registers the time series on first call, so
        // rio_scheduler_log_gc_swept_total exists in Prometheus even when
        // nothing has expired yet — distinguishing "GC never ran" (no
        // series) from "GC ran, found nothing" (series at 0). Outside the
        // `if` deliberately.
        metrics::counter!("rio_scheduler_log_gc_swept_total").increment(total_swept);
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
    use aws_sdk_s3::operation::delete_objects::DeleteObjectsOutput;
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
        // Batch deletes (GC sweep) flatten into the same captured-deletes
        // Vec, so a test can assert on individual keys regardless of
        // whether the production code chose `delete_object` or
        // `delete_objects` for a given key.
        let dcap2 = Arc::clone(&deletes);
        let del_objects_rule = mock!(S3Client::delete_objects)
            .match_requests(move |req| {
                let mut sink = dcap2.lock().unwrap();
                for obj in req.delete().map(|d| d.objects()).unwrap_or_default() {
                    sink.push(obj.key().to_string());
                }
                true
            })
            .then_output(|| DeleteObjectsOutput::builder().build());
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&put_rule, &del_rule, &del_objects_rule]
        );
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

    /// Always-leader gate for tests. The leader-gated arms (periodic
    /// snapshot, GC sweep) only run when the gate is true; tests
    /// exercise them directly via `flush_periodic()` / `sweep_expired_logs()`
    /// rather than the spawned loop, so the gate only matters for the
    /// `spawn`-loop tests.
    fn always_leader() -> Arc<std::sync::atomic::AtomicBool> {
        Arc::new(std::sync::atomic::AtomicBool::new(true))
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
            30,
            always_leader(),
        );

        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
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

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            buffers,
            30,
            always_leader(),
        );

        flusher
            .flush_final(FlushRequest {
                drv_path: "nonexistent".into(),
                exec_id: Uuid::now_v7(),
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

    /// A `FlushRequest` queued for exec₁ that arrives after the drv was
    /// re-dispatched (and the buffer re-stamped with exec₂) must be
    /// dropped, NOT drained — otherwise the re-dispatched execution's
    /// in-progress lines would be uploaded under exec₂ with exec₁'s
    /// `status`, marked `is_complete=true`, and `push_for` from exec₂'s
    /// worker would land in nothing (entry gone). The whole re-dispatched
    /// log would be silently lost.
    /// r[verify obs.log.exec-keyed]
    #[tokio::test]
    async fn flush_final_stale_request_does_not_drain_redispatched_exec() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/aaaa-stale.drv";

        // exec₁ runs and terminates. The actor queues a FlushRequest pinned
        // to exec₁. Then the drv is re-dispatched (discard + set_exec exec₂).
        let exec1 = stamp_and_push(&buffers, drv_path, &[b"exec1-line"]);
        // ... actor would call trigger_log_flush here, queuing exec1 ...
        buffers.discard(drv_path); // assign_to_worker
        let exec2 = stamp_and_push(&buffers, drv_path, &[b"exec2-line0", b"exec2-line1"]);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // The stale exec₁ request arrives now.
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id: exec1,
                status: Some("failed".into()),
            })
            .await;

        // Stale request dropped: no S3 PUT, no PG row, exec₂'s buffer intact.
        assert!(puts.lock().unwrap().is_empty(), "stale flush must not PUT");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 0, "stale flush must not write a PG row");
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec2),
            "exec₂'s buffer must survive the stale flush"
        );
        assert_eq!(
            buffers.read_since(drv_path, 0).map(|v| v.len()),
            Some(2),
            "exec₂'s lines must survive"
        );

        // Now the legitimate exec₂ flush arrives and is processed normally.
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id: exec2,
                status: Some("succeeded".into()),
            })
            .await;
        let row: (Uuid, i64, Option<String>) =
            sqlx::query_as("SELECT exec_id, line_count, status FROM drv_logs")
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(row.0, exec2);
        assert_eq!(row.1, 2);
        assert_eq!(row.2.as_deref(), Some("succeeded"));
        Ok(())
    }

    /// A stale `FlushRequest` (exec₁) processed after the live execution
    /// (exec₂) reached terminal must NOT remove exec₂'s seal. The seal is
    /// keyed by drv path, not by exec_id; an unconditional `unseal()` on
    /// the stale-mismatch path would re-open `push_for` to post-terminal
    /// batches from exec₂'s worker during the window between the stale
    /// request's processing and exec₂'s own `flush_final`.
    /// r[verify sched.log.batch-binding]
    #[tokio::test]
    async fn flush_final_stale_request_preserves_live_exec_seal() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/aaaa-staleseal.drv";

        // exec₁ runs and terminates. Production order: terminal handler
        // seals BEFORE queuing the FlushRequest.
        let exec1 = stamp_and_push(&buffers, drv_path, &[b"exec1-line"]);
        buffers.seal(drv_path); // terminal_failure_epilogue: seal_log_buffer
        // ... actor would queue FlushRequest{D, exec1} here ...

        // Drv re-dispatched (poison-clear). discard removes exec₁'s entry
        // AND exec₁'s seal; set_exec stamps exec₂'s fresh entry.
        buffers.discard(drv_path); // assign_to_worker: discard_log_buffer
        let exec2 = stamp_and_push(&buffers, drv_path, &[b"exec2-line0", b"exec2-line1"]);

        // exec₂ reaches terminal. Production order: seal BEFORE queue.
        buffers.seal(drv_path); // handle_success_completion: seal_log_buffer
        assert!(
            buffers.is_sealed(drv_path),
            "exec₂'s terminal handler sealed"
        );
        // ... actor would queue FlushRequest{D, exec2} here ...

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // The flusher dequeues exec₁'s STALE request first (FIFO).
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id: exec1,
                status: Some("failed".into()),
            })
            .await;

        // The stale request must not remove exec₂'s seal: a late batch
        // from exec₂'s worker (still the assigned executor on the live
        // entry) must continue to be rejected until exec₂'s own
        // `flush_final` drains and clears the seal.
        assert!(
            buffers.is_sealed(drv_path),
            "stale flush_final must not unseal the live execution"
        );
        assert!(
            !buffers.push_for(drv_path, &mk_batch(drv_path, 2, &[b"late"]), "test-worker"),
            "post-terminal batch from the live exec's worker must stay sealed out"
        );
        assert!(puts.lock().unwrap().is_empty(), "stale flush must not PUT");

        // exec₂'s own request arrives. It drains, uploads, and unseals.
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id: exec2,
                status: Some("succeeded".into()),
            })
            .await;
        assert!(
            !buffers.is_sealed(drv_path),
            "live exec's own flush_final unseals after drain"
        );
        let row: (Uuid, i64, Option<String>) =
            sqlx::query_as("SELECT exec_id, line_count, status FROM drv_logs")
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(row.0, exec2);
        assert_eq!(row.1, 2, "the late batch must not have landed");
        assert_eq!(row.2.as_deref(), Some("succeeded"));
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
            30,
            always_leader(),
        );

        // Both paths should skip.
        flusher.flush_periodic().await;
        flusher
            .flush_final(FlushRequest {
                drv_path: "/nix/store/aaaa-nostamp.drv".into(),
                exec_id: Uuid::now_v7(),
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

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            buffers,
            30,
            always_leader(),
        );

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
            30,
            always_leader(),
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
            30,
            always_leader(),
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
                exec_id,
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
        // `exec_fail` is bound to the request below; the upload fails so it never lands in PG.

        let flusher = LogFlusher::new(
            client,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // First flush → S3 fails. Buffer is drained but upload fails → log
        // is lost. NOT a panic.
        flusher
            .flush_final(FlushRequest {
                drv_path: "drvfail".into(),
                exec_id: exec_fail,
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
                exec_id: exec_ok,
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
            30,
            always_leader(),
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
        LogFlusher::new(
            s3,
            "b".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        )
        .spawn(rx);

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

    /// Seed a `drv_logs` row directly with an arbitrary `started_at`.
    /// The production flush path always derives `started_at` from the
    /// UUIDv7's embedded timestamp, so the only way to test the GC
    /// cutoff is to bypass it. Returns `(exec_id, drv_hash)`.
    async fn seed_drv_log_aged(
        pool: &PgPool,
        age_days: i64,
        is_complete: bool,
    ) -> anyhow::Result<(Uuid, String)> {
        let exec_id = Uuid::now_v7();
        // 32-char fake hash distinct per call (the full 128-bit UUID
        // as hex). drv_log_hash() passes a 32-char hash-shaped string
        // through unchanged, so log_s3_key() produces a key that
        // matches what the GC sweep will reconstruct.
        let drv_hash = format!("{:032x}", exec_id.as_u128());
        sqlx::query(
            "INSERT INTO drv_logs
                 (exec_id, drv_hash, s3_key, first_line, line_count,
                  total_bytes, is_complete, status, started_at, finished_at)
             VALUES ($1, $2, $3, 0, 1, 1, $4, NULL,
                     now() - $5 * interval '1 day',
                     CASE WHEN $4 THEN now() - $5 * interval '1 day' ELSE NULL END)",
        )
        .bind(exec_id)
        .bind(&drv_hash)
        .bind(log_s3_key(&drv_hash, &exec_id, !is_complete))
        .bind(is_complete)
        .bind(age_days)
        .execute(pool)
        .await?;
        Ok((exec_id, drv_hash))
    }

    /// The GC sweep deletes expired rows + BOTH S3 blobs and keeps
    /// recent ones. The expired seed is deliberately `is_complete=false`
    /// to verify the "one TTL, no `is_complete` discriminator" rule —
    /// a 60-day-old `.partial`-only row (flusher crashed mid-write) is
    /// exactly as expired as a complete one.
    // r[verify obs.log.exec-keyed]
    #[tokio::test]
    async fn gc_sweep_deletes_expired_logs_and_keeps_recent() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Migration 061 must carry the started_at index — without it the
        // sweep's sub-LIMIT terminal pass seq-scans the full heap every
        // tick. See M_061 commentary; same pattern as
        // build_samples_completed_at_idx in db/tests/history.rs.
        let (idx_exists,): (bool,) = sqlx::query_as(
            "SELECT EXISTS(SELECT 1 FROM pg_indexes \
             WHERE tablename = 'drv_logs' AND indexname = 'drv_logs_started_at')",
        )
        .fetch_one(&db.pool)
        .await?;
        assert!(
            idx_exists,
            "drv_logs_started_at index missing — GC sweep degrades to seq scan"
        );
        let (s3, _puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30, // retention: 60d-old → expired, 0d-old → kept
            always_leader(),
        );

        // Expired (60d, incomplete — the no-discriminator case) and
        // recent (0d, complete). The recent row must survive the sweep.
        let (old_exec, old_hash) = seed_drv_log_aged(&db.pool, 60, false).await?;
        let (new_exec, _new_hash) = seed_drv_log_aged(&db.pool, 0, true).await?;

        flusher.sweep_expired_logs().await;

        // PG: expired row gone, recent row kept.
        let remaining: Vec<(Uuid,)> = sqlx::query_as("SELECT exec_id FROM drv_logs")
            .fetch_all(&db.pool)
            .await?;
        let remaining: Vec<Uuid> = remaining.into_iter().map(|(u,)| u).collect();
        assert!(
            !remaining.contains(&old_exec),
            "60d-old row must be swept (got {remaining:?})"
        );
        assert!(
            remaining.contains(&new_exec),
            "0d-old row must be kept (got {remaining:?})"
        );

        // S3: BOTH keys for the expired row deleted (the row's stored
        // `s3_key` was the `.partial` one; the sweep also issues a
        // delete for the final `.log.zst` it never wrote — S3
        // DeleteObjects on a nonexistent key is a no-op, and not having
        // to know which blobs exist is the point of always deleting
        // both). Nothing for the recent row.
        let dels: Vec<String> = deletes.lock().unwrap().clone();
        let want_final = log_s3_key(&old_hash, &old_exec, false);
        let want_partial = log_s3_key(&old_hash, &old_exec, true);
        assert!(
            dels.contains(&want_final),
            "expected S3 delete of {want_final}, got {dels:?}"
        );
        assert!(
            dels.contains(&want_partial),
            "expected S3 delete of {want_partial}, got {dels:?}"
        );
        assert_eq!(
            dels.len(),
            2,
            "no S3 deletes for the kept row, got {dels:?}"
        );
        Ok(())
    }

    /// Multi-batch backstop: more expired rows than `LOG_GC_BATCH` are
    /// swept across multiple passes (the loop keeps going until the
    /// DELETE returns 0 rows). Seeds `LOG_GC_BATCH + 1` rows — the
    /// smallest count that forces a second pass.
    #[tokio::test]
    async fn gc_sweep_drains_more_than_one_batch() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, _puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // LOG_GC_BATCH + 1 forces a second pass. Per-row INSERT round
        // trips would be slow at 1001 rows, so use a single
        // generate_series INSERT.
        let n = LOG_GC_BATCH + 1;
        sqlx::query(
            "INSERT INTO drv_logs
                 (exec_id, drv_hash, s3_key, first_line, line_count,
                  total_bytes, is_complete, status, started_at)
             SELECT
                 gen_random_uuid(),
                 lpad(i::text, 32, '0'),
                 'logs/' || lpad(i::text, 32, '0') || '/x.log.zst',
                 0, 1, 1, FALSE, NULL,
                 now() - interval '60 days'
             FROM generate_series(1, $1) AS i",
        )
        .bind(n)
        .execute(&db.pool)
        .await?;

        flusher.sweep_expired_logs().await;

        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count, 0, "all {n} expired rows swept across batches");
        // 2 keys per row.
        assert_eq!(
            deletes.lock().unwrap().len() as i64,
            n * 2,
            "2 S3 keys per swept row"
        );
        Ok(())
    }
}
