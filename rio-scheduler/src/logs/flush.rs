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
//!      (or swept by the TTL GC at expiry if that delete fails) — unless
//!      the recovered prefix could not be re-read at final-flush time
//!      (`FlushPayload::preserve_partial`), in which case it is left in
//!      place for operator recovery / TTL sweep.
//!
//! A third behavior kicks in on any later tenure of an execution after a
//! leader failover: either recovery restamps the same `exec_id` (onto a
//! fresh standby's empty buffer, or onto a re-acquired ex-leader's
//! retained one — even one whose ring still starts at line 0) and the
//! reconnecting worker only re-streams undelivered batches, or the
//! acquisition-time re-arm ([`LogBuffers::rearm_prefix_reconciliation`])
//! clears the per-tenure latch on a retained entry recovery did not
//! restamp (e.g. spared Ready after an interim leader's reset). Either
//! way the ring no longer necessarily covers everything a prior tenure
//! already flushed. On the first
//! non-empty flush of the tenure the flusher reconciles the ring against
//! the stored `drv_logs` row ([`LogFlusher::reconcile_stored_prefix`]):
//! when the ring does not contiguously subsume the stored range it fetches
//! the stored `.partial` once (cached on the ring-buffer entry as a
//! [`RecoveredPrefix`]), drops any ring lines the stored copy supersedes,
//! and folds the prefix into every subsequent flush of that execution so
//! the periodic overwrite and the final blob keep covering output recorded
//! by earlier tenures. A merged row's `first_line`/`line_count` stay in
//! true line-number space (the gap is counted, the marker is not extra) —
//! see `obs.log.gap-span`. An interior hole consisting of lines an interim
//! leader received but never flushed remains a silent (unmarked) gap —
//! unavoidable, and within the ≤30s periodic-flush budget; the row's span
//! still counts it, so a `since_line` past the hole is not told it is
//! caught up — the read path's physical-vs-claimed check re-serves the
//! blob from the start instead. The flusher's self-driven arms
//! additionally wait for the actor's acquisition-time recovery to
//! complete ([`LogFlusher::may_flush`]), so the first flush of a tenure
//! always runs after that re-arm.
//!
//! Both flush kinds write **one** `drv_logs` row per execution, UPSERTed on
//! `(exec_id)` — a periodic snapshot inserts the row at `is_complete=false`,
//! the final flush flips it to `is_complete=true` and stamps `finished_at`.
//! (Exception: a final whose drain yields zero lines — failover restamp, worker
//! never reconnected — stamps `status`/`finished_at` only and leaves
//! `is_complete=false`; see `finalize_empty_drain`.)
//! The `exec_id` (per-execution UUIDv7 minted by `assign_to_worker`) lives
//! on the `LogBuffers` ring-buffer entry; the flusher reads it from there
//! because it has no actor `FlushRequest` for the periodic path. A flush
//! with no `exec_id` (entry never `set_exec`'d — recovery gap or
//! test-construction artifact) is **dropped**, not written under a garbage
//! key.
//!
//! The flusher NEVER blocks the actor. It's mpsc-fed (`try_send`, bounded
//! channel); if the channel is full, the actor's completion flush is
//! dropped: the buffer stays in `LogBuffers.buffers` (sealed) and the next
//! periodic tick still snapshots it — so the content survives at the
//! `.partial` key with an `is_complete=false` PG row — and
//! `CleanupTerminalBuild` (after `TERMINAL_CLEANUP_DELAY`, ~60s) reaps the
//! DAG node and discards the buffer (`LogBuffers::discard`), bounding the
//! leak. A final whose `FlushRequest` *was* accepted is handled
//! differently: the epilogue marks the entry final-pending at enqueue,
//! terminal cleanup leaves marked entries to the flusher, and a deferral
//! (already-finalized guard could not read `drv_logs`) keeps the request
//! retained and retried on the periodic tick (and once at shutdown) while
//! the sealed entry stays in memory (`obs.log.deferred-final-retry`) —
//! unless the stored-coverage reconcile later empties that ring, in which
//! case the periodic sealed-empty reap may remove the entry first and the
//! retried final resolves via the no-entry arm; the
//! cleanup discard therefore only bounds buffers whose enqueue failed.
//! A request is only ever finalized by the leadership tenure that enqueued
//! it (`FlushRequest::lease_generation`): the tenure check runs before the
//! finalize guard's row consult (and is re-checked after the guard SELECT
//! and the stored-prefix reconcile — the awaits that precede any
//! destructive arm; the post-drain upload window is deliberately not
//! re-checked, see the pre-drain re-check's comment), so a request orphaned
//! by a leadership change is dropped without any PG or S3 work and uploads
//! nothing. Its ring entry is reaped only
//! while still sealed and stamped with that exec: an empty one outright
//! (nothing to lose — and since the read path probes the stored `.partial`
//! for an empty entry, the reap is memory/bookkeeping hygiene, not
//! read-path rescue); a non-empty one is left in place and reaped later by
//! the periodic flush — its snapshot UPSERT is refused by the frozen-row
//! latch once another tenure has finalized the execution (the durable
//! record supersedes the retained lines), or, when the stored-coverage
//! reconcile finds a prior tenure's row covering past the retained ring
//! and empties it, the sealed-empty reap at the empty-snapshot
//! early-return removes it (see `flush_final` / `upload_and_record`) —
//! and the live tenure's own terminal processing finalizes the execution.
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

use super::{LogBuffers, PrefixState, RecoveredPrefix, drv_log_hash, log_s3_key};
use crate::lease::LeaderState;

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

/// How many deferred final flushes (finalize guard could not read `drv_logs`)
/// the flusher retains for retry. Each retained request pins its sealed ring
/// entry in memory until PG answers (an orphaned-tenure request is dropped
/// even earlier by the per-attempt tenure pin, with no PG work at all; see
/// `FlushRequest::lease_generation`) (≤ RING_BYTE_CAP each), and — since the
/// final-pending mark is set at enqueue — entries whose finals are still
/// *queued* are pinned too (bounded by the flush channel's 1000-deep
/// backlog; an EMPTY entry is the exception — the periodic sealed-empty
/// reap may remove it before its final is processed, that final then
/// taking the no-entry arm), so the cap bounds the *retained* set, not the
/// whole pinned set, during a long PG outage. Deferrals beyond the cap
/// drop the execution's
/// buffered entry at overflow time (only while the overflowing request is
/// still in tenure — an out-of-tenure victim's entry may be the live
/// execution's restamped carrier and is left untouched; see
/// `retain_deferred`) — the same loss the pre-retry behavior
/// accepted at terminal cleanup, which can no longer be relied on because
/// the enqueue-time pending mark may already have made cleanup skip the
/// entry.
const DEFERRED_FINALS_MAX: usize = 64;

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
    /// The execution this request is FOR — resolved once by
    /// `terminal_log_epilogue` via `exec_id_for_terminal` and pinned
    /// here so a stale request can't drain a re-dispatched execution's
    /// buffer.
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
    /// Scheduler-lease generation (`LeaderState::generation()`) under which
    /// the actor enqueued this request (`trigger_log_flush`). `flush_final`
    /// refuses to FINALIZE under any other tenure: if this replica is no
    /// longer leader, or its generation has moved past this value, the
    /// request is dropped without uploading — after a leadership change the
    /// execution may still be live and being extended by the tenure that
    /// now owns it (recovery restamps the same exec_id), and a stale final
    /// would freeze that row and delete the `.partial` carrying the live
    /// coverage. The periodic snapshot path builds its request inline with
    /// the current generation; the field is not consulted there.
    pub lease_generation: u64,
}

/// One execution's flushable content: the ring snapshot/drain plus, after a
/// leader failover with a reconnecting worker, the recovered prefix fetched
/// from the prior leader's `.partial` blob.
struct FlushPayload {
    first_line: u64,
    /// Highest true worker line number present in `lines`. Equal to
    /// `first_line + line_count - 1` for a contiguous payload; larger when
    /// the ring carried an interior hole (lines delivered only to an interim
    /// leader that never flushed them). Meaningless when `line_count == 0`.
    last_line: u64,
    line_count: u64,
    raw_bytes: u64,
    lines: Vec<Vec<u8>>,
    recovered_prefix: Option<Arc<RecoveredPrefix>>,
    /// Final-flush-only escape hatch: a stored prefix is known to exist but
    /// could not be fetched — upload what we have but do NOT delete the
    /// `.partial` (it is the only copy of the prefix).
    preserve_partial: bool,
}

/// Outcome of [`LogFlusher::lookup_stored_prefix`].
enum StoredPrefixLookup {
    /// No stored content needs rescuing (no row, finalized row, or the ring
    /// contiguously subsumes the stored range). Safe to flush as-is.
    NotNeeded,
    /// The prior tenure's stored content was fetched.
    Found(Arc<RecoveredPrefix>),
    /// A qualifying row exists but it could not be read — do not overwrite.
    FetchFailed,
}

/// Outcome of [`LogFlusher::reconcile_stored_prefix`].
enum PrefixReconcile {
    /// Settled: the entry is now `Cached` (stored content must be folded
    /// into every flush; any superseded ring head has been dropped) or
    /// `Checked` (nothing stored needs rescuing).
    Reconciled,
    /// The ring holds no lines yet — nothing to evaluate. State stays
    /// `Unchecked` so the first real flush re-runs this.
    RingEmpty,
    /// A qualifying stored row exists but its blob could not be read.
    /// State stays `Unchecked`; the caller must not overwrite stored
    /// content this round.
    FetchFailed,
}

/// One execution's stored `drv_logs` row, as consulted by the flush path
/// before it trusts retained in-memory ring state. Shared by the
/// already-finalized refusal in [`LogFlusher::flush_final`] and the
/// recovered-prefix rescue ([`LogFlusher::lookup_stored_prefix`]).
struct StoredDrvLogRow {
    s3_key: String,
    first_line: u64,
    line_count: u64,
    total_bytes: u64,
    is_complete: bool,
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
    /// Leadership + acquisition-recovery gate for the self-driven arms
    /// (periodic snapshot, GC sweep, and the channel-close last-gasp
    /// sweep): they no-op unless this replica holds the lease AND the
    /// actor's acquisition-time recovery for this tenure has completed.
    /// Why `is_leader` alone is not enough is explained once, in
    /// [`Self::may_flush`].
    ///
    /// The completion-flush arm stays un-gated *at the arm level*:
    /// everything in `flush_rx` was enqueued by the actor *while it held
    /// the lease*, for a derivation it fully observed reaching terminal,
    /// and in the common continuous-tenure history processing it
    /// post-blip still produces a correct `drv_logs` row. But a request
    /// that is still queued or deferred when the lease moves is NOT safe
    /// to finalize later: it sat unprocessed because PG was unreachable —
    /// the same outage under which the cancel/terminal status persist
    /// fails — so the next tenure's recovery loads the drv as still
    /// Assigned/Running, restamps the SAME `exec_id`, and keeps extending
    /// that execution's non-finalized row and `.partial` while the worker
    /// keeps streaming. Per-request tenure validation therefore happens
    /// inside [`Self::flush_final`]: every request carries the lease
    /// generation at enqueue (`FlushRequest::lease_generation`) and is
    /// dropped — first attempt or retained retry alike — when this
    /// replica no longer holds the lease or its generation has moved on.
    ///
    /// Note the staleness guard in [`Self::flush_final`] is NOT what
    /// makes this safe: it is a *re-dispatch* cutoff (fires only when
    /// the ring-buffer entry was discarded and re-stamped with a fresh
    /// `exec_id`), not a "the lease moved on" cutoff. After a flap the
    /// ex-leader's `LogBuffers` is retained (`clear_persisted_state`
    /// wipes actor state on lease *acquisition* and explicitly classes
    /// `log_buffers` as retained) and still carries the same `exec_id`
    /// the request pinned, so that guard passes — the tenure pin is the
    /// check that catches the lease movement.
    leader: LeaderState,
}

impl LogFlusher {
    pub fn new(
        s3: S3Client,
        bucket: String,
        pool: PgPool,
        buffers: Arc<LogBuffers>,
        log_retention_days: u32,
        leader: LeaderState,
    ) -> Self {
        Self {
            s3,
            bucket,
            pool,
            buffers,
            log_retention_days,
            leader,
        }
    }

    /// Whether this replica may run a self-driven flush right now: it
    /// holds the scheduler lease AND the actor's acquisition-time
    /// recovery for this tenure has completed.
    ///
    /// `is_leader` alone is not enough. The lease loop stores
    /// `is_leader=true` synchronously at acquisition and only
    /// fire-and-forgets `LeaderAcquired`; the per-tenure prefix
    /// bookkeeping (`prefix_checked` / `recovered_prefix`) is re-armed
    /// only when the actor dequeues that command and runs
    /// `recover_from_pg` (re-arm, same-exec restamp, stale-entry
    /// sweep). A periodic tick in that gap on a re-acquired ex-leader
    /// would trust the PREVIOUS tenure's latch, skip
    /// `reconcile_stored_prefix`, and overwrite the `.partial` blob and
    /// non-finalized `drv_logs` row an interim leader extended — the
    /// UPSERT's conflict clause only freezes finalized rows.
    ///
    /// `recovery_complete` is cleared by `on_lose` and set strictly
    /// after the re-arm, even when the PG load fails — so the gate
    /// cannot wedge a degraded tenure. The guarantee: a self-driven
    /// flush only ever consults prefix latches that were re-armed at,
    /// or reconciled after, the most recent recovery — never a previous
    /// tenure's latch. (Not quite "never flush before this tenure's
    /// re-arm": a lease lost while recovery is running leaves the flag
    /// set through standby — see the TODO at the
    /// `set_recovery_complete` call site — so the next acquire's gap is
    /// open; but in exactly that history every retained entry is still
    /// Unchecked from that recovery's re-arm, so a gap flush reconciles
    /// stored coverage rather than overwriting it.) Mirrors
    /// `dispatch_ready`'s two-flag gate. Both accessors are SeqCst, so
    /// observing `recovery_complete=true` also observes the re-arm's
    /// latch clears. The GC sweep needs no latch but inherits the gate
    /// harmlessly (hourly cadence); the completion-flush arm stays
    /// un-gated (see the `leader` field doc — per-request tenure
    /// validation happens inside `flush_final`).
    // r[impl obs.log.stored-coverage-preserved]
    fn may_flush(&self) -> bool {
        self.leader.is_leader() && self.leader.recovery_complete()
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
            // Deferred final flushes retained for retry (finalize guard could
            // not read drv_logs). Bounded by DEFERRED_FINALS_MAX; retried on
            // each periodic tick and once at shutdown; requests from a
            // previous leadership tenure are dropped by `flush_final` when
            // attempted.
            let mut deferred: Vec<FlushRequest> = Vec::new();
            loop {
                tokio::select! {
                    maybe = flush_rx.recv() => {
                        match maybe {
                            Some(req) => {
                                if let Some(d) = self.flush_final(req).await {
                                    self.retain_deferred(&mut deferred, d);
                                }
                            }
                            None => {
                                // Actor died. One last retry of any deferred
                                // finals (PG may have recovered since the last
                                // tick), then the existing leader-gated sweep.
                                self.retry_deferred(&mut deferred).await;
                                if !deferred.is_empty() {
                                    warn!(
                                        pending = deferred.len(),
                                        "exiting with deferred final flushes still \
                                         unresolved; their buffered content is lost \
                                         with process memory"
                                    );
                                }
                                // No more completions coming. One last periodic
                                // sweep to save whatever's in the buffers, then
                                // exit.
                                //
                                // Gated for the same reason as the
                                // periodic-tick arm below: an ex-leader's
                                // retained, re-stamped buffers would UPSERT a
                                // stale `.partial` over the new leader's row
                                // for the same `exec_id`. The gate does NOT
                                // cost a current leader its last-gasp sweep:
                                // graceful release calls `step_down()` only —
                                // `on_lose()` (which clears `is_leader`)
                                // fires on losing the lease to a peer or on
                                // self-fence, never on plain shutdown — so a
                                // still-leading replica passes. A
                                // never-leader standby's buffers are
                                // structurally empty either way. A leader
                                // shutting down before its recovery completed
                                // skips the sweep too (see `may_flush()`): its
                                // retained buffers are exactly the stale-latch
                                // hazard, and the lines it abandons are those
                                // pushed since this acquisition (≤ recovery
                                // duration) plus pre-flap lines it never
                                // flushed — already inside the flap's accepted
                                // ≤30s loss budget when leadership moved.
                                if self.may_flush() {
                                    debug!("flush channel closed; final periodic sweep then exit");
                                    self.flush_periodic().await;
                                } else {
                                    debug!(
                                        "flush channel closed; skipping final sweep (not leader \
                                         or recovery pending)"
                                    );
                                }
                                break;
                            }
                        }
                    }

                    _ = tick.tick() => {
                        // Deferred-final retries are re-attempted here
                        // regardless of leadership; per-request tenure
                        // validation (and the drop of requests orphaned by a
                        // leadership change) happens inside `flush_final`.
                        // The snapshot sweep below keeps its `may_flush`
                        // gate.
                        // r[impl obs.log.deferred-final-retry+3]
                        self.retry_deferred(&mut deferred).await;
                        // Gated. A standby's `LogBuffers` is
                        // structurally empty (no worker streams connect to it),
                        // but an ex-leader after a lease flap retains its
                        // stamped buffers — and for drvs that were still
                        // Assigned|Running at failover, recovery deliberately
                        // re-stamps the *same* `exec_id` from `assignments`
                        // (so a reconnecting worker keeps streaming under its
                        // in-flight execution). An un-gated ex-leader periodic
                        // tick would therefore PUT stale `.partial` blobs and
                        // churn the same `(exec_id)`-keyed `drv_logs` rows the
                        // new leader is writing. For FINALIZED rows the UPSERT
                        // conflict clause is the backstop; for a non-finalized
                        // row an interim leader extended, the
                        // recovery_complete half of the gate is the barrier —
                        // see `may_flush()`.
                        if self.may_flush() {
                            self.flush_periodic().await;
                        } else if self.leader.is_leader() {
                            // Leadership held but this tenure's recovery (and
                            // its prefix re-arm) hasn't finished — defer rather
                            // than trust latches from a previous tenure; see
                            // `may_flush()`. Standbys stay silent
                            // (is_leader=false is a replica's steady state).
                            debug!(
                                "periodic flush deferred: leadership acquired but recovery \
                                 not yet complete this tenure"
                            );
                        }
                    }

                    _ = gc_tick.tick() => {
                        // Leader-gated (and recovery-gated — inherited from
                        // may_flush; an hour-cadence sweep losing a few seconds
                        // is irrelevant). The DELETE is idempotent so a
                        // redundant sweep on a standby is wasted PG/S3 traffic,
                        // not corruption — but with `replicas: 2` (chart
                        // default) it doubles every sweep for nothing.
                        if self.may_flush() {
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
    ///
    /// Validates the request against its enqueueing lease tenure FIRST: an
    /// out-of-tenure request is dropped before any PG or S3 work and uploads
    /// nothing. Its entry is reaped only when that entry is still sealed,
    /// stamped with the request's exec, AND empty (anything else may be the
    /// live execution's restamped buffer); a sealed non-empty orphan is left
    /// in place — its reaper is the periodic flush: the refused-UPSERT reap
    /// once another tenure has finalized the execution, or the sealed-empty
    /// reap at the empty-snapshot early-return once the stored-coverage
    /// reconcile has emptied its ring (see
    /// [`Self::upload_and_record`]). The tenure is re-checked after every
    /// awaited step (guard SELECT, reconcile) before any destructive arm
    /// runs; a request that goes stale during the guard SELECT in the
    /// already-finalized arm reaps the sealed residue (empty or not) using
    /// the finalized row already in hand. An
    /// in-tenure request then consults the execution's stored `drv_logs`
    /// row: an execution another leader already finalized is refused
    /// (residue reaped if still stamped with it, nothing uploaded), and a
    /// row that cannot be read defers the flush entirely (fail closed — the
    /// request is retained for retry; see the `Err` arm below and
    /// [`Self::retry_deferred`]).
    ///
    /// Returns `Some(req)` when the final was deferred because the finalize
    /// guard could not read `drv_logs` and the request should be retried
    /// while the (non-empty) sealed entry is still held — the spawn loop
    /// retains it and re-runs it on the periodic tick. `None` in every other
    /// case (uploaded, refused, stale, dropped because the enqueueing lease
    /// tenure ended, no-op, or deferred with nothing left to retry).
    async fn flush_final(&self, req: FlushRequest) -> Option<FlushRequest> {
        // Tenure pin: a final flush request — first attempt or retained
        // retry — may only FINALIZE (upload + freeze the row + delete the
        // .partial) under the leadership tenure that enqueued it. A request
        // that outlived its tenure is evidence of the correlated failure
        // this guards against: it sat queued/deferred because PG was
        // unreachable, which is the same outage under which the terminal
        // status persist fails and the lease lapses — so the execution may
        // still be live, restamped with the SAME exec_id by the next
        // tenure's recovery (possibly this replica's own re-acquisition),
        // with its non-finalized drv_logs row and .partial still being
        // extended. The already-finalized guard below cannot catch that
        // (the row is is_complete=false precisely because the execution is
        // still running) and the UPSERT clause only protects finalized
        // rows. Drop the request instead: the live tenure's own terminal
        // processing finalizes the execution; if the terminal had already
        // persisted before the outage (or the worker died and the drv was
        // re-dispatched under a new exec), the row stays an
        // is_complete=false .partial — the pre-retry bounded loss,
        // surfaced per obs.log.incomplete-surfaced.
        //
        // Checked BEFORE the already-finalized guard: the guard's reap and
        // deferral arms are exec-guarded but not tenure-guarded, and
        // recovery's same-exec restamp reuses the exec_id, so for an
        // out-of-tenure request the Err arm's empty-entry reap (or the
        // deferral retention feeding the cap-overflow drain) could destroy
        // the LIVE execution's restamped carrier. With the pin first those
        // arms only ever see requests that were in tenure when the attempt
        // started; a request that goes stale DURING the guard/reconcile
        // awaits is caught by the post-await re-checks inside each arm
        // (`req_in_tenure` again, right before the entry-touching ops).
        // The drop arm itself performs ZERO PG and S3 work: every stale
        // request skips both during the very outage that orphaned it, so a
        // backlog of orphaned finals can never stall the flusher loop on
        // pool-acquire timeouts. The only entry it reaps is a sealed EMPTY
        // one; a sealed non-empty orphan is left for the periodic flush's
        // reaps — refused-UPSERT, or sealed-empty once the stored-coverage
        // reconcile empties its ring (see `upload_and_record`).
        //
        // generation() is monotone: bumped by on_acquire, raisable (never
        // lowered) by recovery's seed_generation_from, untouched by
        // on_lose — so `is_leader && generation == enqueue-time stamp` is
        // exactly "the same unbroken tenure", including flaps too fast for
        // any tick to observe the standby window.
        // r[impl obs.log.deferred-final-retry+3]
        if !self.req_in_tenure(&req) {
            // The retained entry is left for its real owner: across an
            // A→B→A flap recovery restamps the SAME exec_id onto it and the
            // reconnected worker's execution is live again on this replica
            // (the restamp cleared the seal and the final-pending mark), so
            // draining it would discard the live execution's ring and leave
            // its later pushes with no entry to land in (the re-dispatch
            // hazard FlushRequest::exec_id documents). Its reaper is then
            // the live tenure's own final, the drv's next dispatch discard,
            // or process exit.
            //
            // One exception: an entry that is still SEALED for this exec.
            // For an out-of-tenure request's exec, still-sealed means no
            // restamp in the current tenure adopted the entry as the live
            // carrier (every restamp clears the seal) — typically the drv's
            // terminal persisted under the old tenure and no reaper
            // remains: recovery restamps only Assigned|Running drvs, the
            // acquisition sweep skips sealed keys, and terminal cleanup
            // skips marked entries. (It can also be an entry the CURRENT
            // tenure's epilogue re-sealed with its own final still pending;
            // both reaps below stay safe in that case — see each.)
            //
            //  - Sealed and EMPTY: nothing to lose. Reads don't need it
            //    either — GetDerivationLogs probes the prior leader's
            //    stored `.partial` when the entry it finds holds zero
            //    lines — so the reap is memory/bookkeeping hygiene rather
            //    than read-path rescue; reap it. If it was
            //    the current tenure's empty pending final, that final takes
            //    the no-entry arm and only the empty drain's status stamp
            //    is lost. Seal, exec, and emptiness are evaluated inside
            //    the removal's own predicate (`discard_if_sealed_for_exec`),
            //    so the actor's same-exec restamp cannot adopt the entry
            //    between the check and the reap — it either lands first
            //    (seal cleared, no reap) or recreates a fresh carrier right
            //    after.
            //  - Sealed and NON-empty: left in place — this arm does no PG
            //    work, so it cannot tell whether another tenure already
            //    finalized the exec, and a row consult here would burn a
            //    pool-acquire timeout per orphaned final, serially on the
            //    flusher loop, during the very outage that orphaned them.
            //    Its reaper is the periodic flush, through one of two
            //    chokepoints in `upload_and_record`: while the ring stays
            //    non-empty the snapshot sweep keeps covering the entry,
            //    and the moment its row UPSERT is refused by the
            //    frozen-row latch (another tenure finalized the exec —
            //    `obs.log.finalize-immutable`) the still-sealed entry is
            //    discarded (the refused-UPSERT reap); if instead the
            //    per-tenure stored-coverage reconcile finds the stored row
            //    covering past the retained ring's tail (an interim leader
            //    kept flushing) and truncates the ring to empty, no UPSERT
            //    can ever run for it again — the periodic tick then reaps
            //    the sealed, now-empty entry at its empty-snapshot
            //    early-return (bookkeeping: reads already fall through to
            //    that stored `.partial` once the ring holds zero
            //    lines). Either way the orphan's shadowing of the
            //    durable record (and any `.partial` re-PUT churn) is
            //    bounded to one periodic tick after PG/leadership
            //    recovery. The permanent residual is an exec whose row is
            //    never finalized by any tenure AND whose stored coverage
            //    leaves the ring with lines past the stored end: there the
            //    ring's lines are the best data available, the periodic
            //    path keeps them durable at `.partial` coverage, and
            //    serving them as is_complete=false is correct.
            warn!(
                drv = %req.drv_path,
                exec_id = %req.exec_id,
                enqueued_generation = req.lease_generation,
                current_generation = self.leader.generation(),
                is_leader = self.leader.is_leader(),
                "dropping final log flush enqueued under a previous leadership \
                 tenure; the live tenure owns this execution's finalization"
            );
            metrics::counter!("rio_scheduler_log_flush_stale_tenure_total").increment(1);
            if self
                .buffers
                .discard_if_sealed_for_exec(&req.drv_path, req.exec_id, true)
            {
                debug!(
                    drv = %req.drv_path,
                    exec_id = %req.exec_id,
                    "reaped the dropped final's sealed, empty entry \
                     (bookkeeping; reads are unaffected)"
                );
            }
            return None;
        }
        // r[impl obs.log.finalize-immutable]
        // Refuse to re-finalize an execution another leader already
        // finalized — consulted before the prefix reconcile, the drain, and
        // any S3 work (only the tenure pin above runs earlier). Across an
        // A→B→A lease flap the
        // re-acquired ex-leader can retain a ring entry stamped with an
        // exec_id whose final flush already happened on the interim leader
        // (the drv was reset out of terminal afterwards, so the acquisition
        // sweep keeps the entry for the cancel-sweep finalization, and the
        // actor-side gate cannot see PG). Uploading that residue would
        // overwrite the finalized `.log.zst` with a stale pre-failover
        // snapshot; the frozen UPSERT clause would keep the row, which
        // would then describe content the stale PUT replaced. The refusal
        // makes no S3 calls at all: a `.partial` re-created by this
        // replica's periodic churn is left for the GC to sweep at TTL.
        //
        // On a final that passes this guard and still reaches the
        // reconcile with an Unchecked, non-empty ring (its execution had
        // no prior non-empty flush this tenure — the common fast-build
        // case), this point-SELECT duplicates the one inside
        // `lookup_stored_prefix` — accepted, to keep the guard independent
        // of the prefix-state machine.
        match self.fetch_stored_drv_log(req.exec_id).await {
            Ok(Some(stored)) if stored.is_complete => {
                // The guard SELECT awaited: the lease can have moved while
                // it was in flight, and recovery's same-exec restamp may by
                // now have adopted this entry as the LIVE execution's
                // carrier — which the exec-keyed drain below cannot tell
                // apart. Re-validate the tenure before the residue
                // drain/unseal; a request that went stale mid-await takes
                // the same drop path as the entry-time pin — except that
                // here the row in hand is proven finalized, so the reap may
                // remove the sealed residue even when non-empty
                // (require_empty=false; see `drop_stale_after_await`).
                // r[impl obs.log.deferred-final-retry+3]
                if !self.req_in_tenure(&req) {
                    self.drop_stale_after_await(&req, "already_finalized_refusal", false);
                    return None;
                }
                // Reap the retained residue iff it is still stamped with
                // this execution (atomic with the staleness check, exactly
                // like the upload path's drain). The durable record is
                // authoritative — any retained lines it lacks fall within
                // the accepted ≤30s periodic-flush failover-loss bound.
                match self.buffers.drain_if_exec(&req.drv_path, req.exec_id) {
                    Some((_, _, line_count, _, _)) => {
                        self.buffers.unseal(&req.drv_path);
                        warn!(
                            drv = %req.drv_path,
                            exec_id = %req.exec_id,
                            dropped_lines = line_count,
                            "dropping final flush: execution already finalized \
                             by another leader; retained buffer was stale \
                             pre-failover residue"
                        );
                        metrics::counter!("rio_scheduler_log_flush_already_finalized_total")
                            .increment(1);
                    }
                    // No retained entry stamped with this exec (duplicate
                    // final, or the entry was restamped to a newer
                    // execution): nothing to reap, nothing at risk — the
                    // blob/row are already final and we make no S3/PG
                    // writes. Not the flap signal the counter above tracks,
                    // so log-only at debug, no metric. No defensive unseal:
                    // flush_final never seals (the terminal handler seals
                    // before enqueueing), a duplicate's seal was already
                    // cleared by the first request's drain, a restamped
                    // entry's seal belongs to the live execution (mirrors
                    // the stale-request arm below), and orphan seals are
                    // bounded by `CleanupTerminalBuild`'s discard.
                    None => {
                        debug!(
                            drv = %req.drv_path,
                            exec_id = %req.exec_id,
                            "final flush for an already-finalized execution \
                             with no retained residue (duplicate request or \
                             restamped entry); nothing to drop"
                        );
                    }
                }
                return None;
            }
            Ok(_) => {}
            Err(e) => {
                // The failed SELECT still awaited (typically a full
                // pool-acquire timeout): the lease can have moved while it
                // was in flight, and recovery's same-exec restamp may by
                // now have adopted this entry as the LIVE execution's
                // carrier. Re-validate the tenure before the empty reap /
                // re-mark / retention below — a request that went stale
                // mid-await is dropped (counted on the stale-tenure
                // counter, not as a deferral), never retained, and only a
                // still-sealed empty entry is reaped. No second row consult
                // here: the SELECT just failed on this same pool, so
                // another `fetch_stored_drv_log` would only burn a second
                // acquire-timeout on the serial flusher.
                // r[impl obs.log.deferred-final-retry+3]
                if !self.req_in_tenure(&req) {
                    self.drop_stale_after_await(&req, "finalize_guard_error", true);
                    return None;
                }
                // Fail closed (`obs.log.finalize-immutable`): without the row we
                // cannot tell "first finalization" from "another leader already
                // finalized this exec", and proceeding is a destructive PUT at
                // the finalized key. Defer instead — but do not abandon the
                // request: the periodic snapshotter cannot PUT an entry whose
                // stored-coverage reconcile needs this same row (it skips the
                // tick), and on a deposed leader it does not run at all, so
                // "leave it to the snapshotter" is not a durability story.
                // r[impl obs.log.deferred-final-retry+3]
                metrics::counter!("rio_scheduler_log_flush_finalize_deferred_total").increment(1);

                // A zero-line entry (failover restamp whose worker never
                // re-streamed) has nothing any retry could upload, and
                // reads don't need it (GetDerivationLogs probes the
                // ex-leader's stored `.partial` when the entry it finds
                // holds zero lines) — retaining it would only pin memory
                // and the final-pending mark. Reap it
                // now — exec-guarded so a re-dispatched execution's fresh
                // empty entry is never touched. Mirrors the epilogue's
                // enqueue-failure reap; the prior `.partial` row keeps
                // status NULL until the TTL sweep, same as that path.
                if self
                    .buffers
                    .discard_if_empty_for_exec(&req.drv_path, req.exec_id)
                {
                    debug!(
                        drv = %req.drv_path,
                        exec_id = %req.exec_id,
                        error = %e,
                        "deferring final flush with an empty entry: reaped it \
                         (bookkeeping; reads are unaffected); row left to \
                         the TTL sweep"
                    );
                    return None;
                }

                // Non-empty entry still stamped with this execution:
                // re-assert the pending mark (normally already set at
                // enqueue; the call doubles as the does-an-entry-stamped-
                // with-this-exec-still-exist check) and retain the request
                // for retry. The retried flush re-runs this guard once PG
                // answers and then drains/uploads (or refuses, if an
                // interim leader finalized it meanwhile).
                if self.buffers.mark_final_pending(&req.drv_path, req.exec_id) {
                    warn!(
                        drv = %req.drv_path,
                        exec_id = %req.exec_id,
                        error = %e,
                        "deferring final flush: could not consult drv_logs to \
                         rule out an existing finalization; nothing uploaded, \
                         buffer retained and the request will be retried on the \
                         next flusher tick (and dropped instead if leadership \
                         has moved by then)"
                    );
                    return Some(req);
                }

                // No entry (or restamped to a newer execution): there is
                // nothing a retry could ever drain — consume the request.
                debug!(
                    drv = %req.drv_path,
                    exec_id = %req.exec_id,
                    error = %e,
                    "deferring final flush: drv_logs unreadable and no ring entry \
                     remains for this execution; nothing to retain"
                );
                return None;
            }
        }
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
        //
        // Settle the stored-coverage question BEFORE the drain (see
        // `reconcile_stored_prefix`): a superseded ring head must be
        // dropped while the lines are still in the entry (the drained
        // payload has no per-line numbers), and the recovered prefix must
        // be cached before the drain removes the entry. A stale request
        // (entry restamped to a newer exec) reads `Checked` here and
        // skips — `drain_if_exec` below still owns the staleness call.
        if matches!(
            self.buffers.prefix_state(&req.drv_path, req.exec_id),
            PrefixState::Unchecked
        ) {
            // FetchFailed leaves the state Unchecked; the match below maps
            // that to "upload the drain but preserve the .partial".
            let _ = self
                .reconcile_stored_prefix(&req.drv_path, req.exec_id, true)
                .await;
        }
        // Read AFTER the reconcile and BEFORE the drain (the drain removes
        // the entry and the cached prefix with it).
        let pre_drain_prefix_state = self.buffers.prefix_state(&req.drv_path, req.exec_id);
        // Post-await tenure re-check, as late as possible: the guard SELECT
        // and the reconcile above both awaited (PG + possibly an S3 GET),
        // and the lease can have moved while they were in flight — by now
        // the entry may be the LIVE execution's same-exec-restamped carrier
        // and the row/`.partial` may be the live tenure's coverage, neither
        // of which this stale request may drain/freeze/delete. Deliberately
        // placed AFTER the reconcile: its entry mutations (head truncation
        // below the stored end, prefix cache/latch) are exec-guarded and are
        // the same operations the live tenure's own flush performs, so
        // running them under a stale lease is harmless — do not move this
        // check above the reconcile, or the post-reconcile window reopens.
        // The window between this check and the PUT/UPSERT below is
        // deliberately NOT re-checked: after the drain this replica holds
        // the only copy of the terminal-observed lines (aborting would lose
        // them), and the row write is protected by the frozen-row UPSERT
        // clause plus the already-finalized guard. The residual is a lease
        // move during the PUT freezing the row while the same exec is still
        // streaming elsewhere — only reachable when the worker keeps
        // streaming past a terminal this tenure already observed (the
        // cancel race), and those post-cancel lines are the same marginal
        // class the design already discards.
        // r[impl obs.log.deferred-final-retry+3]
        if !self.req_in_tenure(&req) {
            self.drop_stale_after_await(&req, "pre_drain", true);
            return None;
        }
        if let Some((first_line, last_line, line_count, raw_bytes, lines)) =
            self.buffers.drain_if_exec(&req.drv_path, req.exec_id)
        {
            self.buffers.unseal(&req.drv_path);
            let (recovered_prefix, preserve_partial) = match pre_drain_prefix_state {
                PrefixState::Cached(p) => (Some(p), false),
                // Looked (or nothing stored): final behaves exactly as
                // today.
                PrefixState::Checked => (None, false),
                // Still Unchecked ⇒ the ring was empty at reconcile time
                // (the drain is then empty too and `finalize_empty_drain`
                // owns the row), or the stored blob could not be re-read.
                // In neither case is deleting the `.partial` required, and
                // in the fetch-failed case it is the only copy of content
                // this upload does not carry — keep it.
                PrefixState::Unchecked => (None, true),
            };
            self.upload_and_record(
                req,
                FlushPayload {
                    first_line,
                    last_line,
                    line_count,
                    raw_bytes,
                    lines,
                    recovered_prefix,
                    preserve_partial,
                },
                true,
            )
            .await;
            return None;
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
        // backstop if a `FlushRequest` was dropped (`try_send` full)
        // (zero-line entries are reaped at the epilogue the moment the
        // enqueue fails).
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
        None
    }

    /// Whether `req` was enqueued by the leadership tenure this replica
    /// currently holds: leader AND the generation still equals the
    /// enqueue-time stamp. The single definition of "in tenure" shared by
    /// `flush_final`'s entry pin, its post-await re-checks, and
    /// `retain_deferred`'s overflow gate (see the entry pin's comment for
    /// why generation monotonicity makes this exactly "the same unbroken
    /// tenure").
    fn req_in_tenure(&self, req: &FlushRequest) -> bool {
        self.leader.is_leader() && self.leader.generation() == req.lease_generation
    }

    /// Shared drop tail for a final whose tenure ended while `flush_final`
    /// was parked on an awaited step (the finalize-guard SELECT or the
    /// stored-prefix reconcile): warn, count it on the stale-tenure
    /// counter, reap the entry only when it is still sealed for this exec
    /// AND the caller's `require_empty` bound holds (an unsealed entry may
    /// be the live execution's restamped carrier and is never touched), and
    /// let the caller return `None` so the request is never retained.
    ///
    /// `require_empty` is `true` at the `finalize_guard_error` and
    /// `pre_drain` stages: those hold no evidence about the stored row, so
    /// a non-empty sealed entry still holds lines another owner may fold
    /// and only the empty no-reaper shape is reaped. The
    /// `already_finalized_refusal` stage passes `false`: its guard SELECT
    /// returned `is_complete = true` before the staleness was detected, and
    /// finalize-immutability means that cannot regress — the durable record
    /// supersedes the retained lines, so the sealed non-empty residue is
    /// reaped too instead of shadowing the finalized blob until restart
    /// (same safety argument as the periodic refused-UPSERT reap in
    /// `upload_and_record`; a same-exec restamp landing mid-await clears
    /// the seal and the discard no-ops).
    // r[impl obs.log.deferred-final-retry+3]
    fn drop_stale_after_await(&self, req: &FlushRequest, stage: &str, require_empty: bool) {
        warn!(
            drv = %req.drv_path,
            exec_id = %req.exec_id,
            enqueued_generation = req.lease_generation,
            current_generation = self.leader.generation(),
            is_leader = self.leader.is_leader(),
            stage,
            "dropping final log flush: leadership tenure ended while the \
             flush was awaiting PG/S3; the live tenure owns this \
             execution's finalization"
        );
        metrics::counter!("rio_scheduler_log_flush_stale_tenure_total").increment(1);
        if self
            .buffers
            .discard_if_sealed_for_exec(&req.drv_path, req.exec_id, require_empty)
        {
            if require_empty {
                // finalize_guard_error / pre_drain: only an empty entry is
                // ever reaped here, and reads already fall through to the
                // stored side for a zero-line entry whether or not the reap
                // fires.
                debug!(
                    drv = %req.drv_path,
                    exec_id = %req.exec_id,
                    require_empty,
                    "reaped the dropped final's sealed, empty entry \
                     (bookkeeping; reads are unaffected)"
                );
            } else {
                // already_finalized_refusal: the guard row is proven
                // is_complete=true, so the sealed non-empty residue is
                // dropped and reads fall through to the finalized drv_logs
                // record instead of the stale ring lines.
                debug!(
                    drv = %req.drv_path,
                    exec_id = %req.exec_id,
                    require_empty,
                    "reaped the dropped final's sealed entry so reads fall \
                     through to the finalized drv_logs record"
                );
            }
        }
    }

    /// Retain a deferred final for retry, bounded by [`DEFERRED_FINALS_MAX`]
    /// and deduplicated by `exec_id` (one retained request per execution; a
    /// duplicate from a NEWER lease tenure replaces the retained one — the
    /// older request is already dead under `flush_final`'s tenure check). On
    /// overflow the request is dropped and the execution's buffered entry is
    /// dropped with it (exec-guarded): the final-pending mark is set at
    /// enqueue, so terminal cleanup may already have run and skipped the
    /// entry — handing it back to cleanup is no longer a disposal, and
    /// clearing only the mark would leak a sealed entry that nothing ever
    /// reaps. The entry drop additionally requires the overflowing request
    /// to still be in tenure: an out-of-tenure victim's entry may be the
    /// live execution's restamped carrier, so the request is dropped without
    /// touching it — `flush_final`'s tenure-drop arm and its post-await
    /// re-checks are the only entry-touching paths for stale requests, and
    /// they only ever remove SEALED entries, never the unsealed live
    /// carrier (at most a sealed empty one, except the already-finalized
    /// re-check, which also removes a sealed non-empty residue because its
    /// guard row is already finalized).
    // r[impl obs.log.deferred-final-retry+3]
    fn retain_deferred(&self, deferred: &mut Vec<FlushRequest>, req: FlushRequest) {
        if let Some(existing) = deferred.iter_mut().find(|d| d.exec_id == req.exec_id) {
            // One retained request per execution. Prefer the most recently
            // enqueued one: after a lose/re-acquire the same exec_id can get
            // a second, legitimate final from the new tenure (recovery
            // restamps the same exec), and the older request is already dead
            // under the tenure check — keeping it would shadow the only
            // request that can still finalize the row.
            if existing.lease_generation != req.lease_generation {
                *existing = req;
            }
            return;
        }
        if deferred.len() >= DEFERRED_FINALS_MAX {
            // Cap overflow: accept the loss of this execution's buffered
            // tail (the documented fallback). The entry cannot be handed
            // back to terminal cleanup any more — the final-pending mark is
            // set at enqueue, so the build's CleanupTerminalBuild may
            // already have run and skipped this entry on the strength of
            // it; clearing the mark here would leave a sealed entry that
            // nothing ever reaps. Drop it now instead (exec-guarded so a
            // re-dispatched execution's fresh entry is never touched) and
            // clear the seal tombstone with it — but only while the request
            // is still in tenure: the lease can move between `flush_final`'s
            // pin check and this retention (the guard SELECT awaits), and
            // recovery's same-exec restamp makes an out-of-tenure victim's
            // entry the LIVE execution's carrier, which must not be drained
            // (the tenure-drop arm and the post-await re-checks — which
            // never drain anything and never touch an unsealed entry, only
            // seal-guarded reaps — are the only entry-touching paths for
            // stale requests).
            let in_tenure = self.req_in_tenure(&req);
            if in_tenure
                && self
                    .buffers
                    .drain_if_exec(&req.drv_path, req.exec_id)
                    .is_some()
            {
                self.buffers.unseal(&req.drv_path);
            }
            warn!(
                drv = %req.drv_path,
                exec_id = %req.exec_id,
                retained = deferred.len(),
                "deferred-final retry queue full; dropping this execution's \
                 buffered log tail"
            );
            return;
        }
        deferred.push(req);
    }

    /// Re-run retained deferred finals. Requests that resolve (uploaded,
    /// refused, stale, entry gone, or dropped as orphaned by a leadership
    /// change) are dropped; the pass STOPS at the first request that defers
    /// again — every retained request goes through the same PG pool, so
    /// further attempts in the same pass would repeat the failure and
    /// serially burn the acquire timeout each (with 64 retained requests
    /// that would stall the select loop for minutes and back up flush_rx).
    /// The un-attempted remainder stays at the front of the queue for the
    /// next pass; the re-deferred request goes to the back, so a
    /// request-specific persistent error (e.g. a row that fails to decode)
    /// cannot head-of-line-block the rest forever. Runs on the periodic tick
    /// and on shutdown, with no leadership gate at the loop level: each
    /// request is validated per-attempt inside [`Self::flush_final`] against
    /// the tenure that enqueued it (`FlushRequest::lease_generation`). An
    /// orphaned request resolves as a drop at the per-attempt tenure pin
    /// with no PG work at all (a still-down PG can neither delay nor retain
    /// it);
    /// what that drop costs is bounded — the live tenure's own terminal
    /// flush finalizes a still-live execution, an execution whose terminal
    /// had already persisted (or whose drv was re-dispatched under a new
    /// exec) stays at its `.partial` coverage (surfaced per
    /// `obs.log.incomplete-surfaced`), and an execution with no stored row
    /// yet loses only its un-flushed ring prefix.
    // r[impl obs.log.deferred-final-retry+3]
    async fn retry_deferred(&self, deferred: &mut Vec<FlushRequest>) {
        if deferred.is_empty() {
            return;
        }
        debug!(pending = deferred.len(), "retrying deferred final flushes");
        let mut pending = std::mem::take(deferred).into_iter();
        while let Some(req) = pending.next() {
            if let Some(again) = self.flush_final(req).await {
                // PG still failing for this pass: keep the rest for the next
                // one (front), rotate the failing request to the back. Same
                // elements as were taken — cannot exceed the cap.
                deferred.extend(pending);
                deferred.push(again);
                return;
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
            // queued request to go stale. The reconcile below can await
            // (SELECT + possibly a GET), so the stamp is re-checked after
            // it before the snapshot is taken. (The staleness guard is for
            // `flush_final`, where the request can sit in the mpsc across
            // a re-dispatch.)
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

            // First non-empty flush of this execution on this tenure:
            // settle the stored-coverage question BEFORE snapshotting so a
            // superseded ring head is dropped before it can be uploaded
            // over durable content (A→B→A re-acquire), and the snapshot
            // below already reflects the truncation. Zero-line rings leave
            // the state Unchecked — the lookup needs the ring's span.
            if matches!(
                self.buffers.prefix_state(&drv_path, exec_id),
                PrefixState::Unchecked
            ) {
                match self
                    .reconcile_stored_prefix(&drv_path, exec_id, false)
                    .await
                {
                    PrefixReconcile::Reconciled | PrefixReconcile::RingEmpty => {}
                    // Never overwrite content we failed to read; retry next tick.
                    PrefixReconcile::FetchFailed => continue,
                }
                // The reconcile awaited (SELECT + possibly a GET). A
                // re-dispatch could have restamped the entry meanwhile;
                // re-read the stamp so the snapshot below cannot upload a
                // newer execution's lines under this exec's key. (Restores
                // the pre-existing no-await-sized window between this check
                // and the snapshot.)
                if self.buffers.exec_id(&drv_path) != Some(exec_id) {
                    continue;
                }
            }

            let Some((first_line, last_line, line_count, raw_bytes, lines)) =
                self.buffers.snapshot(&drv_path)
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
                // Not consulted on the periodic path (the snapshot is
                // self-driven and already behind `may_flush`); stamped for
                // completeness.
                lease_generation: self.leader.generation(),
            };

            // Recovered-prefix handling (any later tenure of the same
            // execution — see the module doc). Zero-line snapshots
            // (dispatched but not yet streaming, including the
            // post-failover window BEFORE the worker reconnects) must
            // leave the prefix state Unchecked: the reconcile needs the
            // ring's span, and prematurely marking "checked" here would
            // let the first real snapshot after reconnection clobber the
            // stored prefix.
            let recovered_prefix = if line_count == 0 {
                None // upload_and_record's early-return skips this snapshot anyway
            } else {
                match self.buffers.prefix_state(&req.drv_path, exec_id) {
                    PrefixState::Cached(p) => Some(p),
                    PrefixState::Checked => None,
                    // Lines landed between the empty-span reconcile above
                    // and this snapshot (sub-ms window): defer to the next
                    // tick rather than upload without having consulted the
                    // row.
                    PrefixState::Unchecked => continue,
                }
            };
            self.upload_and_record(
                req,
                FlushPayload {
                    first_line,
                    last_line,
                    line_count,
                    raw_bytes,
                    lines,
                    recovered_prefix,
                    preserve_partial: false,
                },
                false,
            )
            .await;
        }
    }

    /// Point-SELECT the `drv_logs` row for one execution. `Ok(None)` means
    /// no flush of this execution has ever recorded a row.
    async fn fetch_stored_drv_log(&self, exec_id: Uuid) -> sqlx::Result<Option<StoredDrvLogRow>> {
        let row: Option<(String, i64, i64, i64, bool)> = sqlx::query_as(
            "SELECT s3_key, first_line, line_count, total_bytes, is_complete \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(
            |(s3_key, first_line, line_count, total_bytes, is_complete)| StoredDrvLogRow {
                s3_key,
                first_line: first_line as u64,
                line_count: line_count as u64,
                total_bytes: total_bytes as u64,
                is_complete,
            },
        ))
    }

    /// Detect and fetch stored content of this execution that the current
    /// in-memory ring does not cover. One point-SELECT + (at most one) S3
    /// GET. Only called by [`Self::reconcile_stored_prefix`] while the
    /// entry is still `Unchecked` and the ring is non-empty — i.e. at most
    /// once per execution per tenure, never on the steady-state path.
    ///
    /// The stored content needs rescuing iff a `drv_logs` row exists for
    /// this `exec_id`, is not finalized, and the ring does **not**
    /// contiguously subsume the row's stated range. A prior tenure is the
    /// only writer that can have produced such a row — exec_ids are minted
    /// fresh per dispatch and the same-tenure first flush sees no row — so
    /// this fires only after the per-tenure re-arm of the entry's prefix
    /// bookkeeping (a recovery restamp, or
    /// [`LogBuffers::rearm_prefix_reconciliation`] for retained entries
    /// recovery does not restamp): a fresh standby holding the re-streamed
    /// suffix, or a re-acquired ex-leader whose retained ring overlaps /
    /// has interior holes relative to what an interim leader stored.
    /// Overlap handling (yielding the ring's head to the stored copy) is
    /// the caller's job.
    ///
    /// `ring_span` is `(first_line, last_line, line_count)` of the stamped
    /// ring entry, as returned by [`LogBuffers::span`]; `line_count > 0`.
    ///
    /// `is_final` is the caller's flush kind, threaded into
    /// [`prefix_fetch_failure`] so the metric label and message reflect
    /// whether the degraded merge affects a final upload or a periodic
    /// snapshot (fetch failures here are pre-drain and are NOT flush
    /// failures — see that helper's doc).
    async fn lookup_stored_prefix(
        &self,
        drv_path: &str,
        exec_id: Uuid,
        ring_span: (u64, u64, u64),
        is_final: bool,
    ) -> StoredPrefixLookup {
        let (ring_first_line, ring_last_line, ring_line_count) = ring_span;
        // The row's `s3_key` is what we'd GET; on PG failure we don't know
        // it yet, so report the deterministic `.partial` key (what the row
        // would point at) rather than a placeholder.
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        let row = match self.fetch_stored_drv_log(exec_id).await {
            Ok(row) => row,
            Err(e) => {
                // Treat as FetchFailed: don't clobber what we can't see.
                prefix_fetch_failure(is_final, "pg", &partial_key, &e, drv_path);
                return StoredPrefixLookup::FetchFailed;
            }
        };

        let Some(StoredDrvLogRow {
            s3_key,
            first_line,
            line_count,
            total_bytes,
            is_complete,
        }) = row
        else {
            return StoredPrefixLookup::NotNeeded;
        };
        // Finalized rows are refused upstream by `flush_final`'s
        // already-finalized guard and frozen at the UPSERT
        // (r[obs.log.finalize-immutable]); the `is_complete` arm here
        // covers the periodic path, which has no such guard.
        if is_complete || line_count == 0 {
            return StoredPrefixLookup::NotNeeded;
        }
        // The ring makes the stored row redundant only when it contiguously
        // covers the row's whole stated range — the same-tenure steady state
        // (this process wrote the row from this very ring). Anything short
        // of that — the row extends past the ring's head or tail, or the
        // ring has an interior hole (lines delivered only to an interim
        // leader during an A→B→A flap) — means the stored blob may hold
        // lines this replica never had, and it must be folded in rather
        // than overwritten. Finalized rows stay NotNeeded: the frozen
        // UPSERT clause protects the row, and refusing the re-finalization
        // upload is the already-finalized gate's job in flush_final.
        //
        // The stated end is the execution's true line-number end even for
        // gap-merged blobs — `upload_and_record` keeps the row's range in
        // true line-number space (`obs.log.gap-span`) — so this subsumption
        // test and the caller's gap arithmetic operate on exact bounds.
        // r[impl obs.log.stored-coverage-preserved]
        let stored_end = first_line + line_count;
        // Checked: a non-monotone ring (only possible if the push_for
        // ingestion gate regresses) classifies as non-contiguous — the
        // conservative branch (the stored prefix is fetched and merged) —
        // instead of underflowing.
        let ring_contiguous = ring_last_line
            .checked_sub(ring_first_line)
            .is_some_and(|d| d.saturating_add(1) == ring_line_count);
        if ring_contiguous && ring_first_line <= first_line && ring_last_line + 1 >= stored_end {
            return StoredPrefixLookup::NotNeeded;
        }
        match self
            .s3
            .get_object()
            .bucket(&self.bucket)
            .key(&s3_key)
            .send()
            .await
        {
            Ok(out) => match out.body.collect().await {
                Ok(bytes) => {
                    metrics::counter!("rio_scheduler_log_prefix_recovered_total").increment(1);
                    info!(
                        drv = %drv_path,
                        exec_id = %exec_id,
                        prefix_lines = line_count,
                        ring_first_line,
                        "recovered prior-tenure log content from stored .partial"
                    );
                    StoredPrefixLookup::Found(Arc::new(RecoveredPrefix {
                        first_line,
                        line_count,
                        total_bytes,
                        compressed: bytes.into_bytes().to_vec(),
                    }))
                }
                Err(e) => {
                    prefix_fetch_failure(is_final, "s3", &s3_key, &e, drv_path);
                    StoredPrefixLookup::FetchFailed
                }
            },
            Err(e) => {
                prefix_fetch_failure(is_final, "s3", &s3_key, &e, drv_path);
                StoredPrefixLookup::FetchFailed
            }
        }
    }

    /// Once-per-execution-per-tenure reconciliation of the ring against the
    /// stored `drv_logs` row, run before the first non-empty flush of an
    /// execution on this tenure (`PrefixState::Unchecked`).
    ///
    /// A non-finalized row that pre-exists this tenure's first flush was
    /// written by a prior tenure: a fresh-standby failover (ring holds only
    /// the re-streamed suffix) or an A→B→A re-acquire where this replica's
    /// retained ring starts at line 0 but an interim leader flushed lines
    /// this replica never received (interior hole, or stored coverage past
    /// the ring's tail). In all of those the stored blob may be the only
    /// copy of those lines: it is fetched and cached as the execution's
    /// [`RecoveredPrefix`], and the ring yields every line below the row's
    /// stated end ([`LogBuffers::truncate_below`]) so the standard
    /// prefix + gap-marker + ring merge applies. Within the stored range
    /// the stored copy wins; a retained head BELOW the stored range is
    /// dropped too (the merge supports one prefix only) — that head is the
    /// prior tenure's unflushed tail, inside the spec's ≤30s failover
    /// budget. Healing in-band-marked gaps of the stored blob from retained
    /// memory, and a lossless head+prefix+tail merge, are deliberately out
    /// of scope (≤30s already-declared/budgeted loss). When the ring
    /// contiguously subsumes the stored range, nothing is fetched and the
    /// entry is marked checked.
    ///
    /// Once the entry is `Checked`/`Cached` this does not re-run within the
    /// tenure, so same-tenure ring eviction past the stored row keeps the
    /// pre-existing accepted head-loss behavior
    /// (`checked_entry_final_flush_skips_lookup`); prior-tenure content
    /// stays covered regardless because it lives in the cached prefix, not
    /// the ring.
    ///
    /// The truncation threshold is the row's stated end
    /// (`first_line + line_count`), which is the execution's true
    /// line-number span even for gap-merged blobs (`obs.log.gap-span`),
    /// so the threshold is exact.
    // r[impl obs.log.stored-coverage-preserved]
    async fn reconcile_stored_prefix(
        &self,
        drv_path: &str,
        exec_id: Uuid,
        is_final: bool,
    ) -> PrefixReconcile {
        let Some((ring_first, ring_last, ring_count)) = self.buffers.span(drv_path, exec_id) else {
            return PrefixReconcile::RingEmpty;
        };
        if ring_count == 0 {
            return PrefixReconcile::RingEmpty;
        }
        match self
            .lookup_stored_prefix(
                drv_path,
                exec_id,
                (ring_first, ring_last, ring_count),
                is_final,
            )
            .await
        {
            StoredPrefixLookup::NotNeeded => {
                self.buffers.mark_prefix_checked(drv_path, exec_id);
                PrefixReconcile::Reconciled
            }
            StoredPrefixLookup::Found(p) => {
                let stored_end = p.first_line + p.line_count;
                if stored_end > ring_first {
                    // Stored coverage reaches into (or past) the retained
                    // ring: the ring yields below the stored end so the
                    // merge cannot un-cover durable content (overlapping
                    // lines are superseded; a non-overlapping head below
                    // the stored range is the prior tenure's ≤30s unflushed
                    // tail — see the rustdoc).
                    let (dropped_lines, dropped_bytes) =
                        self.buffers.truncate_below(drv_path, exec_id, stored_end);
                    info!(
                        drv = %drv_path,
                        exec_id = %exec_id,
                        stored_end,
                        ring_first,
                        dropped_lines,
                        dropped_bytes,
                        "stored log coverage overlaps the retained ring; superseded ring head dropped in favor of the stored prefix"
                    );
                }
                self.buffers.set_recovered_prefix(drv_path, exec_id, p);
                PrefixReconcile::Reconciled
            }
            StoredPrefixLookup::FetchFailed => PrefixReconcile::FetchFailed,
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
    /// `payload` carries the ring snapshot/drain plus, on any later tenure
    /// of the execution after a failover with a reconnecting worker, the
    /// recovered prior-tenure prefix ([`FlushPayload::recovered_prefix`]).
    /// When the prefix is present the stored blob carries
    /// prefix + (optional gap marker) + ring lines, while the row's
    /// `first_line`/`line_count` describe the execution's TRUE
    /// line-number span (the prefix→ring gap and any interior hole are
    /// counted, the marker is not extra —
    /// `obs.log.gap-span`) and `total_bytes` stays physical. The
    /// `.partial` overwrite is therefore strictly coverage-growing and the
    /// final `.log.zst` + `.partial` delete are safe again — including
    /// across A→B→A lease flaps, where [`Self::reconcile_stored_prefix`]
    /// is what guarantees a retained ring cannot un-cover what an interim
    /// leader stored. A worker that re-sent lines the stored row already
    /// covers (excluded by the worker's resume-after-last-delivered
    /// contract) now classifies as overlapping → no merge, suffix-only
    /// snapshot — the understated end could previously have merged it
    /// with a duplicated boundary line. When a final flush could not
    /// re-read a known stored prefix ([`FlushPayload::preserve_partial`])
    /// the `.partial` delete is skipped — that blob is the prefix's only
    /// copy.
    ///
    /// `req.exec_id` keys the S3 blob and PG row. The actor pins it at
    /// terminal time for finals; the periodic flusher reads it from the
    /// live entry and re-checks it after the awaited reconcile, before the
    /// snapshot. Both callers verify it before constructing the request —
    /// there is no `None` case here. `set_exec` is always called at
    /// `assign_to_worker` and re-stamped by recovery for active
    /// assignments.
    async fn upload_and_record(&self, req: FlushRequest, payload: FlushPayload, is_final: bool) {
        let exec_id = req.exec_id;
        let FlushPayload {
            first_line,
            last_line,
            line_count,
            raw_bytes,
            lines,
            recovered_prefix,
            preserve_partial,
        } = payload;
        // Empty drain/snapshot. `set_exec` creates an empty ring-buffer
        // entry, so BOTH callers can land here with zero lines:
        //
        //  - Periodic (`is_final=false`): the window between dispatch and
        //    the worker's first batch (overlay setup, FUSE warm — easily
        //    >30s). Skip entirely — a zero-line `.partial` blob and PG row
        //    for every dispatched-but-not-yet-streaming drv would be noise.
        //    One exception before returning: a SEALED empty entry stamped
        //    with this exec is reaped here. Sealed means a terminal already
        //    fired and no restamp adopted the entry as the live carrier;
        //    empty means there is nothing left to persist — either no line
        //    ever landed, or this tick's stored-coverage reconcile just
        //    truncated the whole ring away because a prior tenure's row
        //    covers past its tail. Such an entry can never reach the
        //    PUT/UPSERT below (this early-return runs every tick), so the
        //    refused-UPSERT reap further down is structurally unreachable
        //    for it, no other reaper remains (recovery restamps only
        //    Assigned|Running drvs, the acquisition sweep skips sealed
        //    keys, terminal cleanup skips final-pending entries), and left
        //    in place it would just sit in memory until process restart
        //    (reads are unaffected either way: GetDerivationLogs probes
        //    the stored `.partial` when the entry it finds holds zero
        //    lines). Seal, exec, and emptiness are re-evaluated
        //    inside the removal's own predicate
        //    (`discard_if_sealed_for_exec`), so a concurrent same-exec
        //    restamp either lands first (seal cleared, no reap) or
        //    recreates a fresh carrier right after; an unsealed empty
        //    entry (a just-dispatched carrier waiting for its first batch)
        //    is never touched. The residual matches the tenure-drop arm's
        //    empty reap: if the entry's own IN-tenure final is still
        //    queued (silent build, or a cancel before any output, racing
        //    the unbiased select), that final takes the no-entry arm and
        //    only the empty drain's status/finished_at stamp is lost —
        //    and unlike the drop arm this can now happen on a healthy
        //    leader with no tenure or PG failure at all. No log lines can
        //    be lost: the predicate requires emptiness, and a sealed entry
        //    rejects pushes anyway.
        //  - Final (`is_final=true`): `drain_if_exec` matches on `exec_id`
        //    alone, so a stamped-but-empty entry drains as
        //    `Some((0, 0, 0, []))`, not `None`. Recovery's `set_log_exec`
        //    restamp on a fresh standby leaves exactly that when the
        //    worker never reconnects before the drv terminates. There is
        //    nothing to upload, but a `drv_logs` row the ex-leader's
        //    periodic flusher already wrote still gets its terminal
        //    stamps (status/finished_at) so it does not look in-flight
        //    for the full retention TTL while `record_exec_correlation`
        //    pins the dashboard to it. `is_complete` stays false: the
        //    stored `.partial` is truncated at the last periodic snapshot
        //    and the incomplete indicator must survive
        //    (`obs.log.incomplete-surfaced`). (The
        //    stale-request arm of `flush_final` — `drain_if_exec` misses,
        //    the live entry is already stamped exec₂ — has the analogous
        //    pre-existing gap for a re-dispatched drv: exec₁'s `.partial`
        //    row is never finalized. The dashboard pin does NOT move:
        //    `record_exec_correlation` is write-once
        //    (`AND exec_id IS NULL`), so builds that observed exec₁'s
        //    terminal keep exec₁ and ARE served that row (if a periodic
        //    snapshot wrote one), minus the tail `discard_log_buffer`
        //    dropped at re-dispatch (≤ ~PERIODIC_FLUSH_INTERVAL of
        //    output). Accepted, not fixed here: the tail is already gone
        //    when the stale request is seen, `is_complete=false` keeps
        //    the incomplete banner honest, and `status`/`finished_at`
        //    have no production read path. Pinned by
        //    `flush_final_stale_request_leaves_prior_partial_row_untouched`.)
        if line_count == 0 {
            if is_final {
                self.finalize_empty_drain(&req).await;
            } else if self
                .buffers
                .discard_if_sealed_for_exec(&req.drv_path, exec_id, true)
            {
                debug!(
                    drv = %req.drv_path,
                    exec_id = %exec_id,
                    "empty periodic snapshot of a sealed entry: reaped it \
                     (bookkeeping; reads are unaffected)"
                );
            }
            return;
        }

        let drv_hash = drv_log_hash(&req.drv_path);
        debug_assert!(
            !drv_hash.is_empty(),
            "drv_log_hash({:?}) yielded empty hash — drv_path not store-path-shaped",
            req.drv_path
        );
        let s3_key = log_s3_key(&req.drv_path, &exec_id, !is_final);

        // Effective row metadata. With a recovered prefix the stored blob
        // covers prefix + (optional gap marker) + ring lines, but the row's
        // (first_line, line_count) is kept in TRUE worker line-number
        // space: the gap's lost lines are counted even though the blob
        // replaces them with a single marker line, so `first_line +
        // line_count` is one past the highest true line stored — the end
        // `s3_is_caught_up` and the next failover's `lookup_stored_prefix`
        // subsumption/gap arithmetic rely on. (Recording the physical
        // count — prefix + marker + ring — understated the end by gap−1
        // and made every further flap fold in a spurious "~1 earlier lines
        // lost" marker.) `p.line_count` is the stored row's value, itself
        // a true span for nested merges. The marker is one synthetic line
        // flagging the spec-accepted ≤30s window that was never flushed by
        // the prior leader; ranges that abut exactly (gap == 0) get no
        // marker. `total_bytes` stays physical — it sizes the blob, not
        // the span. The payload's own contribution is its line-number span
        // (`last_line − first_line + 1`), NOT its physical count: a
        // re-acquired ex-leader's retained ring can carry an interior hole
        // (lines delivered only to an interim leader that never flushed
        // them), and recording the physical count would understate the
        // end — `s3_is_caught_up` would then skip stored tail lines and
        // the read path's physical-vs-claimed re-serve could never fire
        // because the counts would match. The hole gets no in-band marker
        // (spec: its absence is not separately marked); hole-carrying
        // blobs simply claim more lines than they physically hold, which
        // is the divergence the read path already answers with a full
        // re-serve. A later tenure still holding the hole lines truncates
        // them at this (larger) end without storing them — the same
        // accepted interim-leader loss, now without a lying row.
        // r[impl obs.log.gap-span]
        let prefix_lines_recovered = recovered_prefix.as_ref().map(|p| p.line_count);
        // Ring contribution to the row span, in TRUE line-number space
        // (last − first + 1; exceeds the physical count exactly when the ring
        // carries an interior hole — see above). Ingestion enforces monotone
        // numbering per entry (`LogBuffers::push_for` rejects non-monotone
        // and overflowing batches), so for a non-empty payload
        // last_line ≥ first_line always holds in production. Computed totally
        // anyway: the ingestion gate only constrains a batch against the
        // ring's CURRENT contents (it resets when the ring empties), so if a
        // future ingestion gap lets out-of-order numbering through, the span
        // must degrade to the physical line count (the well-formed
        // pre-hole-aware behavior) rather than wrap into a negative BIGINT in
        // drv_logs.line_count — which corrupts s3_is_caught_up and the
        // physical-vs-claimed re-serve for every interested build — or panic
        // the only flusher task.
        // r[impl sched.executor.input-bounds+2]
        let ring_span = match last_line.checked_sub(first_line) {
            Some(d) => d.saturating_add(1),
            None => {
                warn!(
                    drv = %req.drv_path,
                    exec_id = %exec_id,
                    first_line,
                    last_line,
                    line_count,
                    is_final,
                    "flush payload carries non-monotone line numbers; \
                     recording the physical line count instead of the \
                     line-number span"
                );
                metrics::counter!(
                    "rio_scheduler_log_flush_span_fallback_total",
                    "kind" => if is_final { "final" } else { "periodic" },
                )
                .increment(1);
                line_count
            }
        };
        let (eff_first_line, eff_line_count, eff_total_bytes, gap_marker) = match &recovered_prefix
        {
            Some(p) => {
                // Saturating: total under arbitrary stored/worker inputs;
                // sane inputs are unaffected.
                let prefix_end = p.first_line.saturating_add(p.line_count);
                let gap = first_line.saturating_sub(prefix_end);
                let marker = (gap > 0).then(|| {
                    format!("[rio: ~{gap} earlier lines lost across scheduler failover]")
                        .into_bytes()
                });
                let marker_len = marker.as_ref().map(|m| m.len() as u64).unwrap_or(0);
                (
                    p.first_line,
                    // True span: prefix span + gap + ring line-number span
                    // (= ring true end − prefix start), NOT prefix + marker
                    // + physical ring lines.
                    p.line_count.saturating_add(gap).saturating_add(ring_span),
                    p.total_bytes
                        .saturating_add(marker_len)
                        .saturating_add(raw_bytes),
                    marker,
                )
            }
            None => (first_line, ring_span, raw_bytes, None),
        };

        // Compress in spawn_blocking. ~10 MiB of log compresses in ~50ms on
        // modern hardware; not long enough to matter for latency, but long
        // enough to hog a tokio worker thread under heavy log volume
        // (50 active derivations × 50ms = 2.5s of worker-thread time per
        // periodic tick, spread across tokio's NUM_CPU workers). The
        // recovered prefix (already zstd) is streamed through a decoder
        // into the same encoder — no full decompressed materialization.
        let compressed = match tokio::task::spawn_blocking(move || {
            compress_with_prefix(
                recovered_prefix.as_ref().map(|p| p.compressed.as_slice()),
                gap_marker.as_deref(),
                &lines,
            )
        })
        .await
        {
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
            first_line = eff_first_line,
            line_count = eff_line_count,
            recovered_prefix_lines = ?prefix_lines_recovered,
            compressed_size,
            is_final,
            "log flushed to S3"
        );
        metrics::counter!("rio_scheduler_log_flush_total", "kind" => if is_final { "final" } else { "periodic" }).increment(1);

        // PG UPSERT — one row per execution, keyed on exec_id. Periodic
        // and final flushes UPSERT the same row (is_complete flips
        // false→true on the final, and never back: the conflict clause
        // refuses the downgrade, so a periodic UPSERT that loses a race
        // with a final is a logged no-op, not corruption).
        let row_written = match upsert_drv_log(
            &self.pool,
            exec_id,
            &drv_hash,
            &s3_key,
            eff_first_line,
            eff_line_count,
            eff_total_bytes,
            is_final,
            req.status.as_deref(),
        )
        .await
        {
            Ok(written) => written,
            Err(e) => {
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
        };

        // A REFUSED periodic UPSERT (frozen row — another tenure already
        // finalized this execution) is the recurring "finalized elsewhere"
        // signal for an orphaned ring entry: reap the entry iff it is still
        // SEALED and stamped with the exec this snapshot was taken for.
        // Still-sealed means no restamp in the current tenure adopted it as
        // the live carrier (every restamp clears the seal) — typically a
        // prior tenure's terminal orphan with no other reaper left
        // (recovery restamps only Assigned|Running drvs, the acquisition
        // sweep skips sealed keys, terminal cleanup skips marked entries,
        // and `flush_final`'s out-of-tenure drop arm deliberately does no
        // PG work). It can also be an entry the CURRENT tenure's epilogue
        // re-sealed with its own final still pending — reaping it is still
        // safe, because the row is already frozen complete, so those lines
        // were unpersistable and that final's own already-finalized arm
        // would have drained and discarded them anyway (it then resolves
        // via its no-entry arm). An UNSEALED entry with a refused UPSERT is
        // the live execution's carrier (the documented post-drain residual:
        // a stale final froze the row while the same exec keeps streaming
        // here) and is left alone. The reap ends the per-tick `.partial`
        // re-PUT churn for the orphan and lets reads fall through to the
        // finalized blob; the `.partial` this tick uploaded is swept by the
        // TTL GC at expiry. Finals never take this branch: their entry was
        // already drained before the upload.
        if !is_final
            && !row_written
            && self
                .buffers
                .discard_if_sealed_for_exec(&req.drv_path, exec_id, false)
        {
            debug!(
                drv = %req.drv_path,
                exec_id = %exec_id,
                "periodic snapshot refused (execution already finalized by \
                 another tenure): reaped the sealed orphan entry so reads \
                 fall through to the durable record"
            );
        }

        // Best-effort delete the `.partial` snapshot AFTER the final blob's
        // PG row landed — the final supersedes it. A failed delete leaves a
        // stale `.partial` that the TTL GC sweep catches at expiry; that's
        // an accepted residual leak (bounded by retention), not an error.
        // Exception: `preserve_partial` (the final could not re-read a
        // known stored prefix) — the `.partial` is the prefix's only copy,
        // so it is deliberately left for operator recovery / TTL sweep.
        if is_final && !preserve_partial {
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

    /// Metadata-only close for a final flush whose drain yielded zero lines.
    ///
    /// There is nothing to compress or PUT, but if a periodic snapshot of this
    /// execution already produced a `.partial` blob and a `drv_logs` row
    /// (typically on the ex-leader, before a failover handed the empty
    /// re-stamped buffer to this replica), that row gets its terminal stamps:
    /// `status` and `finished_at`. It does NOT get `is_complete = true` — the
    /// `.partial` snapshot is the execution's only stored content and is missing
    /// everything after the ex-leader's last periodic flush, so the read path
    /// must keep serving it with `is_complete=false` and the CLI/dashboard
    /// incomplete indicator stays visible (`obs.log.incomplete-surfaced`).
    /// "No further upload is coming" is deliberately NOT encoded anywhere: no
    /// consumer needs it (the GC sweep has no `is_complete` discriminator and
    /// no client re-polls a stored log).
    ///
    /// A targeted UPDATE — not [`upsert_drv_log`] — so the content columns
    /// (`s3_key`, `first_line`, `line_count`, `total_bytes`) keep describing
    /// the `.partial` blob, which is deliberately NOT deleted here. The TTL GC
    /// sweep reconstructs both S3 keys from `(exec_id, drv_hash)`, so the
    /// `.partial` blob is still swept at expiry.
    ///
    /// Because the row stays `is_complete=false`, it is NOT protected by
    /// `upsert_drv_log`'s monotonicity latch: a late periodic UPSERT from a
    /// still-alive ex/re-acquired leader that retained this entry would
    /// refresh the content columns to a newer snapshot and re-null
    /// `status`/`finished_at`. Accepted — nothing in production reads those
    /// two columns, the served content only gets fresher, and the row stays
    /// flagged incomplete either way (see the leader-gate comments above).
    ///
    /// 0 rows affected ⇒ no periodic flush ever ran for this execution ⇒ the
    /// worker never streamed a line to any leader ⇒ there is no log and there
    /// must be no row (a row pointing at a blob that was never uploaded turns
    /// the read path's "no log found" into an S3 404). 0 rows is also returned
    /// when the row is already finalized — the empty drain is then stale
    /// residue from another leader's tenure and must not restamp
    /// `status`/`finished_at` (`obs.log.finalize-immutable`).
    ///
    /// On failure the row keeps whatever it had (still `is_complete=false`,
    /// `status` possibly NULL) until the TTL sweep — content is intact either
    /// way. The failure is reported at `warn!` on
    /// `rio_scheduler_log_empty_drain_finalize_failures_total`, NOT through
    /// [`flush_failure`]: that chokepoint's `is_final=true` arm is the
    /// post-drain data-loss alert, and nothing here was drained or lost
    /// (round-13 bug_008 made the same split for the pre-drain lookup sites).
    /// The stamp is deliberately not retried: the entry is already drained
    /// (a re-enqueued FlushRequest would look like a stale duplicate and be
    /// dropped at the drain-miss arm), nothing in production reads
    /// `status`/`finished_at`, and the served behavior — `.partial` content
    /// with the incomplete indicator — is identical with or without it.
    // r[impl obs.log.finalize-immutable]
    async fn finalize_empty_drain(&self, req: &FlushRequest) {
        match sqlx::query(
            "UPDATE drv_logs
             SET status = $2, finished_at = now()
             WHERE exec_id = $1 AND NOT is_complete",
        )
        .bind(req.exec_id)
        .bind(req.status.as_deref())
        .execute(&self.pool)
        .await
        {
            Ok(r) => {
                debug!(
                    exec_id = %req.exec_id,
                    drv_path = %req.drv_path,
                    status = ?req.status,
                    rows_affected = r.rows_affected(),
                    "empty final drain: stamped status/finished_at on the prior .partial \
                     drv_logs row; is_complete stays false — stored content is truncated \
                     at the last periodic snapshot (0 rows = never streamed, or already \
                     finalized by another leader and left untouched)"
                );
            }
            Err(e) => {
                // Zero lines drained; any prior `.partial` blob / `drv_logs`
                // row is untouched and still served (`is_complete=false`).
                // Deliberately NOT `flush_failure()`: its `is_final=true` arm
                // is the post-drain data-loss alert and its `is_final=false`
                // arm promises a next-tick retry — neither is true here. Same
                // split as `prefix_fetch_failure` (round-13 bug_008); no-retry
                // rationale in the fn doc.
                let partial_key = log_s3_key(&req.drv_path, &req.exec_id, true);
                warn!(
                    exec_id = %req.exec_id,
                    drv_path = %req.drv_path,
                    status = ?req.status,
                    partial_key = %partial_key,
                    error = %e,
                    "empty final drain: status/finished_at stamp failed; nothing \
                     drained or lost — the prior .partial row (if any) keeps its \
                     content and stays is_complete=false; not retried (the row \
                     ages out at the TTL sweep)"
                );
                metrics::counter!("rio_scheduler_log_empty_drain_finalize_failures_total")
                    .increment(1);
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
    /// retried next tick). S3 DeleteObjects failure — request-level
    /// (`Err`) or per-key (a 200 whose `output.errors()` lists keys S3
    /// could not delete) — is logged and the pass continues: the PG rows
    /// are already gone, and re-running the query won't re-find them, so
    /// the orphan blobs are unreachable from PG. They're bounded by the
    /// same lifecycle-rule backstop.
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
                // PG rows already gone — undeleted blobs are unreachable
                // from PG, so they won't be re-tried. Bounded by the
                // S3 lifecycle-rule backstop. Log at warn (operator
                // should know S3 deletes are failing) but don't abort
                // the pass — there may be more expired rows to sweep.
                // Two failure shapes: the whole request failing (`Err` —
                // transport/auth) and a 200 response that lists the keys
                // it could NOT delete (`output.errors()` — KMS denied,
                // Object Lock, transient backend). With `quiet(true)` the
                // response body carries only the failed keys.
                match self
                    .s3
                    .delete_objects()
                    .bucket(&self.bucket)
                    .delete(delete)
                    .send()
                    .await
                {
                    Err(e) => {
                        warn!(
                            error = %e,
                            keys = chunk.len(),
                            "log GC sweep: S3 DeleteObjects failed \
                             (orphan blobs; lifecycle rule is the backstop)"
                        );
                    }
                    Ok(output) if !output.errors().is_empty() => {
                        let errs = output.errors();
                        warn!(
                            failed = errs.len(),
                            keys = chunk.len(),
                            first_key = errs[0].key().unwrap_or("<unknown>"),
                            first_code = errs[0].code().unwrap_or("<unknown>"),
                            "log GC sweep: S3 DeleteObjects had per-key failures \
                             (orphan blobs; lifecycle rule is the backstop)"
                        );
                    }
                    Ok(_) => {}
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
/// `error!`s every 30s when nothing was lost. Pre-drain stored-coverage
/// lookup failures are reported by [`prefix_fetch_failure`] instead — keep
/// them separate: this fn's `is_final=true` arm is the data-loss alert
/// signal. The empty-drain finalization stamp failure
/// ([`LogFlusher::finalize_empty_drain`]'s `Err` arm — zero lines drained,
/// nothing lost) is likewise kept off this chokepoint, on
/// `rio_scheduler_log_empty_drain_finalize_failures_total`.
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

/// Log + metric for a stored-coverage lookup failure inside
/// [`LogFlusher::lookup_stored_prefix`] (the `drv_logs` point-SELECT, or the
/// S3 GET of a prior tenure's `.partial` blob). Deliberately NOT routed
/// through [`flush_failure`]: that chokepoint's `is_final=true` arm means
/// "the buffer was already drained and its data is gone" and feeds the
/// data-loss alert (`rio_scheduler_log_flush_failures_total`), while this
/// lookup runs BEFORE any drain or delete and the caller degrades without
/// losing anything (`obs.log.stored-coverage-preserved`): the periodic
/// snapshot is skipped and retried next tick; the final flush uploads the
/// drained ring without the stored prefix and skips the `.partial` delete.
/// Routing these through `flush_failure` emitted a false "log is lost"
/// error and tripped the loss alert for flushes that fully succeeded
/// (round-13 bug_008).
fn prefix_fetch_failure(
    is_final: bool,
    phase: &'static str,
    s3_key: &str,
    error: &dyn std::fmt::Display,
    drv_path: &str,
) {
    metrics::counter!(
        "rio_scheduler_log_prefix_fetch_failures_total",
        "phase" => phase,
        "is_final" => if is_final { "true" } else { "false" },
    )
    .increment(1);
    if is_final {
        warn!(
            s3_key = %s3_key, error = %error, is_final, phase,
            "stored-coverage lookup failed; final flush for {drv_path} \
             continues without the stored prefix and the .partial delete is \
             skipped (pre-drain failure — nothing drained or lost by this step)"
        );
    } else {
        warn!(
            s3_key = %s3_key, error = %error, is_final, phase,
            "stored-coverage lookup failed; periodic snapshot for {drv_path} \
             skipped this tick, retried next tick (nothing overwritten)"
        );
    }
}

/// Zstd-compress lines, joined by `\n`. Returns the compressed bytes.
///
/// Test-facing thin wrapper over [`compress_with_prefix`] (production goes
/// through the latter directly). Kept so tests can build expected bodies
/// and mock `.partial` blobs that are byte-identical to what a no-prefix
/// flush produces — the recovered-prefix tests depend on that equivalence.
#[cfg(test)]
fn compress_lines(lines: &[Vec<u8>]) -> std::io::Result<Vec<u8>> {
    compress_with_prefix(None, None, lines)
}

/// Zstd-compress an optional already-compressed prefix, an optional gap
/// marker line, and the ring lines into one frame.
///
/// `prefix_compressed` (the prior leader's `.partial` blob, itself produced
/// by [`compress_lines`]) is streamed through a decoder into the encoder —
/// no full decompressed materialization. Its content always ends with `\n`,
/// so plain concatenation preserves line boundaries.
///
/// Standalone fn so spawn_blocking can take it without capturing `self`.
fn compress_with_prefix(
    prefix_compressed: Option<&[u8]>,
    gap_marker: Option<&[u8]>,
    lines: &[Vec<u8>],
) -> std::io::Result<Vec<u8>> {
    // Level 6 (NOT the crate default 3): log text is already highly
    // compressible (~10:1 on typical build output), and the periodic
    // flush re-uploads ever-growing prefixes — the extra ratio at 6 is
    // worth the CPU on a path that's already off-thread in spawn_blocking.
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 6)?;
    if let Some(prefix) = prefix_compressed {
        let mut decoder = zstd::stream::Decoder::new(prefix)?;
        std::io::copy(&mut decoder, &mut encoder)?;
    }
    if let Some(marker) = gap_marker {
        encoder.write_all(marker)?;
        encoder.write_all(b"\n")?;
    }
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
///
/// `is_complete` is not just monotone — once `true`, the row is **frozen**
/// through this path: the `DO UPDATE`'s `WHERE` clause refuses ANY update
/// to a finalized row, covering both the periodic true→false downgrade
/// (with its `s3_key`/`status`/`finished_at` clobber) and a second "final"
/// true→true rewrite (an ex-leader's retained stale entry across an A→B→A
/// lease flap re-finalizing an exec the interim leader already finalized).
/// A periodic snapshot is by definition stale relative to any final flush
/// for the same execution — an ex-leader's still-running sweep, or a
/// periodic tick queued behind a final, must not downgrade the row the
/// final wrote, because `flush_final` drains the buffer and nothing would
/// ever repair it; and a stale re-finalization is refused upstream by
/// `flush_final`'s already-finalized guard, with this clause as the
/// defense-in-depth backstop. A refused write is observed via
/// `rows_affected() == 0`, logged at `debug!`, and reported to the caller
/// as `Ok(false)` (`Ok(true)` ⇒ the row was inserted/updated):
/// `upload_and_record`'s periodic path uses that signal to reap a sealed
/// orphan ring entry whose execution another tenure already finalized.
// r[impl obs.log.finalize-immutable]
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
) -> sqlx::Result<bool> {
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

    let result = sqlx::query(
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
             finished_at = EXCLUDED.finished_at
         WHERE NOT drv_logs.is_complete",
    )
    .bind(exec_id)
    .bind(drv_hash)
    .bind(s3_key)
    // Clamp before the i64 casts — same precedent as
    // build_samples.peak_memory_bytes (completion.rs). The ingestion gate
    // bounds ordering, not magnitude: a worker that claims line numbers,
    // spans, or byte totals ≥ 2^63 would otherwise wrap these BIGINTs
    // negative (corrupting s3_is_caught_up and the physical-vs-claimed
    // re-serve), and a post-failover gate reset can let the prefix-arm sum
    // reach u64::MAX. Clamping both halves to i64::MAX also guarantees
    // first_line + line_count ≤ u64::MAX − 1 for every recorded row, so the
    // read-side adds (s3_is_caught_up, lookup_stored_prefix /
    // reconcile_stored_prefix stored_end) can never wrap either.
    // r[impl sched.executor.input-bounds+2]
    .bind(first_line.min(i64::MAX as u64) as i64)
    .bind(line_count.min(i64::MAX as u64) as i64)
    .bind(total_bytes.min(i64::MAX as u64) as i64)
    .bind(is_complete)
    .bind(status)
    .bind(started_at_epoch)
    .execute(pool)
    .await?;
    // rows_affected()==0 has exactly one cause for an INSERT … ON CONFLICT
    // DO UPDATE: the conflict fired and the WHERE refused the update — i.e.
    // the row is already finalized and this write would have rewritten it.
    // That is a periodic snapshot racing a final flush for the same
    // execution (an ex-leader's sweep landing after the new leader
    // finalized the drv, or a queued periodic landing after a final), or a
    // stale "final" for an exec another leader already finalized that got
    // past `flush_final`'s already-finalized guard (a concurrent
    // finalization landing after the guard's lookup read the row as
    // unfinalized — the lookup-error case now defers instead of
    // proceeding). Working as designed in both cases, but only the
    // periodic case leaves an orphaned blob for the TTL GC to sweep at
    // expiry (its PUT went to a `.partial` key the finalized row does not
    // reference). A refused stale final already uploaded to the same
    // canonical `.log.zst` key the frozen row points at — no separate
    // orphan, and it may have replaced the finalized blob in place while
    // the frozen row still describes the replaced content; see
    // `flush_final`'s already-finalized guard comment and the
    // `obs.log.finalize-immutable` prose in observability.typ.
    if result.rows_affected() == 0 {
        debug!(
            %exec_id,
            s3_key,
            "drv_logs upsert refused: row is already finalized (finalized rows are frozen)"
        );
        return Ok(false);
    }
    Ok(true)
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

    /// Always-leader gate for tests: leader with recovery complete (the
    /// non-K8s `LeaderState::default()`), i.e. the self-driven arms'
    /// gate is open. The leader-gated arms only consult the gate in the
    /// spawned loop; tests that call `flush_periodic()` /
    /// `sweep_expired_logs()` directly bypass it either way.
    fn always_leader() -> crate::lease::LeaderState {
        crate::lease::LeaderState::default()
    }

    /// One captured counter series: `(metric name, sorted label pairs, value)`.
    type CounterSeries = (String, Vec<(String, String)>, u64);

    /// Capture-once dump of every counter the local recorder saw:
    /// `(metric name, sorted label pairs, value)`. Snapshotting drains the
    /// recorder (see `sla::metrics::counter_map_by`'s doc) — call this once
    /// per test and assert against the returned Vec.
    fn all_counters(snap: &metrics_util::debugging::Snapshotter) -> Vec<CounterSeries> {
        snap.snapshot()
            .into_vec()
            .into_iter()
            .filter_map(|(k, _, _, v)| {
                let metrics_util::debugging::DebugValue::Counter(c) = v else {
                    return None;
                };
                let mut labels: Vec<(String, String)> = k
                    .key()
                    .labels()
                    .map(|l| (l.key().to_string(), l.value().to_string()))
                    .collect();
                labels.sort();
                Some((k.key().name().to_string(), labels, c))
            })
            .collect()
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
                lease_generation: 1,
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
                lease_generation: 1,
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
                lease_generation: 1,
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
                lease_generation: 1,
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

    /// The accepted residual of the stale-request arm, pinned: when a
    /// `FlushRequest` for exec₁ goes stale (drv re-dispatched as exec₂
    /// before the flusher dequeued it), exec₁'s prior periodic `.partial`
    /// row is left exactly as the periodic flush wrote it — present,
    /// `is_complete=false`, `status IS NULL` — and is NOT finalized,
    /// deleted, or (worse) marked complete. Builds pinned to exec₁ via the
    /// write-once `bd.exec_id` are served that row with the incomplete
    /// banner; marking it complete here would suppress the banner for a
    /// log that is genuinely missing its post-snapshot tail. exec₂'s later
    /// final flush must touch only exec₂'s row.
    #[tokio::test]
    async fn flush_final_stale_request_leaves_prior_partial_row_untouched() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/bbbb-stalepartial.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // exec₁ runs and streams; a periodic tick snapshots it → `.partial`
        // blob + drv_logs(exec₁, is_complete=false, status NULL).
        let exec1 = stamp_and_push(&buffers, drv_path, &[b"exec1-line0", b"exec1-line1"]);
        flusher.flush_periodic().await;
        assert_eq!(puts.lock().unwrap().len(), 1, "periodic snapshot PUT");

        // exec₁ reaches a terminal (actor queues FlushRequest{exec₁}), but
        // before the flusher dequeues it the drv is re-dispatched:
        // assign_to_worker discards the buffer and stamps exec₂.
        buffers.discard(drv_path);
        let exec2 = stamp_and_push(&buffers, drv_path, &[b"exec2-line0"]);

        // The stale exec₁ final arrives now and must be a pure no-op on PG
        // and S3: exec₁'s periodic row stays exactly as written.
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id: exec1,
                status: Some("failed".into()),
                lease_generation: 1,
            })
            .await;
        assert_eq!(puts.lock().unwrap().len(), 1, "stale final must not PUT");
        let rows: Vec<(Uuid, bool, Option<String>)> =
            sqlx::query_as("SELECT exec_id, is_complete, status FROM drv_logs ORDER BY exec_id")
                .fetch_all(&db.pool)
                .await?;
        assert_eq!(rows.len(), 1, "only exec₁'s periodic row exists");
        assert_eq!(rows[0].0, exec1);
        assert!(
            !rows[0].1,
            "stale final must NOT mark exec₁'s truncated row complete (incomplete banner stays)"
        );
        assert_eq!(rows[0].2, None, "stale final must not stamp exec₁'s status");

        // exec₂'s legitimate final flush touches only exec₂'s row; exec₁'s
        // row still reads as the incomplete periodic snapshot it is.
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id: exec2,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;
        let rows: Vec<(Uuid, bool, Option<String>)> =
            sqlx::query_as("SELECT exec_id, is_complete, status FROM drv_logs ORDER BY started_at")
                .fetch_all(&db.pool)
                .await?;
        assert_eq!(rows.len(), 2, "exec₂'s final adds its own row");
        let e1 = rows
            .iter()
            .find(|r| r.0 == exec1)
            .expect("exec₁ row present");
        let e2 = rows
            .iter()
            .find(|r| r.0 == exec2)
            .expect("exec₂ row present");
        assert!(!e1.1, "exec₁ stays is_complete=false after exec₂'s final");
        assert_eq!(e1.2, None, "exec₁ status stays NULL after exec₂'s final");
        assert!(e2.1, "exec₂'s own final completes exec₂'s row");
        assert_eq!(e2.2.as_deref(), Some("succeeded"));
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
                lease_generation: 1,
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
                lease_generation: 1,
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
                lease_generation: 1,
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

    /// Leader failover leaves the new leader with an empty ring-buffer
    /// entry re-stamped to the in-flight execution (`recover_from_pg` →
    /// `set_log_exec`), while the ex-leader's periodic flusher already
    /// wrote a `.partial` blob and a `drv_logs` row at `is_complete=false`.
    /// If the drv terminates before the worker reconnects, the final drain
    /// yields zero lines — the flusher must stamp the terminal metadata
    /// (`status`/`finished_at`) on the existing row but leave
    /// `is_complete=false` (the `.partial` snapshot is truncated at the
    /// ex-leader's last periodic flush, so the incomplete indicator must
    /// stay visible), WITHOUT repointing `s3_key` at a `.log.zst` blob
    /// that was never uploaded and WITHOUT deleting the `.partial` blob
    /// (the only stored content).
    /// r[verify obs.log.exec-keyed]
    #[tokio::test]
    async fn flush_final_empty_drain_stamps_status_but_stays_incomplete() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/failoverempty-final.drv";

        // Ex-leader: the worker streamed a line and the periodic flusher
        // wrote the `.partial` blob + row.
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"streamed to the ex-leader"]);
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        flusher.flush_periodic().await;
        let snap: (bool, String, i64) = sqlx::query_as(
            "SELECT is_complete, s3_key, line_count FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(!snap.0, "fixture: periodic row starts incomplete");
        assert!(snap.1.ends_with(".partial.log.zst"), "fixture: {}", snap.1);
        assert_eq!(snap.2, 1, "fixture: periodic snapshot recorded the line");
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "fixture: periodic PUT happened"
        );

        // Failover: the new leader's process has no lines for this drv.
        // Recovery re-stamps an EMPTY entry with the SAME exec_id loaded
        // from `assignments`. Modeled as discard (the ex-leader's memory
        // is gone) + set_exec (recovery's restamp on the fresh standby) —
        // set_exec alone would NOT model this, a same-exec restamp keeps
        // the lines.
        buffers.discard(drv_path);
        buffers.set_exec(drv_path, exec_id, "test-worker");

        // The drv terminates before the worker reconnects.
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("failed".into()),
                lease_generation: 1,
            })
            .await;

        // Lifecycle columns flipped; content columns untouched.
        let row: (bool, Option<String>, String, i64, Option<f64>) = sqlx::query_as(
            "SELECT is_complete, status, s3_key, line_count, \
                    EXTRACT(EPOCH FROM finished_at)::float8 \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(
            !row.0,
            "empty final drain must NOT claim is_complete — the .partial content is \
             truncated at the ex-leader's last periodic snapshot and the incomplete \
             indicator (obs.log.incomplete-surfaced) must stay visible"
        );
        assert_eq!(row.1.as_deref(), Some("failed"), "status stamped");
        assert!(
            row.2.ends_with(".partial.log.zst"),
            "s3_key must keep pointing at the .partial blob (the only content): {}",
            row.2
        );
        assert_eq!(
            row.3, 1,
            "line_count from the periodic snapshot is preserved"
        );
        assert!(row.4.is_some(), "finished_at stamped");

        // Nothing new uploaded, and the .partial blob — the only stored
        // content for this execution — is not deleted.
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "empty final drain must not PUT"
        );
        assert!(
            deletes.lock().unwrap().is_empty(),
            "empty final drain must not delete the .partial blob"
        );
        // The entry is still drained (the execution is over).
        assert_eq!(buffers.active_count(), 0);
        Ok(())
    }

    /// An empty final drain for an execution that never produced a
    /// periodic snapshot (the worker never streamed a line to ANY leader
    /// — assigned then immediately poisoned/cancelled) has no `drv_logs`
    /// row to finalize. The metadata-only UPDATE must match zero rows and
    /// must NOT create one: a row whose `s3_key` names a blob that was
    /// never uploaded turns the read path's clean "no log found" into an
    /// S3 404. Pins the UPDATE-not-INSERT decision; passes both before
    /// and after the fix (the regression catch is the sibling test).
    #[tokio::test]
    async fn flush_final_empty_drain_without_prior_row_creates_nothing() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/norowempty-final.drv";
        // Stamped at dispatch, never pushed to, never periodically flushed.
        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");

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
                status: Some("cancelled".into()),
                lease_generation: 1,
            })
            .await;

        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(
            count.0, 0,
            "no row may be created for a never-streamed execution"
        );
        assert_eq!(
            buffers.active_count(),
            0,
            "the empty entry is still drained"
        );
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
                lease_generation: 1,
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

    /// `upsert_drv_log`'s conflict clause is monotone in `is_complete`: a
    /// periodic snapshot must never downgrade a row a final flush has
    /// already completed. The race this pins: an ex-leader's
    /// `flush_periodic` sweep is mid-flight when the lease flips; the new
    /// leader re-stamps the same `exec_id` from `assignments`, the worker
    /// completes there, and `flush_final` writes `is_complete=true` and
    /// drains the buffer — so the ex-leader's still-running iteration for
    /// that drv would be the LAST write for the 30-day TTL (no "next
    /// snapshot" ever repairs it). The latch refuses that write at the one
    /// chokepoint every flush goes through, for every caller and timing.
    /// Single-process simulation of the cross-replica write order: PG only
    /// sees the order of the UPSERTs, not which replica issued them.
    ///
    /// Anti-vacuousness: the row is proven finalized before the stale
    /// sweep; the stale sweep is proven to have reached the UPSERT (its
    /// `.partial` PUT is asserted — `upload_and_record` calls
    /// `upsert_drv_log` unconditionally after a successful PUT); the
    /// re-stamp reuses the SAME exec_id and the row count stays 1 (the
    /// stale write conflicted, it did not insert a sibling row); and the
    /// allowed direction (periodic refresh of an incomplete row) is pinned
    /// so an over-strict latch fails here too.
    #[tokio::test]
    async fn final_then_stale_periodic_does_not_downgrade_row() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/aaaa-latch.drv";
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"compiling"]);
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // Allowed direction: a second periodic snapshot still refreshes an
        // incomplete row (the latch's `NOT drv_logs.is_complete` disjunct).
        flusher.flush_periodic().await;
        buffers.push(&mk_batch(drv_path, 1, &[b"still compiling"]));
        flusher.flush_periodic().await;
        let partial: (bool, i64) =
            sqlx::query_as("SELECT is_complete, line_count FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert!(!partial.0);
        assert_eq!(
            partial.1, 2,
            "a periodic re-snapshot must still refresh an incomplete row"
        );

        // The build completes: the final flush drains and finalizes the row.
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;
        let fin: (bool, String) =
            sqlx::query_as("SELECT is_complete, s3_key FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert!(fin.0, "precondition: the final flush landed");
        assert!(fin.1.ends_with(".log.zst") && !fin.1.contains(".partial"));

        // The ex-leader's retained buffer for the SAME execution: recovery
        // re-stamps the same exec_id from `assignments`, and the worker's
        // stream kept feeding the ex-leader through the flap. Its periodic
        // sweep (already past the arm-level gate) now reaches this drv.
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.push(&mk_batch(drv_path, 0, &[b"stale line on the ex-leader"]));
        let puts_before = puts.lock().unwrap().len();
        flusher.flush_periodic().await;

        // The stale sweep DID reach the UPSERT: it PUT a `.partial` blob,
        // and upload_and_record calls upsert_drv_log unconditionally after
        // a successful PUT. Without this assert, a fixture whose entry was
        // skipped (unstamped / empty) would pass vacuously.
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(
            captured.len(),
            puts_before + 1,
            "the stale periodic sweep must reach the S3 PUT + PG UPSERT"
        );
        assert!(
            captured.last().unwrap().0.ends_with(".partial.log.zst"),
            "the stale write is a periodic `.partial` snapshot"
        );

        // …and the row did NOT move: the conflict clause refused the
        // is_complete true→false downgrade and, with it, the s3_key /
        // status / finished_at clobber.
        let row: (bool, String, Option<String>, Option<f64>) = sqlx::query_as(
            "SELECT is_complete, s3_key, status, \
                    EXTRACT(EPOCH FROM finished_at)::float8 \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(
            row.0,
            "stale periodic UPSERT must not downgrade is_complete"
        );
        assert!(
            row.1.ends_with(".log.zst") && !row.1.contains(".partial"),
            "stale periodic UPSERT must not repoint s3_key at the .partial"
        );
        assert_eq!(row.2.as_deref(), Some("succeeded"), "status survives");
        assert!(row.3.is_some(), "finished_at survives");

        // Still exactly one row — the stale write conflicted with (and was
        // refused by) the finalized row; it did not land under a new key.
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 1);
        Ok(())
    }

    /// The A→B→A flap shape: an interim leader (B) already finalized this
    /// execution's row and final `.log.zst`; the re-acquired ex-leader (A)
    /// still holds a retained ring entry stamped with the same exec_id
    /// (the drv was reset out of terminal, so the acquisition sweep kept
    /// it for the cancel-sweep finalization). The cancel-sweep's final
    /// flush must NOT re-finalize: the drained entry is stale pre-failover
    /// residue — drop it, leave B's blob and row untouched.
    ///
    /// Single-process simulation of the cross-replica write order (PG/S3
    /// only see the order of writes, not which replica issued them), same
    /// approach as `final_then_stale_periodic_does_not_downgrade_row`.
    /// r[verify obs.log.finalize-immutable]
    #[tokio::test]
    async fn flush_final_skips_already_finalized_exec() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/cccc-refinalize.drv";

        // (1) Interim leader B's tenure: the worker streamed three lines and
        // the build finished there — B's final flush PUTs the `.log.zst`,
        // finalizes the row, and deletes the `.partial`.
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"line0", b"line1", b"line2"]);
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
                lease_generation: 1,
            })
            .await;
        let baseline: (bool, String, Option<String>, i64, i64, Option<f64>) = sqlx::query_as(
            "SELECT is_complete, s3_key, status, line_count, total_bytes, \
                    EXTRACT(EPOCH FROM finished_at)::float8 \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(baseline.0, "fixture: B's final flush finalized the row");
        assert!(
            baseline.1.ends_with(".log.zst") && !baseline.1.contains(".partial"),
            "fixture: finalized s3_key: {}",
            baseline.1
        );
        assert_eq!(puts.lock().unwrap().len(), 1, "fixture: B's final PUT");
        assert_eq!(
            deletes.lock().unwrap().len(),
            1,
            "fixture: B's .partial delete"
        );

        // (2) The re-acquired ex-leader A's retained residue: same exec_id,
        // one stale pre-failover line at `first_line = 0` (`set_exec` +
        // `push` — NOT `stamp_and_push`, which would mint a different
        // exec_id).
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.push(&mk_batch(drv_path, 0, &[b"stale pre-failover line"]));
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "fixture: retained entry is stamped with the finalized exec"
        );

        // (3) The cancel-sweep finalization on A.
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("cancelled".into()),
                lease_generation: 1,
            })
            .await;

        // No second PUT over the finalized `.log.zst` — the
        // data-destruction assert.
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "must not re-PUT over the finalized final blob"
        );
        // The refusal makes no S3 calls at all (a `.partial` re-created by
        // this replica's periodic churn is left for the GC TTL sweep).
        assert_eq!(
            deletes.lock().unwrap().len(),
            1,
            "the refused flush must not issue S3 deletes"
        );
        // The row did not move at all.
        let row: (bool, String, Option<String>, i64, i64, Option<f64>) = sqlx::query_as(
            "SELECT is_complete, s3_key, status, line_count, total_bytes, \
                    EXTRACT(EPOCH FROM finished_at)::float8 \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row, baseline, "the finalized row must be untouched");
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count.0, 1);
        // The stale residue was reaped (drained), not left to shadow reads —
        // and reaching zero from one proves the drained path ran (the
        // stale-request arm leaves the entry; the no-buffer arm starts from
        // none), so the guard — not those arms — suppressed the upload.
        assert_eq!(buffers.exec_id(drv_path), None, "residue reaped");
        assert_eq!(buffers.active_count(), 0);
        Ok(())
    }

    /// The already-finalized guard's lookup-error arm must fail closed: a
    /// transient PG error on the guard's point-SELECT defers the final
    /// flush (nothing drained, nothing uploaded) instead of falling through
    /// to a destructive PUT over another leader's finalized blob — the
    /// A→B→A flap shape with one extra ingredient (PG turbulence at
    /// cancel-sweep time). The deferred request is also retained for retry
    /// (`obs.log.deferred-final-retry`); this test exercises only the single
    /// failed attempt.
    /// r[verify obs.log.finalize-immutable]
    #[tokio::test]
    async fn flush_final_guard_lookup_error_defers_and_preserves_finalized_blob()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r13grd1-guard-defer.drv";

        // (1) Interim leader B's tenure: the worker streamed three lines and
        // the build finished there — B's final flush PUTs the `.log.zst`,
        // finalizes the row, and deletes the `.partial`.
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"line0", b"line1", b"line2"]);
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
                lease_generation: 1,
            })
            .await;
        assert_eq!(puts.lock().unwrap().len(), 1, "fixture: B's final PUT");
        assert_eq!(
            deletes.lock().unwrap().len(),
            1,
            "fixture: B's .partial delete"
        );

        // (2) The re-acquired ex-leader A's retained residue: same exec_id,
        // one stale pre-failover line, sealed by the terminal epilogue
        // (production order: lines arrive, the epilogue seals, the request
        // follows).
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.push(&mk_batch(drv_path, 0, &[b"stale pre-failover line"]));
        buffers.seal(drv_path);

        // (3) PG turbulence exactly at cancel-sweep time: the guard's
        // point-SELECT fails. `close()` closes all clones, including the
        // flusher's.
        db.pool.close().await;
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("cancelled".into()),
                lease_generation: 1,
            })
            .await;

        // THE assertion: the stale residue did not overwrite B's finalized
        // blob (the old fall-through PUT a second body at the same key).
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "deferred final must not PUT over the finalized blob"
        );
        assert_eq!(
            deletes.lock().unwrap().len(),
            1,
            "deferred final must make no S3 calls"
        );
        // Deferred, not consumed: the entry stays stamped and sealed for the
        // periodic snapshotter / CleanupTerminalBuild.
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "entry left undrained"
        );
        assert!(buffers.is_sealed(drv_path), "seal left in place");
        // B's record untouched (fresh pool: `close()` killed the original).
        let pool = db.reopen().await;
        let row: (bool, String, i64) = sqlx::query_as(
            "SELECT is_complete, s3_key, line_count FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&pool)
        .await?;
        assert!(row.0, "row still finalized");
        assert!(
            row.1.ends_with(".log.zst") && !row.1.contains(".partial"),
            "finalized s3_key untouched: {}",
            row.1
        );
        assert_eq!(row.2, 3, "B's line_count untouched");
        Ok(())
    }

    /// A deferred final's entry stays snapshot-able by the periodic flusher:
    /// once PG recovers, the periodic sweep (which does not filter sealed
    /// entries) lands the full content at the `.partial` key with an
    /// `is_complete=false` row. Since r14 this is the *secondary* backstop —
    /// the primary recovery is the flusher's own retry of the retained
    /// request (`deferred_final_is_retried_and_uploads_after_pg_recovers`);
    /// what this test pins is that the deferral leaves the entry in a shape
    /// the periodic path can still serve.
    /// r[verify obs.log.finalize-immutable]
    #[tokio::test]
    async fn flush_final_guard_lookup_error_no_row_defers_to_periodic() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r13grd2-guard-defer-norow.drv";

        // Fast build: this final would have been the first-ever flush — no
        // drv_logs row exists yet. Lines arrive, the epilogue seals, the
        // FlushRequest follows.
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"only-flush-line0", b"line1"]);
        buffers.seal(drv_path);

        // The flusher whose pool is down at final-flush time (a separate
        // reopened pool, so the outage doesn't take `db.pool` down with it).
        let bad_pool = db.reopen().await;
        bad_pool.close().await;
        let flusher_bad = LogFlusher::new(
            s3.clone(),
            "test-bucket".into(),
            bad_pool,
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        flusher_bad
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;
        assert!(
            puts.lock().unwrap().is_empty(),
            "deferred final uploads nothing"
        );
        assert!(
            deletes.lock().unwrap().is_empty(),
            "deferred final makes no S3 calls"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "entry left undrained for the periodic snapshotter"
        );

        // PG recovers; the periodic loop (same buffers, healthy pool) is the
        // existing degraded mode that keeps the content durable.
        let flusher_ok = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        flusher_ok.flush_periodic().await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "exactly the periodic snapshot PUT");
        let (key, body) = &captured[0];
        assert!(
            key.ends_with(".partial.log.zst"),
            "content lands at the .partial key: {key}"
        );
        assert_eq!(
            String::from_utf8(zstd::decode_all(&body[..])?)?,
            "only-flush-line0\nline1\n",
            "no content lost to the deferred final"
        );
        let row: (bool, Option<String>) =
            sqlx::query_as("SELECT is_complete, status FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert!(!row.0, "row stays is_complete=false (incomplete indicator)");
        assert!(
            row.1.is_none(),
            "status never stamped for the deferred final"
        );
        Ok(())
    }

    /// The deferral is retained and retried, not abandoned: a final deferred
    /// because the guard's lookup failed is handed back to the loop, and the
    /// retried request — once PG answers — drains and finalizes exactly as a
    /// first-attempt final would (scenario 1 of r14 merged_bug_003: a fast
    /// build whose first-ever flush met a transient PG blip must not lose its
    /// whole log to the ~60s terminal-cleanup discard while S3 is healthy).
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn deferred_final_is_retried_and_uploads_after_pg_recovers() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r14m3a-fast-build.drv";

        // Fast build: lines arrive, the epilogue seals, the FlushRequest
        // follows. No drv_logs row exists yet (first-ever flush).
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"l0", b"l1"]);
        buffers.seal(drv_path);

        // PG is down at final-flush time.
        let bad_pool = db.reopen().await;
        bad_pool.close().await;
        let flusher_bad = LogFlusher::new(
            s3.clone(),
            "test-bucket".into(),
            bad_pool,
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let deferred = flusher_bad
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;

        // Deferred AND handed back for retry.
        let retained = deferred.expect("deferral must hand the request back for retry");
        assert_eq!(retained.exec_id, exec_id, "retained request pins the exec");
        assert_eq!(retained.drv_path, drv_path);
        assert!(puts.lock().unwrap().is_empty(), "deferral uploads nothing");
        assert!(
            deletes.lock().unwrap().is_empty(),
            "deferral makes no S3 calls"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "entry left undrained for the retry"
        );
        assert!(buffers.is_sealed(drv_path), "seal left in place");
        assert!(
            buffers.final_pending(drv_path),
            "entry marked so terminal cleanup leaves it to the retry"
        );

        // PG recovers; the retried request finalizes like a first attempt.
        let flusher_ok = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let again = flusher_ok.flush_final(retained).await;
        assert!(again.is_none(), "retry resolves; nothing left to retain");

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "exactly the final PUT");
        let (key, body) = &captured[0];
        assert!(
            key.ends_with(".log.zst") && !key.contains(".partial"),
            "retried final lands at the final key: {key}"
        );
        assert_eq!(
            String::from_utf8(zstd::decode_all(&body[..])?)?,
            "l0\nl1\n",
            "no content lost across the deferral"
        );
        let row: (bool, Option<String>, i64) = sqlx::query_as(
            "SELECT is_complete, status, line_count FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(row.0, "row finalized by the retry");
        assert_eq!(row.1.as_deref(), Some("succeeded"));
        assert_eq!(row.2, 2);
        assert_eq!(
            buffers.exec_id(drv_path),
            None,
            "retry drained the entry (its reaper)"
        );
        assert!(!buffers.is_sealed(drv_path), "retry unsealed");
        Ok(())
    }

    /// Scenario 2 of r14 merged_bug_003, amended for the r16 pin-first
    /// ordering: on a deposed leader the periodic snapshot sweep is
    /// leader-gated, so the deferred-final retry must run OUTSIDE that gate
    /// — otherwise a request deferred just before deposition would sit in
    /// the retained vec (pinning its sealed entry) for as long as the
    /// replica stays standby. The retry itself is validated per-attempt
    /// inside `flush_final`: with the tenure pin checked before the
    /// finalize guard, the orphaned request is dropped on the first
    /// post-deposition tick without needing PG (which never heals here),
    /// and the non-empty entry is left in place for its real owner.
    ///
    /// Load-bearing assumption: a closed sqlx pool fails `acquire()`
    /// immediately without arming a tokio timer, which is what keeps this
    /// select loop from wedging under `tokio::time::pause()` (same
    /// assumption as the existing closed-pool deferral tests). If a future
    /// sqlx bump changes that, this test will hang/time out rather than
    /// pass vacuously.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn deferred_final_retry_loop_runs_when_not_leader() -> anyhow::Result<()> {
        // TestDb before pause(): sqlx pool acquire uses tokio::time for its
        // timeout, which PoolTimedOuts under paused time.
        let db = TestDb::new(&crate::MIGRATOR).await;
        // Take PG down for the whole test: every clone of db.pool is closed.
        db.pool.close().await;
        tokio::time::pause();

        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r14m3b-deposed-retry.drv";
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"pre-flap line"]);
        buffers.seal(drv_path);

        // Leader (generation 1) at enqueue time; deposed below, after the
        // in-tenure first attempt has deferred.
        let state = crate::lease::LeaderState::default();
        let (tx, rx) = mpsc::channel::<FlushRequest>(8);
        LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state.clone(),
        )
        .spawn(rx);

        tx.send(FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("succeeded".into()),
            lease_generation: state.generation(),
        })
        .await?;

        // Wait for the in-tenure first attempt to defer (PG is down, the
        // entry is non-empty, the request is retained) before deposing.
        tokio::time::timeout(Duration::from_secs(20), async {
            while !logs_contain("deferring final flush") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;

        // Deposed: the retained request's tenure is over.
        state.on_lose();

        // Auto-advance drives the interval: the ticks at T=+30/+60/+90 run
        // retry_deferred even though is_leader is false (and skip the
        // leader-gated snapshot sweep); the first one resolves the orphaned
        // request by dropping it at the tenure pin — no PG read needed.
        tokio::time::sleep(Duration::from_secs(95)).await;

        logs_assert(|lines: &[&str]| {
            let deferrals = lines
                .iter()
                .filter(|l| l.contains("deferring final flush"))
                .count();
            let drops = lines
                .iter()
                .filter(|l| {
                    l.contains(
                        "dropping final log flush enqueued under a previous leadership tenure",
                    )
                })
                .count();
            if deferrals >= 1 && drops >= 1 {
                Ok(())
            } else {
                Err(format!(
                    "expected an in-tenure deferral then a not-leader tick retry \
                     that drops it; got {deferrals} deferral(s), {drops} drop(s)"
                ))
            }
        });
        assert!(
            puts.lock().unwrap().is_empty(),
            "PG never recovered and the orphaned request was dropped → nothing uploaded"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "non-empty entry left untouched by the not-leader drop"
        );
        drop(tx);
        Ok(())
    }

    /// Scenario 3 of r14 merged_bug_003: a deferral that finds only an empty
    /// (failover-restamped, never-streamed) entry reaps it — exec-guarded —
    /// instead of retaining it: nothing any retry could upload, and keeping
    /// it would only pin memory and the final-pending mark until build
    /// cleanup (reads are unaffected — `GetDerivationLogs` probes the
    /// ex-leader's stored `.partial` for a zero-line entry either way). A
    /// re-dispatched execution's fresh empty entry is never touched by a
    /// stale deferred request.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn deferred_final_with_empty_entry_reaps_it_for_this_exec_only() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());

        let bad_pool = db.reopen().await;
        bad_pool.close().await;
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            bad_pool,
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // (1) The scenario-3 shape: recovery restamped the exec onto a fresh
        // standby's empty entry, the worker never re-streamed, the drv
        // reached terminal (sealed), and the guard SELECT then failed.
        let drv_a = "/nix/store/r14m3c-empty-restamp.drv";
        let exec_a = Uuid::now_v7();
        buffers.set_exec(drv_a, exec_a, "w");
        buffers.seal(drv_a);
        let ret = flusher
            .flush_final(FlushRequest {
                drv_path: drv_a.into(),
                exec_id: exec_a,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;
        assert!(ret.is_none(), "nothing to retry for an empty entry");
        assert!(puts.lock().unwrap().is_empty(), "no S3 calls");
        assert_eq!(
            buffers.exec_id(drv_a),
            None,
            "deferred empty entry must be reaped (bookkeeping; nothing to retain for retry)"
        );
        assert!(
            !buffers.is_sealed(drv_a),
            "seal tombstone cleared with the reaped entry"
        );
        assert!(
            buffers.read_since(drv_a, 0).is_none(),
            "reaped entry leaves no ring state behind"
        );

        // (2) Exec guard: a stale deferred request must not reap the NEW
        // execution's freshly-stamped (still empty) entry.
        let drv_b = "/nix/store/r14m3d-redispatch-guard.drv";
        let exec_old = Uuid::now_v7();
        buffers.set_exec(drv_b, exec_old, "w");
        // Re-dispatch: discard + fresh stamp, worker hasn't streamed yet.
        buffers.discard(drv_b);
        let exec_new = Uuid::now_v7();
        buffers.set_exec(drv_b, exec_new, "w");
        let ret = flusher
            .flush_final(FlushRequest {
                drv_path: drv_b.into(),
                exec_id: exec_old,
                status: Some("cancelled".into()),
                lease_generation: 1,
            })
            .await;
        assert!(
            ret.is_none(),
            "stale request resolves with nothing to retry"
        );
        assert_eq!(
            buffers.exec_id(drv_b),
            Some(exec_new),
            "the new execution's empty entry was NOT reaped by the stale request"
        );
        Ok(())
    }

    /// Retention-cap behavior of the loop-side helpers: the cap bounds the
    /// retry queue, an overflow drops the execution's buffered entry outright
    /// (terminal cleanup may already have run and skipped it on the
    /// enqueue-time final-pending mark, so nothing else would ever reap it),
    /// and duplicate exec_ids do not consume cap slots.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn deferred_final_retention_cap_drops_entry_on_overflow() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, _puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // The overflow drv has a real, marked, sealed entry — the
        // precondition that makes the "entry dropped on overflow" assertions
        // non-vacuous.
        let drv_o = "/nix/store/r14m3e-overflow.drv";
        let exec_o = stamp_and_push(&buffers, drv_o, &[b"line"]);
        buffers.seal(drv_o);
        assert!(buffers.mark_final_pending(drv_o, exec_o));
        assert!(buffers.final_pending(drv_o), "precondition: marked");

        // Fill the queue to the cap with distinct exec_ids (no ring entries
        // needed — retain_deferred only inspects the Vec and the mark).
        let mut deferred: Vec<FlushRequest> = Vec::new();
        for i in 0..DEFERRED_FINALS_MAX {
            flusher.retain_deferred(
                &mut deferred,
                FlushRequest {
                    drv_path: format!("/nix/store/r14fill{i}-x.drv"),
                    exec_id: Uuid::now_v7(),
                    status: None,
                    lease_generation: 1,
                },
            );
        }
        assert_eq!(deferred.len(), DEFERRED_FINALS_MAX);

        // Dedup: an already-retained exec_id does not grow the queue.
        let dup_exec = deferred[0].exec_id;
        flusher.retain_deferred(
            &mut deferred,
            FlushRequest {
                drv_path: "/nix/store/r14dup-x.drv".into(),
                exec_id: dup_exec,
                status: None,
                lease_generation: 1,
            },
        );
        assert_eq!(deferred.len(), DEFERRED_FINALS_MAX, "dedup by exec_id");
        assert_eq!(
            deferred[0].lease_generation, 1,
            "same-generation duplicate neither grows the queue nor replaces"
        );

        // Same exec, NEWER tenure: the retained element is replaced (the
        // older request is already dead under the tenure check — keeping it
        // would shadow the only request that can still finalize the row),
        // still without consuming a second slot.
        flusher.retain_deferred(
            &mut deferred,
            FlushRequest {
                drv_path: "/nix/store/r14dup-x.drv".into(),
                exec_id: dup_exec,
                status: Some("cancelled".into()),
                lease_generation: 2,
            },
        );
        assert_eq!(
            deferred.len(),
            DEFERRED_FINALS_MAX,
            "newer-tenure duplicate does not consume a slot"
        );
        assert_eq!(
            deferred[0].lease_generation, 2,
            "newer-tenure request replaces the retained one for the same exec"
        );

        // Overflow: the request is dropped and the entry is dropped with it
        // (cleanup may already have skipped it on the enqueue-time mark).
        flusher.retain_deferred(
            &mut deferred,
            FlushRequest {
                drv_path: drv_o.into(),
                exec_id: exec_o,
                status: Some("succeeded".into()),
                lease_generation: 1,
            },
        );
        assert_eq!(deferred.len(), DEFERRED_FINALS_MAX, "cap holds");
        assert_eq!(
            buffers.exec_id(drv_o),
            None,
            "overflow drops the entry — terminal cleanup may already have \
             skipped it on the enqueue-time mark, so nothing else would reap it"
        );
        assert!(
            !buffers.is_sealed(drv_o),
            "overflow clears the seal tombstone with the entry"
        );
        Ok(())
    }

    /// merged_bug_008 (r16): the cap-overflow drop is only allowed to drain
    /// the victim's entry when the victim request is still in tenure. An
    /// out-of-tenure victim's entry may be the LIVE execution's carrier
    /// (recovery restamps the same exec_id at re-acquisition), and the
    /// exec-guard alone cannot tell the two apart — the overflow arm must
    /// drop the request without touching the entry, exactly like the
    /// tenure-drop arm in `flush_final`.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn deferred_final_retention_cap_overflow_leaves_out_of_tenure_victims_entry()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, _puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());

        // Tenure 1: the victim execution streamed a line and reached a
        // terminal during the outage — sealed, enqueued (generation 1),
        // marked.
        let state = crate::lease::LeaderState::default();
        let drv_v = "/nix/store/r16b8b-overflow-victim.drv";
        let exec_v = stamp_and_push(&buffers, drv_v, &[b"pre-flap line"]);
        buffers.seal(drv_v);
        assert!(buffers.mark_final_pending(drv_v, exec_v));
        let victim = FlushRequest {
            drv_path: drv_v.into(),
            exec_id: exec_v,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        // The lease flaps; the same replica re-acquires and recovery
        // restamps the SAME exec — the victim's entry is now the live
        // execution's carrier (lines retained, unsealed, unmarked).
        state.on_lose();
        state.on_acquire(1);
        buffers.set_exec(drv_v, exec_v, "test-worker");

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state.clone(),
        );

        // The retained queue is already at the cap with current-tenure
        // deferrals (no ring entries needed — retain_deferred only inspects
        // the Vec for them).
        let mut deferred: Vec<FlushRequest> = Vec::new();
        for i in 0..DEFERRED_FINALS_MAX {
            flusher.retain_deferred(
                &mut deferred,
                FlushRequest {
                    drv_path: format!("/nix/store/r16fill{i}-x.drv"),
                    exec_id: Uuid::now_v7(),
                    status: None,
                    lease_generation: state.generation(),
                },
            );
        }
        assert_eq!(deferred.len(), DEFERRED_FINALS_MAX);

        // The pre-flap victim request overflows the cap: the request is
        // dropped, but its entry — the live carrier — must NOT be drained.
        flusher.retain_deferred(&mut deferred, victim);
        assert_eq!(deferred.len(), DEFERRED_FINALS_MAX, "cap holds");
        assert_eq!(
            buffers.exec_id(drv_v),
            Some(exec_v),
            "an out-of-tenure overflow victim must not drain the live carrier"
        );
        assert!(
            buffers.read_since(drv_v, 0).is_some_and(|l| l.len() == 1),
            "the live execution's retained lines survive the overflow drop"
        );
        assert!(
            !buffers.is_sealed(drv_v),
            "carrier stays unsealed for the live execution"
        );
        assert!(
            buffers.push_for(
                drv_v,
                &mk_batch(drv_v, 1, &[b"post-flap line"]),
                "test-worker"
            ),
            "the live execution's batches must still be accepted"
        );
        Ok(())
    }

    /// bug_009 (r15): A→B→A flap where the interim leader re-dispatched the
    /// drv under exec₂. The re-acquired ex-leader retains exec₁'s SEALED
    /// entry (its final was deferred → request retained, cleanup skipped the
    /// discard); recovery restamps it to exec₂. The restamp must drop the
    /// stale seal so exec₂'s batches are accepted and its final uploads the
    /// full log; an exec₁-pinned request that still reaches `flush_final`
    /// afterwards must resolve as stale without touching exec₂'s entry.
    ///
    /// The exec₁ request is replayed through `flush_final` directly under
    /// the same (always-leader, generation-1) tenure — the queued-request
    /// shape. A retained request whose enqueueing tenure has actually ended
    /// is dropped even earlier by the per-attempt tenure pin, leaving the
    /// entry/seal/mark untouched; this test pins the deeper exec-guards
    /// that back that drop up, plus the user-visible symptom (exec₂'s
    /// blob/row).
    #[tokio::test]
    async fn aba_flap_deferred_final_restamp_does_not_mute_redispatched_exec() -> anyhow::Result<()>
    {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r15b9b-redispatch.drv";

        // exec₁ on worker "test-worker" under leader A; terminal reached
        // while PG is down: epilogue seals + enqueues, the guard SELECT
        // fails, the request is retained and the entry marked.
        let exec1 = stamp_and_push(&buffers, drv_path, &[b"e1-l0", b"e1-l1"]);
        buffers.seal(drv_path);
        let bad_pool = db.reopen().await;
        bad_pool.close().await;
        let flusher_down = LogFlusher::new(
            s3.clone(),
            "test-bucket".into(),
            bad_pool,
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let retained = flusher_down
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id: exec1,
                status: Some("cancelled".into()),
                lease_generation: 1,
            })
            .await
            .expect("non-empty entry + unreadable drv_logs ⇒ deferred and retained");
        assert!(
            buffers.is_sealed(drv_path),
            "premise: entry sealed for exec₁'s deferred final"
        );
        assert!(
            buffers.final_pending(drv_path),
            "premise: cleanup would skip the discard"
        );

        // Interim leader B re-dispatched under exec₂ to worker-2; A
        // re-acquires and recovery restamps the retained entry from
        // assignments.
        let exec2 = Uuid::now_v7();
        buffers.set_exec(drv_path, exec2, "worker-2");

        // The restamp un-mutes the new execution.
        assert!(
            !buffers.is_sealed(drv_path),
            "cross-exec restamp must clear exec₁'s stale seal"
        );
        assert!(
            buffers.push_for(drv_path, &mk_batch(drv_path, 0, &[b"e2-l0"]), "worker-2"),
            "worker-2's exec₂ batches must be accepted after the restamp"
        );
        assert!(buffers.push_for(drv_path, &mk_batch(drv_path, 1, &[b"e2-l1"]), "worker-2"));

        // PG heals; the exec₁ request is replayed (queued-request shape):
        // stale, no writes, and it must not touch exec₂'s entry or its
        // (un)sealed state.
        let flusher_up = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        assert!(
            flusher_up.flush_final(retained).await.is_none(),
            "stale exec₁ final resolves without re-deferring"
        );
        assert!(
            puts.lock().unwrap().is_empty(),
            "stale exec₁ final must not PUT"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec2),
            "exec₂'s entry untouched"
        );
        assert!(
            !buffers.is_sealed(drv_path),
            "stale request must not (re)seal"
        );
        assert!(
            buffers
                .read_since(drv_path, 0)
                .is_some_and(|l| l.len() == 2),
            "exec₂'s lines still in the ring"
        );

        // exec₂ reaches terminal: seal + final flush uploads the FULL log.
        buffers.seal(drv_path);
        assert!(
            flusher_up
                .flush_final(FlushRequest {
                    drv_path: drv_path.into(),
                    exec_id: exec2,
                    status: Some("succeeded".into()),
                    lease_generation: 1,
                })
                .await
                .is_none()
        );
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "exactly exec₂'s final PUT");
        let (key, body) = &captured[0];
        assert!(
            key.contains(&exec2.to_string())
                && key.ends_with(".log.zst")
                && !key.contains(".partial")
        );
        assert_eq!(
            String::from_utf8(zstd::decode_all(&body[..])?)?,
            "e2-l0\ne2-l1\n",
            "the re-dispatched execution's log survives intact"
        );
        let row: (bool, Option<String>, i64) = sqlx::query_as(
            "SELECT is_complete, status, line_count FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec2)
        .fetch_one(&db.pool)
        .await?;
        assert!(row.0, "exec₂'s row finalized");
        assert_eq!(row.1.as_deref(), Some("succeeded"));
        assert_eq!(row.2, 2);
        assert!(
            !buffers.is_sealed(drv_path),
            "exec₂'s own final unseals after drain"
        );
        Ok(())
    }

    /// bug_001 (r16): re-acquired-leader same-exec timeline. Tenure 1 seals
    /// and enqueues a final for exec₁ during a PG/flusher backlog (the
    /// CancelSignal is dropped, so the worker keeps building); the lease
    /// flaps and the SAME replica re-acquires (generation bumps); recovery
    /// restamps the SAME exec_id onto the retained entry. The restamp must
    /// clear the prior tenure's seal (and mark): that tenure's final is
    /// tenure-dropped without unsealing, so nothing else ever would — a
    /// surviving seal mutes the reconnected worker's post-flap batches and
    /// the live tenure's own final then uploads only the pre-flap prefix as
    /// `is_complete=true`.
    #[tokio::test]
    async fn same_exec_restamp_after_flap_unmutes_worker_and_final_uploads_full_log()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r16b1b-same-exec-flap.drv";
        let drv_hash = "r16b1b";

        // Tenure 1 (leader, generation 1): exec₁ streams two lines, then a
        // build-level cancel runs the terminal epilogue: seal + enqueue
        // (stamped with generation 1) + final-pending mark.
        let state = crate::lease::LeaderState::default();
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state.clone(),
        );
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"pre-flap l0", b"pre-flap l1"]);
        buffers.seal(drv_path);
        let old_final = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };
        assert!(buffers.mark_final_pending(drv_path, exec_id));

        // The lease flaps; the same replica re-acquires (generation bumps)
        // and recovery restamps the SAME exec from `assignments` (the
        // terminal persist failed in the same outage, so PG still holds the
        // drv as Assigned/Running).
        state.on_lose();
        state.on_acquire(1);
        buffers.set_exec(drv_path, exec_id, "test-worker");

        // The reconnected worker keeps streaming the same execution: its
        // post-flap batches MUST be accepted.
        assert!(
            buffers.push_for(
                drv_path,
                &mk_batch(drv_path, 2, &[b"post-flap l2"]),
                "test-worker"
            ),
            "post-flap batch must be accepted after the same-exec restamp"
        );

        // The tenure-1 final is processed now: dropped by the tenure pin
        // without touching the live entry.
        assert!(flusher.flush_final(old_final).await.is_none());
        assert!(
            puts.lock().unwrap().is_empty(),
            "the stale tenure-1 final uploads nothing"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "the live entry survives the tenure drop"
        );

        // Tenure 2 reaches the execution's real terminal: seal + final.
        buffers.seal(drv_path);
        assert!(
            flusher
                .flush_final(FlushRequest {
                    drv_path: drv_path.into(),
                    exec_id,
                    status: Some("succeeded".into()),
                    lease_generation: state.generation(),
                })
                .await
                .is_none()
        );

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "exactly the live tenure's final PUT");
        let (key, body) = &captured[0];
        assert_eq!(key, &format!("logs/{drv_hash}/{exec_id}.log.zst"));
        assert_eq!(
            String::from_utf8(zstd::decode_all(&body[..])?)?,
            "pre-flap l0\npre-flap l1\npost-flap l2\n",
            "the final must carry pre-flap AND post-flap lines"
        );
        let row: (bool, Option<String>, i64) = sqlx::query_as(
            "SELECT is_complete, status, line_count FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(row.0, "the live tenure's final finalizes the row");
        assert_eq!(row.1.as_deref(), Some("succeeded"));
        assert_eq!(row.2, 3, "all three lines counted");
        Ok(())
    }

    /// Money test for the tenure pin, retry route (timeline A of r15
    /// bug_005): a leader cancels a build during a PG outage, the final
    /// defers, the lease moves, the live leader keeps extending the same
    /// `exec_id`'s `.partial` row, and only then does PG heal on the
    /// deposed replica. Its retained retry must NOT freeze the live
    /// leader's row, regress its coverage, stamp a stale terminal status,
    /// or delete the `.partial` the live leader owns — the orphaned
    /// request is dropped (counted by
    /// `rio_scheduler_log_flush_stale_tenure_total`) and the retained
    /// entry is left untouched for its real owners (the live tenure's own
    /// final, the drv's next dispatch discard, or process exit).
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn deposed_leader_deferred_retry_must_not_freeze_live_leaders_row() -> anyhow::Result<()>
    {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r15tg1-deposed-retry-freeze.drv";
        let drv_hash = "r15tg1";

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();

        // Tenure-1 state on the (about to be deposed) leader A: worker
        // streamed two lines, the cancel sealed the entry, and A's own
        // earlier periodic flush latched the prefix state Checked — the
        // latch is load-bearing: it is what makes the un-fixed code skip
        // the stored-coverage reconcile and take the destructive
        // PUT+UPSERT+delete path directly.
        let exec_id = stamp_and_push(
            &buffers,
            drv_path,
            &[b"pre-failover l0", b"pre-failover l1"],
        );
        buffers.seal(drv_path);
        buffers.mark_prefix_checked(drv_path, exec_id);

        // The live leader B has since extended the same execution's row:
        // `.partial` key, is_complete=false, 10 lines (8 ahead of A's ring).
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            10,
            1_000,
            false,
            None,
        )
        .await?;

        // The deposed flusher: healthy pool, NOT leader, generation 1.
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            crate::lease::LeaderState::pending(Arc::new(std::sync::atomic::AtomicU64::new(1))),
        );

        // The deferral exactly as production retains it: request enqueued
        // under tenure 1, entry marked final-pending.
        let mut deferred: Vec<FlushRequest> = Vec::new();
        flusher.retain_deferred(
            &mut deferred,
            FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("cancelled".into()),
                lease_generation: 1,
            },
        );
        assert!(buffers.mark_final_pending(drv_path, exec_id));

        {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher.retry_deferred(&mut deferred).await;
        }

        let row: (bool, i64, Option<String>, String) = sqlx::query_as(
            "SELECT is_complete, line_count, status, s3_key FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(
            !row.0,
            "live leader's row must NOT be frozen complete by a deposed leader's retry"
        );
        assert_eq!(
            row.1, 10,
            "live leader's coverage must not be regressed to the stale ring's span"
        );
        assert_eq!(
            row.2, None,
            "no stale terminal status stamped onto a live execution"
        );
        assert!(
            row.3.ends_with(".partial.log.zst"),
            "row must keep pointing at the live .partial: {}",
            row.3
        );
        assert!(
            puts.lock().unwrap().is_empty(),
            "no S3 PUT from the deposed leader's retry"
        );
        assert!(
            deletes.lock().unwrap().is_empty(),
            "the live .partial must not be deleted"
        );
        assert!(
            deferred.is_empty(),
            "orphaned request is dropped, not re-retained"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "entry not drained by the dropped request"
        );
        assert!(
            buffers
                .read_since(drv_path, 0)
                .is_some_and(|l| l.len() == 2),
            "retained ring lines left in place (a later re-acquisition may still fold them)"
        );
        assert!(
            buffers.is_sealed(drv_path),
            "seal left untouched by the drop"
        );
        assert!(
            buffers.final_pending(drv_path),
            "final-pending mark left in place — the entry still belongs to whichever \
             owner resolves it (live tenure's own final, next dispatch, or restart)"
        );
        // Counted as the dedicated tenure drop, not as an upload failure.
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .any(|(n, _, v)| n == "rio_scheduler_log_flush_stale_tenure_total" && *v >= 1),
            "tenure drop must be counted: {counters:?}"
        );
        assert!(
            !counters
                .iter()
                .any(|(n, _, _)| n == "rio_scheduler_log_flush_failures_total"),
            "a tenure drop is not an upload failure: {counters:?}"
        );
        Ok(())
    }

    /// Money test for the tenure pin, first-attempt route (timeline B of r15
    /// bug_005): a final still QUEUED when the lease moves gets its first
    /// `flush_final` attempt on the deposed replica after PG heals — it never
    /// enters the retry machinery at all, so the validation must live in
    /// `flush_final` itself. Same destructive outcome as the retry route on
    /// un-pinned code; same drop-without-touching-anything expectation.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn stale_first_attempt_on_deposed_leader_must_not_freeze_live_leaders_row()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r15tg2-deposed-first-attempt.drv";
        let drv_hash = "r15tg2";

        let exec_id = stamp_and_push(
            &buffers,
            drv_path,
            &[b"pre-failover l0", b"pre-failover l1"],
        );
        buffers.seal(drv_path);
        buffers.mark_prefix_checked(drv_path, exec_id);
        assert!(buffers.mark_final_pending(drv_path, exec_id));

        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            10,
            1_000,
            false,
            None,
        )
        .await?;

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            crate::lease::LeaderState::pending(Arc::new(std::sync::atomic::AtomicU64::new(1))),
        );

        let ret = flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("cancelled".into()),
                lease_generation: 1,
            })
            .await;
        assert!(ret.is_none(), "orphaned first attempt resolves as a drop");

        let row: (bool, i64, Option<String>, String) = sqlx::query_as(
            "SELECT is_complete, line_count, status, s3_key FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(
            !row.0,
            "live leader's row must NOT be frozen complete by a deposed leader's first attempt"
        );
        assert_eq!(row.1, 10, "live leader's coverage must not be regressed");
        assert_eq!(row.2, None, "no stale terminal status stamped");
        assert!(
            row.3.ends_with(".partial.log.zst"),
            "row must keep pointing at the live .partial: {}",
            row.3
        );
        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        assert!(
            deletes.lock().unwrap().is_empty(),
            "the live .partial must not be deleted"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "entry not drained by the dropped request"
        );
        assert!(
            buffers.is_sealed(drv_path),
            "seal left untouched by the drop"
        );
        assert!(
            buffers.final_pending(drv_path),
            "final-pending mark left in place"
        );
        Ok(())
    }

    /// The A→B→A resurrection edge that distinguishes the per-request tenure
    /// pin from any boolean "am I leader" gate (timeline C of r15 bug_005): a
    /// request enqueued in tenure 1 is processed after this same replica
    /// re-acquired the lease (tenure 2), where recovery has restamped the
    /// SAME exec_id and the execution is live again on this very replica. The
    /// stale `status="cancelled"` final must be dropped — the entry it would
    /// have drained is the live execution's buffer.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn stale_final_dropped_after_lease_flap_even_when_leader_again() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r15tg3-flap-resurrection.drv";
        let drv_hash = "r15tg3";

        // Tenure 1: leader, generation 1. Live entry for exec₁ — non-empty,
        // sealed, Checked (without lines/seal the un-fixed code would
        // degrade to a milder path and this test would not demonstrate the
        // freeze).
        let state = crate::lease::LeaderState::default();
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"tenure-1 l0", b"tenure-1 l1"]);
        buffers.seal(drv_path);
        buffers.mark_prefix_checked(drv_path, exec_id);

        // The row the live execution owns (extended past A's ring).
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            10,
            1_000,
            false,
            None,
        )
        .await?;

        // Enqueued in tenure 1...
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };
        // ...processed after a lose/re-acquire flap: tenure 2, leader again.
        state.on_lose();
        state.on_acquire(1);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state,
        );
        let mut deferred: Vec<FlushRequest> = Vec::new();
        flusher.retain_deferred(&mut deferred, req);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        flusher.retry_deferred(&mut deferred).await;

        let row: (bool, i64, Option<String>, String) = sqlx::query_as(
            "SELECT is_complete, line_count, status, s3_key FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(
            !row.0,
            "tenure-1 final must not freeze the row after the flap (the execution is live in tenure 2)"
        );
        assert_eq!(row.1, 10, "coverage not regressed");
        assert_eq!(row.2, None, "no stale terminal status stamped");
        assert!(
            row.3.ends_with(".partial.log.zst"),
            "row must keep pointing at the live .partial: {}",
            row.3
        );
        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        assert!(deletes.lock().unwrap().is_empty(), "no .partial delete");
        assert!(
            deferred.is_empty(),
            "orphaned request dropped, not retained"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "the live execution's entry must not be drained by the stale request"
        );
        assert!(
            buffers
                .read_since(drv_path, 0)
                .is_some_and(|l| l.len() == 2),
            "the live execution's retained lines survive"
        );
        assert!(
            buffers.final_pending(drv_path),
            "mark left for the live tenure's own final"
        );
        Ok(())
    }

    /// bug_009 (r16): a tenure-dropped final whose entry is still SEALED and
    /// EMPTY is the unowned-orphan shape — the terminal already persisted
    /// under the old tenure, so no reaper remains (recovery restamps only
    /// Assigned|Running, the acquisition sweep skips sealed entries, terminal
    /// cleanup skips marked ones), and left in place the entry would just sit
    /// in memory until process restart (reads are unaffected: for a zero-line
    /// entry `GetDerivationLogs` probes the prior leader's stored `.partial`
    /// directly). The tenure-drop arm must reap it: still-sealed proves no
    /// restamp adopted the entry (restamps clear seals), empty proves there
    /// is nothing to lose.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn stale_tenure_drop_reaps_sealed_empty_orphan_entry() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r16b9a-sealed-empty-orphan.drv";
        let drv_hash = "r16b9a";

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();

        // Tenure 1: recovery restamped the exec onto an empty entry (the
        // worker never re-streamed), the drv reached terminal — epilogue
        // shape: seal + enqueue (generation 1) + mark — and the terminal
        // status persisted. The prior leader's periodic flush left the
        // execution's only stored content at the `.partial` key.
        let state = crate::lease::LeaderState::default();
        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("succeeded".into()),
            lease_generation: state.generation(),
        };
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            10,
            1_000,
            false,
            None,
        )
        .await?;

        // The lease flaps before the flusher reaches the request; the same
        // replica re-acquires (generation 2) and serves reads. The drv is
        // terminal in PG, so recovery does NOT restamp this entry.
        state.on_lose();
        state.on_acquire(1);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state,
        );
        let ret = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher.flush_final(req).await
        };
        assert!(ret.is_none(), "orphaned request resolves as a drop");

        // Dropped and counted as a tenure drop...
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .any(|(n, _, v)| n == "rio_scheduler_log_flush_stale_tenure_total" && *v >= 1),
            "tenure drop must be counted: {counters:?}"
        );
        // ...with nothing written or deleted...
        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        assert!(
            deletes.lock().unwrap().is_empty(),
            "the stored .partial must not be deleted"
        );
        let row: (bool, i64, Option<String>) = sqlx::query_as(
            "SELECT is_complete, line_count, status FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(!row.0, "stored row left incomplete (no stale finalize)");
        assert_eq!(row.1, 10, "stored coverage untouched");
        assert_eq!(row.2, None, "no stale terminal status stamped");
        // ...and the sealed, empty, marked orphan is GONE — the bookkeeping
        // reap; reads were already served from the stored `.partial` either
        // way.
        assert_eq!(
            buffers.exec_id(drv_path),
            None,
            "sealed+empty orphan entry must be reaped by the tenure drop"
        );
        assert!(
            buffers.read_since(drv_path, 0).is_none(),
            "reaped orphan leaves no ring state behind"
        );
        assert!(
            !buffers.is_sealed(drv_path),
            "seal tombstone cleared with the reaped entry"
        );
        Ok(())
    }

    /// merged_bug_008 (r16): an out-of-tenure final must be dropped BEFORE
    /// the finalize guard's destructive arms run. Here the entry is the LIVE
    /// execution's carrier — recovery restamped the SAME exec_id at
    /// re-acquisition (which unsealed it), the worker just hasn't
    /// re-streamed yet — and the guard SELECT would fail (PG unreadable
    /// again). The Err arm's empty-entry reap is exec-guarded but not
    /// tenure-guarded, so without the pin running first it removes the live
    /// carrier: every later push lands on no_assignment and the execution's
    /// log is permanently lost while S3 is healthy.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn stale_tenure_drop_before_guard_leaves_live_carrier_writable() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r16b8a-live-carrier-guard.drv";

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();

        // Tenure 1: a cancel-class terminal hits the drv before its worker
        // delivered a single batch — epilogue seals the (empty) entry,
        // enqueues the final (generation 1), marks it; the CancelSignal is
        // dropped, so the worker keeps building.
        let state = crate::lease::LeaderState::default();
        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        // The lease flaps (the terminal persist failed in the same outage);
        // the same replica re-acquires and recovery restamps the SAME exec —
        // the entry is now the live execution's carrier, unsealed and still
        // empty until the reconnected worker re-streams.
        state.on_lose();
        state.on_acquire(1);
        buffers.set_exec(drv_path, exec_id, "test-worker");
        assert!(
            !buffers.is_sealed(drv_path),
            "premise: the same-exec restamp unsealed the live carrier"
        );

        // PG is unreadable again when the flusher dequeues the tenure-1
        // request: the guard SELECT would fail, but the request must be
        // dropped by the tenure pin BEFORE any destructive arm runs.
        let bad_pool = db.reopen().await;
        bad_pool.close().await;
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            bad_pool,
            Arc::clone(&buffers),
            30,
            state,
        );
        let ret = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher.flush_final(req).await
        };
        assert!(ret.is_none(), "orphaned request is dropped, not retained");
        assert!(puts.lock().unwrap().is_empty(), "nothing uploaded");

        // The live carrier survives — unsealed, still stamped — and the
        // reconnected worker's stream keeps landing.
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "the live execution's carrier must not be reaped by an out-of-tenure request"
        );
        assert!(
            buffers
                .read_since(drv_path, 0)
                .is_some_and(|l| l.is_empty()),
            "carrier still present (empty until the worker re-streams)"
        );
        assert!(
            !buffers.is_sealed(drv_path),
            "carrier stays unsealed for the live execution"
        );
        assert!(
            buffers.push_for(
                drv_path,
                &mk_batch(drv_path, 0, &[b"post-flap l0"]),
                "test-worker"
            ),
            "the live execution's batches must still be accepted after the drop"
        );

        // And the drop happened at the tenure pin, before the guard ever ran.
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .any(|(n, _, v)| n == "rio_scheduler_log_flush_stale_tenure_total" && *v >= 1),
            "dropped as a tenure drop, before the guard: {counters:?}"
        );
        Ok(())
    }

    /// bug_003 (r17), drop-then-restamp ordering: when the tenure-drop reap
    /// wins the race against recovery's same-exec restamp, the system
    /// self-heals — `set_exec` recreates a fresh stamped, unsealed carrier
    /// (entry().or_default()) and the reconnected worker's batches land in
    /// it. Complements `stale_tenure_drop_before_guard_leaves_live_carrier_
    /// writable`, which pins the restamp-then-drop ordering.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn stale_tenure_drop_then_same_exec_restamp_recreates_live_carrier() -> anyhow::Result<()>
    {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r17tg3-drop-then-restamp.drv";

        // Tenure 1: terminal for an empty entry — sealed, enqueued
        // (generation 1), marked.
        let state = crate::lease::LeaderState::default();
        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        // The lease flaps; the same replica re-acquires (generation 2). The
        // flusher processes the orphaned request BEFORE the actor's recovery
        // restamps the entry: the sealed, empty orphan is reaped.
        state.on_lose();
        state.on_acquire(1);
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state,
        );
        let ret = flusher.flush_final(req).await;
        assert!(ret.is_none(), "orphaned request resolves as a drop");
        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        assert_eq!(
            buffers.exec_id(drv_path),
            None,
            "sealed+empty orphan reaped by the tenure drop"
        );
        assert!(!buffers.is_sealed(drv_path), "seal cleared with the entry");

        // Recovery's same-exec restamp then runs (the drv was still
        // Assigned|Running in PG): it recreates the carrier, and the
        // reconnected worker's batches are accepted again.
        buffers.set_exec(drv_path, exec_id, "test-worker");
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "restamp recreates the live carrier after the reap"
        );
        assert!(
            !buffers.is_sealed(drv_path),
            "recreated carrier is unsealed"
        );
        assert!(
            buffers.push_for(
                drv_path,
                &mk_batch(drv_path, 0, &[b"post-flap l0"]),
                "test-worker"
            ),
            "the live execution's batches land in the recreated carrier"
        );
        Ok(())
    }

    /// merged_bug_005 (r17), success-path variant: the entry-time tenure pin
    /// passes, but the lease moves WHILE `flush_final` is parked on the
    /// finalize-guard SELECT. The post-await re-check must catch it: no
    /// drain, no upload, no row freeze, no `.partial` delete — the live
    /// tenure's row keeps its coverage and the retained entry stays for its
    /// real owner. Deterministic fixture: an ACCESS EXCLUSIVE table lock
    /// holds the guard SELECT open while the generation is bumped through
    /// the shared `LeaderState` handle, then the lock is released.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn tenure_lost_during_guard_select_drops_final_without_freezing_row() -> anyhow::Result<()>
    {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r17tg1-mid-await-success.drv";
        let drv_hash = "r17tg1";

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();

        // Tenure 1: worker streamed two lines, the cancel sealed the entry,
        // the prefix latch is Checked (an earlier same-tenure flush) — the
        // shape that, without the re-check, goes straight from the guard
        // SELECT to drain → PUT → freeze → .partial delete.
        let state = crate::lease::LeaderState::default();
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"pre-flap l0", b"pre-flap l1"]);
        buffers.seal(drv_path);
        buffers.mark_prefix_checked(drv_path, exec_id);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        // The live tenure's row this stale final must not freeze: `.partial`
        // key, is_complete=false, coverage past the retained ring.
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            10,
            1_000,
            false,
            None,
        )
        .await?;

        // Hold the guard SELECT open: an ACCESS EXCLUSIVE lock on drv_logs
        // blocks the point-SELECT until released.
        let mut lock_tx = db.pool.begin().await?;
        sqlx::query("LOCK TABLE drv_logs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock_tx)
            .await?;

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state.clone(),
        );

        // join! polls flush_final first: the synchronous entry pin has
        // already passed under generation 1 by the time the second branch
        // runs, and the table lock guarantees the guard SELECT cannot
        // complete before the second branch releases it — so the SELECT
        // always returns into a stale tenure.
        let ret = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            let (ret, ()) = tokio::join!(flusher.flush_final(req), async {
                state.on_lose();
                state.on_acquire(1);
                lock_tx.rollback().await.expect("release the table lock");
            });
            ret
        };
        assert!(ret.is_none(), "stale-after-await request is not retained");

        // Nothing destructive happened.
        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        assert!(
            deletes.lock().unwrap().is_empty(),
            "the live .partial must not be deleted"
        );
        let row: (bool, i64, Option<String>, String) = sqlx::query_as(
            "SELECT is_complete, line_count, status, s3_key FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(
            !row.0,
            "row must NOT be frozen complete by a request whose tenure ended mid-await"
        );
        assert_eq!(row.1, 10, "live coverage not regressed");
        assert_eq!(row.2, None, "no stale terminal status stamped");
        assert!(
            row.3.ends_with(".partial.log.zst"),
            "row must keep pointing at the live .partial: {}",
            row.3
        );
        // The retained entry is untouched (its lines, seal, and mark stay
        // for whichever owner resolves it).
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "entry not drained by the stale-after-await request"
        );
        assert!(
            buffers
                .read_since(drv_path, 0)
                .is_some_and(|l| l.len() == 2),
            "retained ring lines left in place"
        );
        assert!(buffers.is_sealed(drv_path), "seal left untouched");
        assert!(buffers.final_pending(drv_path), "mark left untouched");
        // Counted as a stale-tenure drop.
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .any(|(n, _, v)| n == "rio_scheduler_log_flush_stale_tenure_total" && *v >= 1),
            "mid-await tenure loss must be counted as a stale-tenure drop: {counters:?}"
        );
        Ok(())
    }

    /// merged_bug_005 (r17), Err-arm variant: the entry-time pin passes, the
    /// guard SELECT is held open by a table lock, the lease flaps and the
    /// same replica re-acquires (recovery's same-exec restamp turns the
    /// entry into the LIVE execution's empty carrier), then the blocked
    /// SELECT's backend is terminated so the guard returns `Err` into a
    /// stale tenure. The Err arm's re-check must drop the request without
    /// touching the entry: no empty-reap of the live carrier, no re-mark,
    /// no retention.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn tenure_lost_during_guard_select_err_arm_leaves_live_carrier_unreaped()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r17tg2-mid-await-err.drv";

        // Tenure 1: a cancel-class terminal hits the drv before its worker
        // delivered a single batch — epilogue seals the (empty) entry,
        // enqueues the final (generation 1), marks it.
        let state = crate::lease::LeaderState::default();
        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        // Hold the guard SELECT open.
        let mut lock_tx = db.pool.begin().await?;
        sqlx::query("LOCK TABLE drv_logs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock_tx)
            .await?;

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state.clone(),
        );

        let pool = db.pool.clone();
        let bufs = Arc::clone(&buffers);
        let state2 = state.clone();
        let drv = drv_path.to_string();
        let (ret, ()) = tokio::join!(flusher.flush_final(req), async move {
            // Wait until the guard SELECT is actually parked on the table
            // lock (visible in pg_stat_activity as a Lock wait) so the
            // terminate below hits the right backend deterministically.
            let blocked_pid: i32 = {
                let mut found = None;
                for _ in 0..200 {
                    let pid: Option<(i32,)> = sqlx::query_as(
                        "SELECT pid FROM pg_stat_activity \
                         WHERE datname = current_database() \
                           AND wait_event_type = 'Lock' \
                           AND query ILIKE '%FROM drv_logs WHERE exec_id%' \
                           AND pid <> pg_backend_pid()",
                    )
                    .fetch_optional(&pool)
                    .await
                    .expect("pg_stat_activity poll");
                    if let Some((pid,)) = pid {
                        found = Some(pid);
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(25)).await;
                }
                found.expect("guard SELECT never showed up as lock-blocked")
            };
            // The lease flaps; the same replica re-acquires and recovery
            // restamps the SAME exec — the entry is now the live execution's
            // carrier (unsealed, unmarked, still empty).
            state2.on_lose();
            state2.on_acquire(1);
            bufs.set_exec(&drv, exec_id, "test-worker");
            // Now fail the parked SELECT so the guard returns Err into the
            // (stale) tenure.
            let _ = sqlx::query("SELECT pg_terminate_backend($1)")
                .bind(blocked_pid)
                .execute(&pool)
                .await
                .expect("terminate the blocked guard SELECT");
            lock_tx.rollback().await.expect("release the table lock");
        });

        assert!(
            ret.is_none(),
            "stale-after-await request must not be retained for retry"
        );
        assert!(puts.lock().unwrap().is_empty(), "nothing uploaded");
        // The live carrier survives — present, unsealed, unmarked — and the
        // reconnected worker's stream keeps landing.
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "the live execution's carrier must not be reaped by the stale Err-arm pass"
        );
        assert!(
            !buffers.is_sealed(drv_path),
            "carrier stays unsealed for the live execution"
        );
        assert!(
            !buffers.final_pending(drv_path),
            "carrier must not be re-marked by the stale Err-arm pass"
        );
        assert!(
            buffers.push_for(
                drv_path,
                &mk_batch(drv_path, 0, &[b"post-flap l0"]),
                "test-worker"
            ),
            "the live execution's batches must still be accepted after the drop"
        );
        Ok(())
    }

    /// merged_bug_007 (r18, supersedes the r17 one-shot-consult shape): a
    /// tenure-dropped final whose entry is still SEALED and NON-empty has no
    /// request-driven reaper left (recovery restamps only Assigned|Running
    /// drvs, the sweep skips sealed keys, cleanup skips marked entries, and
    /// the drop arm itself does no PG work) — so once another tenure
    /// finalized the exec, the orphan would shadow the finalized blob in
    /// GetDerivationLogs (stale lines served as is_complete=false) while the
    /// periodic tick re-PUTs its `.partial` for the process lifetime. The
    /// recurring reaper is the periodic flush itself: its row UPSERT is
    /// refused by the frozen-row latch (the row is already finalized), and
    /// on that refusal the still-sealed, exec-stamped entry is discarded —
    /// the durable record supersedes the retained lines and the re-PUT
    /// churn ends. The drop arm keeps the entry; the reap self-heals within
    /// one periodic tick of PG/leadership recovery.
    /// r[verify obs.log.deferred-final-retry+3]
    /// r[verify obs.log.finalize-immutable]
    #[tokio::test]
    async fn stale_tenure_orphan_reaped_by_periodic_refused_upsert_when_finalized_elsewhere()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r18tg1-finalized-elsewhere.drv";
        let drv_hash = "r18tg1";

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();

        // Tenure 1: the worker streamed two pre-flap lines, a cancel-class
        // terminal sealed the entry and enqueued the final (generation 1),
        // and the terminal status persisted — so after the flap nobody
        // restamps this entry.
        let state = crate::lease::LeaderState::default();
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"pre-flap l0", b"pre-flap l1"]);
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        // The interim leader finalized this execution: drv_logs row frozen
        // complete at the canonical `.log.zst` key.
        let final_key = log_s3_key(drv_path, &exec_id, false);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &final_key,
            0,
            10,
            1_000,
            true,
            Some("succeeded"),
        )
        .await?;

        // The lease flaps before the flusher reaches the request; this
        // replica re-acquires (generation 2) and serves reads.
        state.on_lose();
        state.on_acquire(1);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state,
        );

        // Phase 1 — the orphaned final: dropped, counted, zero PG and S3
        // work, and the sealed non-empty entry deliberately left in place
        // (the drop arm has no row consult; the periodic path below is the
        // orphan's reaper).
        let ret = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher.flush_final(req).await
        };
        assert!(ret.is_none(), "orphaned request resolves as a drop");
        assert!(
            puts.lock().unwrap().is_empty(),
            "the drop arm does no S3 PUT"
        );
        assert!(deletes.lock().unwrap().is_empty(), "no S3 delete");
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "the drop arm keeps the sealed non-empty orphan (no PG consult)"
        );
        assert!(buffers.is_sealed(drv_path), "seal survives the drop");
        assert!(buffers.final_pending(drv_path), "mark survives the drop");
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .any(|(n, _, v)| n == "rio_scheduler_log_flush_stale_tenure_total" && *v >= 1),
            "tenure drop must be counted: {counters:?}"
        );

        // Phase 2 — the next periodic tick: the snapshot PUTs a `.partial`,
        // the row UPSERT is refused (frozen by the interim leader's
        // finalization), and the refusal reaps the sealed orphan so reads
        // fall through to the authoritative finalized blob instead of stale
        // is_complete=false ring lines until process restart.
        flusher.flush_periodic().await;
        assert_eq!(
            buffers.exec_id(drv_path),
            None,
            "refused periodic UPSERT must reap the sealed orphan stamped with the finalized exec"
        );
        assert!(
            buffers.read_since(drv_path, 0).is_none(),
            "ring must no longer shadow the finalized blob"
        );
        assert!(
            !buffers.is_sealed(drv_path),
            "seal tombstone cleared with the reaped entry"
        );
        // The finalized row is untouched (frozen-row latch).
        let row: (bool, i64, Option<String>, String) = sqlx::query_as(
            "SELECT is_complete, line_count, status, s3_key FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(row.0, "finalized row stays complete");
        assert_eq!(row.1, 10, "finalized coverage untouched");
        assert_eq!(row.2.as_deref(), Some("succeeded"), "status untouched");
        assert_eq!(row.3, final_key, "s3_key untouched");
        // The tick that observed the refusal still PUT one `.partial`
        // snapshot (the TTL GC sweeps it at expiry); nothing was deleted.
        let put_keys: Vec<String> = puts
            .lock()
            .unwrap()
            .iter()
            .map(|(k, _)| k.clone())
            .collect();
        assert_eq!(
            put_keys.len(),
            1,
            "exactly the one snapshot PUT that ran into the refusal: {put_keys:?}"
        );
        assert!(put_keys[0].ends_with(".partial.log.zst"));
        assert!(deletes.lock().unwrap().is_empty(), "no S3 delete");

        // Phase 3 — churn over: with the entry reaped the next tick has
        // nothing left to snapshot for this drv.
        flusher.flush_periodic().await;
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "no further re-PUT churn once the orphan is reaped"
        );
        Ok(())
    }

    /// bug_004 (r17) / r18: the residual the reaps must NOT widen into — same
    /// sealed non-empty orphan shape, but no tenure ever finalized the exec.
    /// The ring's lines are the best data available, so the drop arm (which
    /// performs no PG work) keeps the entry, only the request is dropped, and
    /// the periodic snapshotter keeps it durable at `.partial` coverage
    /// (served as is_complete=false) instead of reaping it.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn stale_tenure_drop_keeps_sealed_nonempty_entry_when_not_finalized() -> anyhow::Result<()>
    {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r17tg5-never-finalized.drv";
        let drv_hash = "r17tg5";

        let state = crate::lease::LeaderState::default();
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"pre-flap l0", b"pre-flap l1"]);
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        // Only a periodic `.partial` row exists — no tenure finalized it.
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            10,
            1_000,
            false,
            None,
        )
        .await?;

        state.on_lose();
        state.on_acquire(1);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state,
        );
        let ret = flusher.flush_final(req).await;
        assert!(ret.is_none(), "orphaned request resolves as a drop");

        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        assert!(deletes.lock().unwrap().is_empty(), "no S3 delete");
        let row: (bool, i64, Option<String>) = sqlx::query_as(
            "SELECT is_complete, line_count, status FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(!row.0, "row left incomplete (no stale finalize)");
        assert_eq!(row.1, 10, "stored coverage untouched");
        assert_eq!(row.2, None, "no stale terminal status stamped");

        // The entry survives with its lines, seal, and mark — the bounded
        // residual for execs never finalized by any tenure.
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "never-finalized orphan entry must be left in place"
        );
        assert!(
            buffers
                .read_since(drv_path, 0)
                .is_some_and(|l| l.len() == 2),
            "retained lines stay readable"
        );
        assert!(buffers.is_sealed(drv_path), "seal left untouched");
        assert!(buffers.final_pending(drv_path), "mark left untouched");
        Ok(())
    }

    /// bug_009 (r18): the out-of-tenure drop arm performs ZERO PG work even
    /// for the sealed non-empty shape — during the very outage that orphans
    /// such requests, a row consult would serially burn a pool-acquire
    /// timeout per retained final and stall the flusher select loop
    /// (periodic snapshots, GC, new finals) for N×timeout. Structural pin:
    /// with `drv_logs` locked ACCESS EXCLUSIVE (any read would block), the
    /// drop must still resolve immediately — consumed, counted, entry kept.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn stale_tenure_drop_does_no_pg_work_for_sealed_nonempty_entry() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r18tg2-no-pg-work.drv";

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();

        // Sealed, non-empty, exec-stamped, final-pending — the exact shape
        // that previously (r17) triggered a drop-arm row consult.
        let state = crate::lease::LeaderState::default();
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"pre-flap l0", b"pre-flap l1"]);
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };
        state.on_lose();
        state.on_acquire(1);

        // Wedge PG for reads: any drv_logs SELECT would park on this lock,
        // so a drop arm that still consulted the row could not return until
        // the lock is released (the timeout below would fire instead).
        let mut lock_tx = db.pool.begin().await?;
        sqlx::query("LOCK TABLE drv_logs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock_tx)
            .await?;

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state,
        );
        let ret = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            tokio::time::timeout(Duration::from_secs(10), flusher.flush_final(req))
                .await
                .expect("the out-of-tenure drop arm must not block on PG")
        };
        lock_tx.rollback().await?;

        assert!(ret.is_none(), "orphaned request resolves as a drop");
        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        assert!(deletes.lock().unwrap().is_empty(), "no S3 delete");
        // The sealed non-empty orphan is left in place for the periodic
        // refused-UPSERT reap (or, if never finalized elsewhere, for the
        // periodic snapshotter to keep durable at `.partial` coverage).
        assert_eq!(buffers.exec_id(drv_path), Some(exec_id), "entry kept");
        assert!(
            buffers
                .read_since(drv_path, 0)
                .is_some_and(|l| l.len() == 2),
            "retained lines stay readable"
        );
        assert!(buffers.is_sealed(drv_path), "seal left untouched");
        assert!(buffers.final_pending(drv_path), "mark left untouched");
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .any(|(n, _, v)| n == "rio_scheduler_log_flush_stale_tenure_total" && *v >= 1),
            "tenure drop must be counted: {counters:?}"
        );
        Ok(())
    }

    /// merged_bug_007 part b (r18): a request that goes stale DURING the
    /// finalize-guard SELECT in the already-finalized arm has the
    /// `is_complete=true` row literally in hand — the mid-await drop must
    /// use it and reap the sealed NON-empty residue (require_empty=false),
    /// not just the empty shape, so the orphan does not shadow the
    /// finalized blob until restart. Same LOCK TABLE fixture as the
    /// not-finalized mid-await test; the row here is frozen complete.
    /// r[verify obs.log.deferred-final-retry+3]
    /// r[verify obs.log.finalize-immutable]
    #[tokio::test]
    async fn tenure_lost_during_finalized_guard_select_reaps_sealed_nonempty_residue()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r18tg3-mid-await-finalized.drv";
        let drv_hash = "r18tg3";

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();

        // Tenure 1: two retained pre-flap lines, sealed, marked — and the
        // execution is ALREADY finalized in drv_logs (an interim leader
        // completed it; this replica's request is pre-failover residue).
        let state = crate::lease::LeaderState::default();
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"pre-flap l0", b"pre-flap l1"]);
        buffers.seal(drv_path);
        buffers.mark_prefix_checked(drv_path, exec_id);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        let final_key = log_s3_key(drv_path, &exec_id, false);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &final_key,
            0,
            10,
            1_000,
            true,
            Some("succeeded"),
        )
        .await?;

        // Hold the guard SELECT open: the lease moves while it is in
        // flight, so it returns the finalized row into a stale tenure.
        let mut lock_tx = db.pool.begin().await?;
        sqlx::query("LOCK TABLE drv_logs IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *lock_tx)
            .await?;

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state.clone(),
        );

        // join! polls flush_final first: the synchronous entry pin passes
        // under generation 1, the guard SELECT parks on the table lock, and
        // the second branch then flips the tenure and releases the lock.
        let ret = {
            let _guard = metrics::set_default_local_recorder(&recorder);
            let (ret, ()) = tokio::join!(flusher.flush_final(req), async {
                state.on_lose();
                state.on_acquire(1);
                lock_tx.rollback().await.expect("release the table lock");
            });
            ret
        };
        assert!(ret.is_none(), "stale-after-await request is not retained");

        // Nothing uploaded or deleted; the finalized row untouched.
        assert!(puts.lock().unwrap().is_empty(), "no S3 PUT");
        assert!(deletes.lock().unwrap().is_empty(), "no S3 delete");
        let row: (bool, i64, Option<String>, String) = sqlx::query_as(
            "SELECT is_complete, line_count, status, s3_key FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(row.0, "finalized row stays complete");
        assert_eq!(row.1, 10, "finalized coverage untouched");
        assert_eq!(row.2.as_deref(), Some("succeeded"), "status untouched");
        assert_eq!(row.3, final_key, "s3_key untouched");

        // The in-hand finalized row proves the retained lines are
        // superseded: the sealed non-empty residue is reaped so reads fall
        // through to the durable blob instead of stale ring lines.
        assert_eq!(
            buffers.exec_id(drv_path),
            None,
            "sealed non-empty residue must be reaped using the in-hand finalized row"
        );
        assert!(
            buffers.read_since(drv_path, 0).is_none(),
            "ring must no longer shadow the finalized blob"
        );
        assert!(
            !buffers.is_sealed(drv_path),
            "seal tombstone cleared with the reaped entry"
        );
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .any(|(n, _, v)| n == "rio_scheduler_log_flush_stale_tenure_total" && *v >= 1),
            "mid-await tenure loss must be counted as a stale-tenure drop: {counters:?}"
        );
        Ok(())
    }

    /// Negative space of the periodic refused-UPSERT reap (r18): an UNSEALED
    /// entry is the live execution's carrier — a refused snapshot UPSERT
    /// (row already finalized, e.g. the accepted post-drain residual where a
    /// stale final froze the row while the same exec keeps streaming here)
    /// must NOT reap it: the worker is still pushing into it and reads are
    /// served from it. Only the seal (no restamp in the current tenure
    /// adopted the entry) marks an entry as reapable orphan residue.
    #[tokio::test]
    async fn periodic_refused_upsert_leaves_unsealed_live_carrier_untouched() -> anyhow::Result<()>
    {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r18tg4-live-carrier-refused.drv";
        let drv_hash = "r18tg4";

        // Live carrier: stamped, unsealed, streaming.
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"live l0", b"live l1"]);

        // The execution's row is already frozen complete (another tenure's
        // final landed; the frozen-row latch will refuse this replica's
        // periodic snapshot UPSERT).
        let final_key = log_s3_key(drv_path, &exec_id, false);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &final_key,
            0,
            10,
            1_000,
            true,
            Some("succeeded"),
        )
        .await?;

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        flusher.flush_periodic().await;

        // The snapshot reached the UPSERT (one `.partial` PUT) and was
        // refused — but the live carrier is untouched and still writable.
        assert_eq!(puts.lock().unwrap().len(), 1, "snapshot PUT happened");
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "unsealed live carrier must not be reaped on a refused UPSERT"
        );
        assert!(
            buffers
                .read_since(drv_path, 0)
                .is_some_and(|l| l.len() == 2),
            "live lines stay served from the ring"
        );
        assert!(!buffers.is_sealed(drv_path), "carrier stays unsealed");
        assert!(
            buffers.push_for(
                drv_path,
                &mk_batch(drv_path, 2, &[b"live l2"]),
                "test-worker"
            ),
            "the live execution's batches must still be accepted"
        );
        let row: (bool, i64) =
            sqlx::query_as("SELECT is_complete, line_count FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert!(
            row.0,
            "finalized row stays complete (latch refused the snapshot)"
        );
        assert_eq!(row.1, 10, "finalized coverage untouched");
        Ok(())
    }

    /// bug_003 (r19): a sealed non-empty orphan kept by the tenure-drop arm
    /// is documented as being reaped by the periodic flush's refused-UPSERT
    /// chokepoint — but when the re-acquired tenure's first tick finds a
    /// stored, non-finalized row whose coverage extends past the retained
    /// ring's tail (an interim leader kept flushing), the stored-coverage
    /// reconcile truncates the whole ring away, and an empty snapshot
    /// early-returns out of the periodic path before any PUT/UPSERT — the
    /// refused-UPSERT reap is structurally unreachable and no other reaper
    /// exists. The periodic tick must reap the sealed, now-empty entry on
    /// that same tick — otherwise it would just sit in memory until process
    /// restart (reads are unaffected either way: a zero-line entry's reads
    /// are served from the stored `.partial`).
    /// r[verify obs.log.deferred-final-retry+3]
    /// r[verify obs.log.stored-coverage-preserved]
    #[tokio::test]
    async fn stale_tenure_orphan_emptied_by_stored_coverage_reconcile_reaped_on_periodic_tick()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r19tg1-emptied-by-reconcile.drv";
        let drv_hash = "r19tg1";

        // The interim leader B's stored `.partial`: A's lines 0-2 plus the
        // lines only B received (3-5) — coverage [0..6), past A's retained
        // ring tail (0..=2).
        let stored: Vec<&[u8]> = vec![b"a-0", b"a-1", b"a-2", b"b-3", b"b-4", b"b-5"];
        let stored_zst = compress_lines(&stored.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, deletes, gets) = mock_s3_capturing_with_get(Ok(stored_zst));
        let buffers = Arc::new(LogBuffers::new());

        // Tenure A (generation 1): the worker streamed lines 0-2, a
        // cancel-class terminal sealed the entry and enqueued the final, and
        // the terminal status persisted — so after the flap nobody restamps
        // this entry.
        let state = crate::lease::LeaderState::default();
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"a-0", b"a-1", b"a-2"]);
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));
        let req = FlushRequest {
            drv_path: drv_path.into(),
            exec_id,
            status: Some("cancelled".into()),
            lease_generation: state.generation(),
        };

        // The interim leader B durably extended the stored row past A's
        // retained ring tail and never finalized it (B lost the lease before
        // its own final landed).
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            6,
            24,
            false,
            None,
        )
        .await?;

        // The lease flaps before the flusher reaches A's queued final; this
        // replica re-acquires (generation 2).
        state.on_lose();
        state.on_acquire(1);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state,
        );

        // Phase 1 — the orphaned final: dropped by the tenure pin; the
        // sealed non-empty entry is deliberately left in place (the drop arm
        // does no PG work).
        let ret = flusher.flush_final(req).await;
        assert!(ret.is_none(), "orphaned request resolves as a drop");
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "fixture: the drop arm keeps the sealed non-empty orphan"
        );
        assert!(
            buffers.is_sealed(drv_path),
            "fixture: seal survives the drop"
        );

        // Phase 2 — the first periodic tick: the stored-coverage reconcile
        // finds the row ending past the ring tail, fetches the stored blob,
        // and truncates the whole ring away; the snapshot is then empty, so
        // no PUT/UPSERT runs and the refused-UPSERT reap can never fire. The
        // tick itself must reap the sealed, now-empty entry.
        flusher.flush_periodic().await;
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "fixture: the reconcile fetched the stored blob (coverage not subsumed)"
        );
        assert!(
            puts.lock().unwrap().is_empty(),
            "an emptied ring has nothing to upload — no PUT this tick"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            None,
            "the sealed entry emptied by the reconcile must be reaped on the same periodic tick"
        );
        assert!(
            buffers.read_since(drv_path, 0).is_none(),
            "reaped entry leaves no ring state behind"
        );
        assert!(
            !buffers.is_sealed(drv_path),
            "seal tombstone cleared with the reaped entry"
        );

        // The stored row — the orphan's only durable coverage — is untouched.
        let row: (bool, i64, Option<String>) = sqlx::query_as(
            "SELECT is_complete, line_count, status FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(!row.0, "row left incomplete (no stale finalize)");
        assert_eq!(row.1, 6, "interim leader's stored coverage untouched");
        assert_eq!(row.2, None, "no stale terminal status stamped");
        assert!(deletes.lock().unwrap().is_empty(), "no S3 delete");

        // Phase 3 — steady state: nothing left to snapshot for this drv.
        flusher.flush_periodic().await;
        assert!(
            puts.lock().unwrap().is_empty(),
            "no re-PUT churn after the reap"
        );
        Ok(())
    }

    /// Negative space of the sealed-empty periodic reap (r19): an UNSEALED
    /// empty entry is a just-dispatched live carrier (`set_exec` ran, the
    /// worker has not streamed a line yet — overlay setup, FUSE warm). The
    /// periodic tick's empty-snapshot early-return must leave it alone:
    /// only the seal (terminal observed, no restamp adopted the entry)
    /// marks an empty entry as reapable orphan residue.
    #[tokio::test]
    async fn periodic_tick_leaves_unsealed_empty_live_carrier_untouched() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r19tg2-fresh-carrier.drv";

        // Just dispatched: stamped, unsealed, no lines yet.
        let exec_id = stamp_and_push(&buffers, drv_path, &[]);

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        flusher.flush_periodic().await;

        assert!(
            puts.lock().unwrap().is_empty(),
            "nothing to upload for an empty carrier"
        );
        assert_eq!(
            buffers.exec_id(drv_path),
            Some(exec_id),
            "an unsealed empty carrier must not be reaped by the periodic tick"
        );
        assert!(
            buffers.push_for(
                drv_path,
                &mk_batch(drv_path, 0, &[b"first line"]),
                "test-worker"
            ),
            "the live execution's first batch must still land in the carrier"
        );
        Ok(())
    }

    /// The benign corner of the sealed-empty periodic reap (r19): a sealed
    /// empty entry whose own IN-tenure final is still pending (failover
    /// restamp whose worker never re-streamed, or a silent build / cancel
    /// before any output), when a periodic sweep runs between the
    /// epilogue's seal+enqueue and the final's dequeue. The reap fires; the
    /// final then takes the no-entry arm: request consumed, no panic,
    /// nothing uploaded, and NO row write at all — the empty drain's
    /// `status`/`finished_at` stamp is lost. That is the same residual the
    /// tenure-drop arm's empty reap already accepts, now reachable without
    /// any tenure or PG failure; the prior `.partial` row and blob stay
    /// untouched and keep being served as is_complete=false.
    #[tokio::test]
    async fn periodic_sealed_empty_reap_then_in_tenure_final_takes_no_entry_arm_without_row_write()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r19tg3-benign-corner.drv";

        // Ex-leader history: the worker streamed a line and the periodic
        // flusher stored the `.partial` blob + row (status/finished_at NULL).
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"streamed to the ex-leader"]);
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        flusher.flush_periodic().await;
        assert_eq!(puts.lock().unwrap().len(), 1, "fixture: periodic PUT");

        // Failover restamp onto this replica: empty entry, same exec
        // (modeled as discard + set_exec, like
        // `flush_final_empty_drain_stamps_status_but_stays_incomplete`). The
        // drv then terminates before the worker reconnects: the epilogue
        // seals, marks the final pending, and enqueues it.
        buffers.discard(drv_path);
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.seal(drv_path);
        assert!(buffers.mark_final_pending(drv_path, exec_id));

        // A periodic sweep wins the race against the queued final: the
        // sealed empty entry is reaped on this tick.
        flusher.flush_periodic().await;
        assert_eq!(
            buffers.exec_id(drv_path),
            None,
            "sealed empty entry reaped by the periodic tick"
        );

        // The in-tenure final then resolves through the no-entry arm:
        // consumed (not retained) — and, the accepted residual, no row write
        // at all: status/finished_at stay NULL.
        let ret = flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("failed".into()),
                lease_generation: 1,
            })
            .await;
        assert!(ret.is_none(), "request consumed, not retained");

        let row: (bool, Option<String>, Option<f64>, String, i64) = sqlx::query_as(
            "SELECT is_complete, status, EXTRACT(EPOCH FROM finished_at)::float8, s3_key, \
                    line_count \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(!row.0, "row stays is_complete=false");
        assert_eq!(
            row.1, None,
            "the no-entry arm performs no row write: the empty drain's status stamp is lost \
             (accepted residual)"
        );
        assert!(row.2.is_none(), "finished_at stays NULL");
        assert!(
            row.3.ends_with(".partial.log.zst"),
            "row keeps describing the .partial blob: {}",
            row.3
        );
        assert_eq!(row.4, 1, "stored coverage untouched");

        // Nothing further uploaded or deleted; the `.partial` blob (the only
        // stored content) survives and the entry stays gone.
        assert_eq!(puts.lock().unwrap().len(), 1, "no further PUT");
        assert!(
            deletes.lock().unwrap().is_empty(),
            "the .partial blob is not deleted"
        );
        assert_eq!(buffers.exec_id(drv_path), None, "entry stays gone");
        assert!(
            buffers.read_since(drv_path, 0).is_none(),
            "reads keep falling through to the stored .partial"
        );
        Ok(())
    }

    /// The positive gate: a deferral retried under the SAME unbroken tenure
    /// still uploads and finalizes through `retry_deferred` itself (the
    /// round-14 win this fix must not regress). Green before and after the
    /// tenure pin — guards against an over-eager gate.
    /// r[verify obs.log.deferred-final-retry+3]
    #[tokio::test]
    async fn retry_deferred_same_tenure_attempts_and_uploads() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r15tg4-same-tenure-retry.drv";
        let drv_hash = "r15tg4";

        let exec_id = stamp_and_push(&buffers, drv_path, &[b"l0", b"l1"]);
        buffers.seal(drv_path);

        // Same flusher, same tenure (leader, generation 1) for the deferral
        // and the retry — the continuous-leader history the retention exists
        // for.
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let mut deferred: Vec<FlushRequest> = Vec::new();
        flusher.retain_deferred(
            &mut deferred,
            FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            },
        );
        assert!(buffers.mark_final_pending(drv_path, exec_id));

        flusher.retry_deferred(&mut deferred).await;

        assert!(deferred.is_empty(), "retry resolved the deferral");
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "exactly the final PUT");
        let (key, body) = &captured[0];
        assert_eq!(
            key,
            &format!("logs/{drv_hash}/{exec_id}.log.zst"),
            "same-tenure retry lands at the final key"
        );
        assert_eq!(
            String::from_utf8(zstd::decode_all(&body[..])?)?,
            "l0\nl1\n",
            "no content lost across the deferral"
        );
        let row: (bool, Option<String>, i64) = sqlx::query_as(
            "SELECT is_complete, status, line_count FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert!(row.0, "row finalized by the same-tenure retry");
        assert_eq!(row.1.as_deref(), Some("succeeded"));
        assert_eq!(row.2, 2);
        assert_eq!(
            buffers.exec_id(drv_path),
            None,
            "retry drained the entry (its reaper)"
        );
        assert!(!buffers.is_sealed(drv_path), "retry unsealed");
        Ok(())
    }

    /// Defense-in-depth: even if a final-flush write reaches the UPSERT for
    /// an already-finalized row (a concurrent finalization landing after the
    /// guard's lookup read the row as unfinalized, or a future caller
    /// bypasses it), the conflict clause refuses ANY update to a finalized
    /// row — not just the `is_complete` true→false downgrade.
    /// r[verify obs.log.finalize-immutable]
    #[tokio::test]
    async fn upsert_refuses_second_finalization_of_completed_row() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec_id = Uuid::now_v7();

        // The interim leader's genuine final.
        upsert_drv_log(
            &db.pool,
            exec_id,
            "h",
            "logs/h/final.log.zst",
            0,
            5000,
            99999,
            true,
            Some("succeeded"),
        )
        .await?;
        // What an unguarded A→B→A flap would issue: a second "final" for
        // the same exec with stale pre-failover content.
        upsert_drv_log(
            &db.pool,
            exec_id,
            "h",
            "logs/h/final.log.zst",
            0,
            50,
            1000,
            true,
            Some("cancelled"),
        )
        .await?;

        let row: (i64, Option<String>, bool) = sqlx::query_as(
            "SELECT line_count, status, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, 5000, "true→true rewrite must be refused");
        assert_eq!(row.1.as_deref(), Some("succeeded"));
        assert!(row.2);
        Ok(())
    }

    /// Defense-in-depth for the zero-line variant: an empty stale drain
    /// must not restamp `status`/`finished_at` on a row another leader
    /// finalized.
    /// r[verify obs.log.finalize-immutable]
    #[tokio::test]
    async fn finalize_empty_drain_leaves_finalized_row_untouched() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, _puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let exec_id = Uuid::now_v7();

        // A finalized row (the interim leader's final flush).
        upsert_drv_log(
            &db.pool,
            exec_id,
            "h",
            "logs/h/final.log.zst",
            0,
            7,
            70,
            true,
            Some("succeeded"),
        )
        .await?;

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        flusher
            .finalize_empty_drain(&FlushRequest {
                drv_path: "/nix/store/dddd-emptydrain-finalized.drv".into(),
                exec_id,
                status: Some("cancelled".into()),
                lease_generation: 1,
            })
            .await;

        let row: (Option<String>, bool) =
            sqlx::query_as("SELECT status, is_complete FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(
            row.0.as_deref(),
            Some("succeeded"),
            "an empty stale drain must not restamp status on a finalized row"
        );
        assert!(row.1);
        Ok(())
    }

    /// Reporting contract for a PG failure on the empty-drain finalization
    /// stamp: warn (not error), no false "is lost" claim, counted on the
    /// dedicated empty-drain counter — never on the alert-keyed
    /// flush-failures counter (nothing was drained; the .partial blob and
    /// its row are intact and still served). Direct call: through
    /// `flush_final` a whole-pool outage trips the already-finalized
    /// guard's deferral arm first and never reaches the stamp.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn finalize_empty_drain_pg_failure_warns_without_loss_alert() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r14b002a-emptydrain-pgfail.drv";

        // Ex-leader half of the failover-restamp shape: one streamed line,
        // one periodic snapshot → .partial blob + incomplete row.
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"streamed to the ex-leader"]);
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        flusher.flush_periodic().await;
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "fixture: periodic PUT happened"
        );

        // PG outage at the moment of the stamp.
        db.pool.close().await;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher
                .finalize_empty_drain(&FlushRequest {
                    drv_path: drv_path.into(),
                    exec_id,
                    status: Some("failed".into()),
                    lease_generation: 1,
                })
                .await;
        }

        // Reporting: warn-level, accurate message, no false loss claim.
        assert!(!logs_contain("is lost"));
        assert!(logs_contain("status/finished_at stamp failed"));
        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .find(|l| l.contains("status/finished_at stamp failed"))
            {
                Some(line) if line.contains("WARN") && !line.contains("ERROR") => Ok(()),
                Some(line) => Err(format!("expected WARN-level line, got: {line}")),
                None => Err("missing stamp-failure line".to_string()),
            }
        });

        // Routing: dedicated counter moved; the alert-keyed counter did not.
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .all(|(n, _, _)| n != "rio_scheduler_log_flush_failures_total"),
            "empty-drain stamp failure must not hit the loss-alert counter: {counters:?}"
        );
        let stamp: Vec<_> = counters
            .iter()
            .filter(|(n, _, _)| n == "rio_scheduler_log_empty_drain_finalize_failures_total")
            .collect();
        assert_eq!(
            stamp.len(),
            1,
            "exactly one stamp-failure series: {counters:?}"
        );
        assert_eq!(stamp[0].2, 1);

        // Nothing lost or touched: no new S3 traffic, and the row still
        // describes the periodic snapshot, unstamped and incomplete.
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "no new PUT from the failure path"
        );
        assert!(deletes.lock().unwrap().is_empty(), "no .partial delete");
        let pool2 = db.reopen().await;
        let row: (bool, Option<String>, String, i64, Option<f64>) = sqlx::query_as(
            "SELECT is_complete, status, s3_key, line_count, \
                    EXTRACT(EPOCH FROM finished_at)::float8 \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&pool2)
        .await?;
        assert!(!row.0, "row stays is_complete=false");
        assert_eq!(row.1, None, "status not stamped (the UPDATE failed)");
        assert!(
            row.2.ends_with(".partial.log.zst"),
            "s3_key still points at the .partial blob: {}",
            row.2
        );
        assert_eq!(row.3, 1, "periodic snapshot's line_count preserved");
        assert!(row.4.is_none(), "finished_at not stamped");
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
                lease_generation: 1,
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
                lease_generation: 1,
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

    /// The shutdown arm (channel-closed `None =>`) must be leader-gated like
    /// the periodic-tick and gc-tick arms: an ex-leader's retained
    /// `LogBuffers` entries are stamped with `exec_id`s the new leader
    /// re-stamps from `assignments` and may have already finalized — an
    /// un-gated last-gasp sweep would PUT stale `.partial` blobs and churn
    /// the new leader's partial rows for nothing (the `upsert_drv_log`
    /// latch refuses the `is_complete` downgrade on finalized rows, but
    /// the wasted writes are reason enough to gate).
    ///
    /// Anti-vacuousness: the snapshot assert below proves the buffer is
    /// stamped AND non-empty at the moment the arm fires, and the sibling
    /// leader test proves this exact fixture produces a PUT through this
    /// exact arm when the gate passes.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn shutdown_sweep_skipped_when_not_leader() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/notleader-shutdown-gate.drv";
        let _exec_id = stamp_and_push(&buffers, drv_path, &[b"pre-flap line"]);

        // Structural non-vacuousness: a stamped, NON-EMPTY entry is what the
        // shutdown arm is about to see. An empty stamped entry would also
        // produce "no PUT" (the line_count==0 early-return in
        // upload_and_record) and make this test pass without the gate.
        let (_, _, line_count, _, _) = buffers
            .snapshot(drv_path)
            .expect("fixture entry must be stamped");
        assert_eq!(line_count, 1, "fixture must have a line to flush");

        let (tx, rx) = mpsc::channel::<FlushRequest>(8);
        LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            // Ex-leader: on_lose() flipped the gate, but the stamped
            // buffers from before the flap are retained.
            crate::lease::LeaderState::pending(Arc::new(std::sync::atomic::AtomicU64::new(1))),
        )
        .spawn(rx);

        // Close the channel → recv() yields None → the shutdown arm runs →
        // the loop breaks and logs "log flusher exited".
        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !logs_contain("log flusher exited") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("flusher loop did not exit after channel close");

        // The entry survived (the gate skips, it does not drain). NOT the
        // regression catch — that is the two asserts below.
        assert_eq!(buffers.active_count(), 1);
        // The gate — not an empty buffer — is why nothing was written.
        assert!(
            puts.lock().unwrap().is_empty(),
            "ex-leader shutdown sweep must not PUT to S3"
        );
        let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(
            count.0, 0,
            "ex-leader shutdown sweep must not write drv_logs"
        );
        Ok(())
    }

    /// Sibling of `shutdown_sweep_skipped_when_not_leader`: a replica that
    /// still holds the lease at shutdown MUST get its last-gasp sweep —
    /// graceful release calls `step_down()` without `on_lose()`, so
    /// `is_leader` stays true and the ≤30s log-loss bound survives a clean
    /// rolling restart. Also proves the negative test's fixture is capable
    /// of producing a PUT through the same code path.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn shutdown_sweep_runs_when_leader() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/stillleader-shutdown-gate.drv";
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"about to be saved"]);

        let (tx, rx) = mpsc::channel::<FlushRequest>(8);
        LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        )
        .spawn(rx);

        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !logs_contain("log flusher exited") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("flusher loop did not exit after channel close");

        // The last-gasp sweep ran: one `.partial` PUT + one
        // is_complete=false row keyed on the live entry's exec_id.
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1, "leader shutdown sweep PUTs the snapshot");
        assert_eq!(
            captured[0].0,
            format!("logs/stillleader/{exec_id}.partial.log.zst")
        );
        let row: (bool,) = sqlx::query_as("SELECT is_complete FROM drv_logs WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
        assert!(!row.0, "shutdown sweep is a periodic snapshot, not a final");
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

    /// bug_015: S3 `DeleteObjects` returns 200 with the keys it could
    /// NOT delete listed in `output.errors()` (KMS denied, Object Lock,
    /// transient backend). `quiet(true)` means the response body carries
    /// ONLY those failures — but the sweep used to bind only the `Err`
    /// (request-level) arm, so a per-key failure produced an orphan blob
    /// with no warn at all. Pin that the per-key arm now warns and that
    /// the sweep still completes its PG work.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn gc_sweep_warns_on_per_key_delete_errors() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        // 200 OK with one per-key AccessDenied — the request itself
        // succeeds. RuleMode::MatchAny: the sweep only calls
        // delete_objects, so one rule suffices.
        let del_rule = mock!(S3Client::delete_objects).then_output(|| {
            DeleteObjectsOutput::builder()
                .errors(
                    aws_sdk_s3::types::Error::builder()
                        .key("logs/feedfacefeedfacefeedfacefeedface/x.log.zst")
                        .code("AccessDenied")
                        .message("Access Denied")
                        .build(),
                )
                .build()
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&del_rule]);

        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            client,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        // One expired row (60d > 30d retention). Return value unused —
        // the assertion below is on the table being empty, not on a
        // specific exec_id surviving.
        seed_drv_log_aged(&db.pool, 60, true).await?;

        flusher.sweep_expired_logs().await;

        // The per-key failure is surfaced — this is the whole bug. The
        // needle is the arm-specific fragment, not the shared
        // "lifecycle rule is the backstop" suffix.
        assert!(
            logs_contain("per-key failures"),
            "Ok(output) with non-empty errors() must warn"
        );
        // And it did NOT route through the request-level arm.
        assert!(!logs_contain("DeleteObjects failed"));
        // The pass still did its PG work — a per-key S3 failure must not
        // abort the sweep (parity with the request-level Err arm).
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(count, 0, "expired row swept despite per-key S3 failure");
        Ok(())
    }

    /// Sibling of `gc_sweep_warns_on_per_key_delete_errors`: the
    /// request-level failure shape (transport/auth — the whole
    /// `DeleteObjects` call returns `Err`). This arm pre-dates bug_015
    /// but was untested; the fix relocates it from `if let Err` to a
    /// `match` arm, so pin that it still warns and still does not abort
    /// the pass.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn gc_sweep_warns_on_request_level_delete_error() -> anyhow::Result<()> {
        use aws_sdk_s3::error::ErrorMetadata;
        use aws_sdk_s3::operation::delete_objects::DeleteObjectsError;
        let db = TestDb::new(&crate::MIGRATOR).await;
        let del_rule = mock!(S3Client::delete_objects).then_error(|| {
            DeleteObjectsError::generic(
                ErrorMetadata::builder()
                    .code("InternalError")
                    .message("simulated S3 500")
                    .build(),
            )
        });
        let client = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&del_rule]);

        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            client,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        seed_drv_log_aged(&db.pool, 60, true).await?;

        flusher.sweep_expired_logs().await;

        // Routed through the request-level arm, not the per-key arm.
        assert!(
            logs_contain("DeleteObjects failed"),
            "request-level Err must warn"
        );
        assert!(!logs_contain("per-key failures"));
        // The pass still did its PG work.
        let (count,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM drv_logs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(
            count, 0,
            "expired row swept despite request-level S3 failure"
        );
        Ok(())
    }

    // ── recovered pre-failover prefix (bug_003, bughunter r11) ─────────────
    //
    // After a leader failover with a reconnecting worker, recovery restamps
    // the SAME exec_id onto an empty buffer and the worker only re-streams
    // the post-failover suffix. The flusher must fetch the ex-leader's
    // stored `.partial` once and prepend it to every flush of that
    // execution instead of overwriting (and finally deleting) the only copy
    // of the pre-failover prefix.

    /// GET-call counter shared with the mock's match closure.
    type GetCalls = Arc<std::sync::atomic::AtomicUsize>;

    /// Like [`mock_s3_capturing`] but with a GetObject rule. `get_body`:
    /// `Ok(bytes)` → 200 with that body; `Err(())` → generic 5xx (the
    /// supported aws-smithy-mocks way to simulate a failed GET). Also
    /// returns a counter of GetObject calls so tests can pin the
    /// fetch-once-then-cache behavior.
    fn mock_s3_capturing_with_get(
        get_body: Result<Vec<u8>, ()>,
    ) -> (S3Client, CapturedPuts, CapturedDeletes, GetCalls) {
        use aws_sdk_s3::error::ErrorMetadata;
        use aws_sdk_s3::operation::get_object::{GetObjectError, GetObjectOutput};

        let puts: CapturedPuts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deletes: CapturedDeletes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gets: GetCalls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pcap = Arc::clone(&puts);
        let dcap = Arc::clone(&deletes);
        let gcap = Arc::clone(&gets);

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
        let count_get = move || gcap.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let get_rule = match get_body {
            Ok(bytes) => mock!(S3Client::get_object)
                .match_requests(move |_req| {
                    count_get();
                    true
                })
                .then_output(move || {
                    GetObjectOutput::builder()
                        .body(ByteStream::from(bytes.clone()))
                        .build()
                }),
            Err(()) => mock!(S3Client::get_object)
                .match_requests(move |_req| {
                    count_get();
                    true
                })
                .then_error(|| {
                    GetObjectError::generic(
                        ErrorMetadata::builder()
                            .code("InternalError")
                            .message("simulated S3 GET failure")
                            .build(),
                    )
                }),
        };
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&put_rule, &del_rule, &get_rule]
        );
        (client, puts, deletes, gets)
    }

    /// Failover-fixture front half shared by the recovered-prefix tests:
    /// leader A stamps + streams `prefix_lines` at line 0 and its periodic
    /// flush stores the `.partial` blob + `drv_logs` row; then the lease
    /// moves and the fresh standby's recovery restamps an EMPTY entry with
    /// the SAME exec_id (modeled as discard + set_exec, the same modeling
    /// as `flush_final_empty_drain_stamps_status_but_stays_incomplete`). The caller owns
    /// pushing the post-reconnect suffix and the flush under test.
    async fn failover_restamp_after_periodic(
        flusher: &LogFlusher,
        buffers: &LogBuffers,
        drv_path: &str,
        prefix_lines: &[&[u8]],
    ) -> Uuid {
        let exec_id = stamp_and_push(buffers, drv_path, prefix_lines);
        flusher.flush_periodic().await;
        buffers.discard(drv_path);
        buffers.set_exec(drv_path, exec_id, "test-worker");
        exec_id
    }

    /// Like [`mock_s3_capturing_with_get`] but GetObject returns the body of
    /// the most recently captured PUT — chained failover tests need the next
    /// leader's prefix fetch to read what the previous merge actually stored,
    /// not a fixed fixture body.
    fn mock_s3_capturing_serving_last_put() -> (S3Client, CapturedPuts, CapturedDeletes, GetCalls) {
        use aws_sdk_s3::operation::get_object::GetObjectOutput;

        let puts: CapturedPuts = Arc::new(std::sync::Mutex::new(Vec::new()));
        let deletes: CapturedDeletes = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gets: GetCalls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let pcap = Arc::clone(&puts);
        let dcap = Arc::clone(&deletes);
        let gcap = Arc::clone(&gets);
        let pserve = Arc::clone(&puts);

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
        let get_rule = mock!(S3Client::get_object)
            .match_requests(move |_req| {
                gcap.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                true
            })
            .then_output(move || {
                let body = pserve
                    .lock()
                    .unwrap()
                    .last()
                    .map(|(_, b)| b.clone())
                    .unwrap_or_default();
                GetObjectOutput::builder()
                    .body(ByteStream::from(body))
                    .build()
            });
        let client = mock_client!(
            aws_sdk_s3,
            RuleMode::MatchAny,
            &[&put_rule, &del_rule, &get_rule]
        );
        (client, puts, deletes, gets)
    }

    /// Second failover for the same execution: the row the first merge wrote
    /// is itself the next leader's recovered prefix. Its line_count covers
    /// the TRUE span (0..7, gap counted), so a re-stream that abuts the true
    /// end (line 7) merges with NO second marker. Pre-fix the first merge
    /// recorded the physical count (6); this leg then computed gap = 7−6 = 1
    /// and folded a spurious "[rio: ~1 earlier lines lost…]" marker per flap.
    // r[verify obs.log.gap-span]
    #[tokio::test]
    async fn second_failover_merge_uses_true_span_of_stored_row() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes, gets) = mock_s3_capturing_serving_last_put();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r12gap1-second-failover.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // Leader A streams 0–2 and periodic-flushes; flap #1; leader B's
        // recovery restamps an empty entry with the same exec.
        let prefix: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2"];
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &prefix).await;

        // Worker reconnects to B at line 5 (3–4 lost): merge #1.
        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));
        flusher.flush_periodic().await;
        let row: (i64, i64) =
            sqlx::query_as("SELECT first_line, line_count FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(
            (row.0, row.1),
            (0, 7),
            "merge #1 records the true span 0..7, not the 6 physical lines"
        );

        // Flap #2: the next leader restamps an empty entry again; the worker
        // re-streams from line 7 — abutting the true end, nothing lost.
        buffers.discard(drv_path);
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.push(&mk_batch(drv_path, 7, &[b"sfx-7"]));
        flusher.flush_periodic().await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (_key, body) = captured.last().expect("merge #2 PUT");
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(
            decoded,
            "pfx-0\npfx-1\npfx-2\n[rio: ~2 earlier lines lost across scheduler failover]\nsfx-5\nsfx-6\nsfx-7\n",
            "abutting re-stream after a second failover must not add a spurious marker"
        );
        assert_eq!(
            decoded.matches("[rio:").count(),
            1,
            "exactly the original marker"
        );
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!((row.0, row.1), (0, 8), "true span grows 7 -> 8");
        assert!(!row.2, "still a periodic snapshot");
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "one prefix fetch per post-failover merge"
        );
        Ok(())
    }

    /// Periodic flush after a failover restamp + worker reconnect must
    /// fetch the ex-leader's stored `.partial` once and PUT a merged blob
    /// (prefix + gap marker + suffix) instead of overwriting the prefix
    /// with the suffix-only snapshot.
    // r[verify obs.log.periodic-flush]
    #[tokio::test]
    async fn failover_reconnect_periodic_merges_stored_prefix() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let prefix: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2"];
        let prefix_zst = compress_lines(&prefix.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Ok(prefix_zst));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx1-merge-periodic.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &prefix).await;

        // A periodic tick fires before the worker reconnects: the snapshot
        // is zero-line and must neither PUT nor poison the later merge
        // (the prefix state stays Unchecked — the lookup needs the suffix's
        // first line to evaluate the disjoint-range condition).
        flusher.flush_periodic().await;
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "zero-line tick before reconnect must not PUT"
        );
        assert!(
            matches!(
                buffers.prefix_state(drv_path, exec_id),
                PrefixState::Unchecked
            ),
            "zero-line tick must leave the prefix state Unchecked"
        );

        // Worker reconnects and re-streams only the undelivered suffix.
        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));
        flusher.flush_periodic().await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("merged periodic PUT");
        assert!(key.ends_with(".partial.log.zst"), "still a snapshot: {key}");
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(
            decoded,
            "pfx-0\npfx-1\npfx-2\n[rio: ~2 earlier lines lost across scheduler failover]\nsfx-5\nsfx-6\n",
            "snapshot must carry prefix + gap marker + suffix"
        );
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, 0, "row covers the merged range from line 0");
        assert_eq!(row.1, 7, "true span 0..7: 3 prefix + 2-line gap + 2 suffix");
        assert!(!row.2, "still a periodic snapshot");
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "stored prefix fetched exactly once"
        );
        Ok(())
    }

    /// Final flush for a failover-restamped execution whose first non-empty
    /// flush IS the final (no periodic ran since the reconnect) must also
    /// merge — and only then is deleting the `.partial` safe.
    // r[verify obs.log.periodic-flush]
    #[tokio::test]
    async fn failover_reconnect_final_merges_and_supersedes_partial() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let prefix: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2"];
        let prefix_zst = compress_lines(&prefix.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, deletes, gets) = mock_s3_capturing_with_get(Ok(prefix_zst));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx2-merge-final.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &prefix).await;
        let drv_hash = "r11pfx2";

        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("final PUT");
        assert_eq!(key, &format!("logs/{drv_hash}/{exec_id}.log.zst"));
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(
            decoded,
            "pfx-0\npfx-1\npfx-2\n[rio: ~2 earlier lines lost across scheduler failover]\nsfx-5\nsfx-6\n",
            "final blob must carry prefix + gap marker + suffix"
        );
        let row: (i64, i64, bool, Option<String>) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete, status FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, 0);
        assert_eq!(row.1, 7, "true span 0..7: 3 prefix + 2-line gap + 2 suffix");
        assert!(row.2, "final flush finalizes the row");
        assert_eq!(row.3.as_deref(), Some("succeeded"));
        // The merged final supersedes the .partial — deleting it is safe now.
        assert_eq!(
            deletes.lock().unwrap().clone(),
            vec![format!("logs/{drv_hash}/{exec_id}.partial.log.zst")],
            ".partial deleted only because the final blob now covers the prefix"
        );
        assert_eq!(buffers.active_count(), 0, "final flush drains");
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 1);
        Ok(())
    }

    /// The prefix is fetched ONCE and cached on the ring-buffer entry: a
    /// merged periodic followed by more streaming and the final must keep
    /// the prefix without a second GET. (Without the cache the second
    /// flush's range test would go false against the merged row and the
    /// suffix-only snapshot would clobber the merge — rejected alt §3.6.)
    // r[verify obs.log.periodic-flush]
    #[tokio::test]
    async fn failover_reconnect_periodic_then_final_keeps_prefix() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let prefix: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2"];
        let prefix_zst = compress_lines(&prefix.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Ok(prefix_zst));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx3-merge-then-final.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &prefix).await;

        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));
        flusher.flush_periodic().await; // merged .partial
        buffers.push(&mk_batch(drv_path, 7, &[b"sfx-7", b"sfx-8"]));
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("final PUT");
        assert!(key.ends_with(".log.zst") && !key.ends_with(".partial.log.zst"));
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert!(
            decoded.starts_with("pfx-0\n"),
            "final body must still start with the recovered prefix: {decoded:?}"
        );
        for line in ["sfx-5", "sfx-6", "sfx-7", "sfx-8"] {
            assert!(decoded.contains(line), "missing {line} in {decoded:?}");
        }
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(row.0, 0);
        assert_eq!(row.1, 9, "true span 0..9: 3 prefix + 2-line gap + 4 suffix");
        assert!(row.2);
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "prefix fetched once, then served from the entry cache"
        );
        Ok(())
    }

    /// A stored row whose range overlaps the re-streamed ring content: the
    /// stored copy wins within its range — the superseded ring head is
    /// dropped, the stored blob is folded in, and the re-streamed
    /// duplicates are not uploaded over it. (Pre-fix this shape was
    /// "accepted head loss": the snapshot overwrote the stored head.)
    // r[verify obs.log.stored-coverage-preserved]
    #[tokio::test]
    async fn overlapping_stored_range_supersedes_ring_head() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let stored: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2", b"pfx-3", b"pfx-4"];
        let stored_zst = compress_lines(&stored.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Ok(stored_zst));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx4-overlap.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        // Stored row covers lines 0-4.
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &stored).await;

        // Re-streamed content starts at line 2 — entirely within the stored
        // range (stored_end = 5 > 2). The reconcile fetches the stored blob,
        // truncates the superseded ring head to empty, and the tick is
        // skipped: nothing new to add, the stored `.partial` stays intact.
        buffers.push(&mk_batch(drv_path, 2, &[b"ovl-2", b"ovl-3", b"ovl-4"]));
        flusher.flush_periodic().await;

        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "no new PUT: the re-streamed duplicates are superseded by the stored copy"
        );
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            (row.0, row.1, row.2),
            (0, 5, false),
            "row keeps describing the stored content"
        );
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            matches!(
                buffers.prefix_state(drv_path, exec_id),
                PrefixState::Cached(_)
            ),
            "stored content cached for the next flush"
        );

        // Once genuinely new lines arrive (past the stored end), the merge
        // carries stored content + new tail with no marker (ranges abut).
        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));
        flusher.flush_periodic().await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (_key, body) = captured.last().expect("merged snapshot PUT");
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(
            decoded, "pfx-0\npfx-1\npfx-2\npfx-3\npfx-4\nsfx-5\nsfx-6\n",
            "stored head preserved; only the genuinely new tail is appended"
        );
        assert!(!decoded.contains("[rio:"), "no gap marker — ranges abut");
        let row: (i64, i64) =
            sqlx::query_as("SELECT first_line, line_count FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!((row.0, row.1), (0, 7), "5 stored + 2 new, no marker slot");
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "stored blob fetched once, then served from the entry cache"
        );
        Ok(())
    }

    /// Failover restamp where the prior leader never flushed anything:
    /// no `drv_logs` row exists, so the suffix-only flush is exactly
    /// today's behavior, the lookup happens once (no GET — the SELECT
    /// finds nothing), and the entry is marked checked.
    #[tokio::test]
    async fn reconnect_without_stored_row_flushes_suffix_only() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Ok(vec![]));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx5-norow.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        // Recovery restamp on a fresh standby; leader A never flushed.
        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");
        // Reconnected worker re-streams from line 5.
        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));

        flusher.flush_periodic().await;
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("suffix-only PUT");
        assert!(key.ends_with(".partial.log.zst"));
        assert_eq!(
            String::from_utf8(zstd::decode_all(&body[..])?)?,
            "sfx-5\nsfx-6\n"
        );
        let row: (i64, i64) =
            sqlx::query_as("SELECT first_line, line_count FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!((row.0, row.1), (5, 2));
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 0, "no GET");
        assert!(
            matches!(
                buffers.prefix_state(drv_path, exec_id),
                PrefixState::Checked
            ),
            "entry marked checked after the no-row lookup"
        );

        // Second periodic: still no GET (and no re-SELECT churn observable
        // here — the Checked state short-circuits the whole lookup).
        flusher.flush_periodic().await;
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 0);
        Ok(())
    }

    /// Periodic flush must not overwrite stored content it failed to
    /// re-read: on prefix-fetch failure the tick skips this drv entirely
    /// (no PUT, no UPSERT) and retries next tick.
    #[tokio::test]
    async fn prefix_fetch_failure_periodic_skips_flush() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let prefix: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2"];
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Err(()));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx6-getfail-periodic.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &prefix).await;
        assert_eq!(puts.lock().unwrap().len(), 1, "fixture: prefix PUT");

        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));
        // Recorder around the flush under test only, so the fixture's
        // periodic flush doesn't pollute the capture.
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher.flush_periodic().await;
        }

        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "fetch failure must skip the PUT (never overwrite unread content)"
        );
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            (row.0, row.1, row.2),
            (0, 3, false),
            "row keeps describing the stored prefix"
        );
        assert!(gets.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        // Reporting routing: a pre-drain lookup failure lands on the
        // dedicated prefix-fetch counter, never on the alert-keyed
        // flush-failures counter (nothing was lost; next tick retries).
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .all(|(n, _, _)| n != "rio_scheduler_log_flush_failures_total"),
            "pre-drain lookup failure must not hit the loss-alert counter: {counters:?}"
        );
        let pfx: Vec<_> = counters
            .iter()
            .filter(|(n, _, _)| n == "rio_scheduler_log_prefix_fetch_failures_total")
            .collect();
        assert_eq!(pfx.len(), 1, "exactly one prefix-fetch failure series");
        assert_eq!(pfx[0].2, 1);
        assert!(pfx[0].1.contains(&("phase".to_string(), "s3".to_string())));
        assert!(
            pfx[0]
                .1
                .contains(&("is_final".to_string(), "false".to_string()))
        );
        Ok(())
    }

    /// Final flush after a prefix-fetch failure uploads the drained suffix
    /// (refusing would lose it too) but preserves the `.partial` — it is
    /// the prefix's only copy.
    // r[verify obs.log.stored-coverage-preserved]
    #[tokio::test]
    async fn prefix_fetch_failure_final_uploads_suffix_but_keeps_partial() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let prefix: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2"];
        let (s3, puts, deletes, _gets) = mock_s3_capturing_with_get(Err(()));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx7-getfail-final.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &prefix).await;

        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("failed".into()),
                lease_generation: 1,
            })
            .await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("final PUT");
        assert!(key.ends_with(".log.zst") && !key.ends_with(".partial.log.zst"));
        assert_eq!(
            String::from_utf8(zstd::decode_all(&body[..])?)?,
            "sfx-5\nsfx-6\n",
            "suffix-only upload (the prefix could not be re-read)"
        );
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!((row.0, row.1, row.2), (5, 2, true));
        assert!(
            deletes.lock().unwrap().is_empty(),
            "the .partial (only copy of the prefix) must NOT be deleted"
        );
        Ok(())
    }

    /// Reporting contract for a pre-drain stored-coverage lookup failure on
    /// the FINAL path: warn (not error), no false "is lost" claim, counted
    /// on the dedicated prefix-fetch counter — never on the alert-keyed
    /// flush-failures counter (the flush proceeds and nothing is drained or
    /// lost by the lookup step). The data path for the identical trace
    /// (suffix-only upload, `.partial` preserved) is pinned by
    /// `prefix_fetch_failure_final_uploads_suffix_but_keeps_partial`.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn prefix_fetch_failure_final_warns_without_loss_alert() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let prefix: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2"];
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Err(()));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r13pfx1-getfail-final-obs.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &prefix).await;
        buffers.push(&mk_batch(drv_path, 5, &[b"sfx-5", b"sfx-6"]));

        // Recorder around the flush under test only, so the fixture's
        // periodic flush doesn't pollute the capture.
        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher
                .flush_final(FlushRequest {
                    drv_path: drv_path.into(),
                    exec_id,
                    status: Some("failed".into()),
                    lease_generation: 1,
                })
                .await;
        }

        // Vacuity guards: the lookup ran and hit the failing GET, and the
        // flush still proceeded to the final upload (data path pinned by
        // the sibling test — not re-asserted here).
        assert!(gets.load(std::sync::atomic::Ordering::SeqCst) >= 1);
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, _body) = captured.last().expect("final PUT");
        assert!(key.ends_with(".log.zst") && !key.ends_with(".partial.log.zst"));

        // Reporting: warn-level, accurate message, no false loss claim.
        assert!(!logs_contain("is lost (buffer already drained)"));
        assert!(logs_contain("stored-coverage lookup failed"));
        logs_assert(|lines: &[&str]| {
            match lines
                .iter()
                .find(|l| l.contains("stored-coverage lookup failed"))
            {
                Some(line) if line.contains("WARN") && !line.contains("ERROR") => Ok(()),
                Some(line) => Err(format!("expected WARN-level line, got: {line}")),
                None => Err("missing 'stored-coverage lookup failed' line".to_string()),
            }
        });

        // Routing: dedicated counter with the real labels; the alert-keyed
        // flush-failures counter did not move.
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .all(|(n, _, _)| n != "rio_scheduler_log_flush_failures_total"),
            "pre-drain lookup failure must not hit the loss-alert counter: {counters:?}"
        );
        let pfx: Vec<_> = counters
            .iter()
            .filter(|(n, _, _)| n == "rio_scheduler_log_prefix_fetch_failures_total")
            .collect();
        assert_eq!(pfx.len(), 1, "exactly one prefix-fetch failure series");
        assert_eq!(pfx[0].2, 1);
        assert!(pfx[0].1.contains(&("phase".to_string(), "s3".to_string())));
        assert!(
            pfx[0]
                .1
                .contains(&("is_final".to_string(), "true".to_string()))
        );
        Ok(())
    }

    /// PG outage during the pre-drain reconcile: the periodic snapshot is
    /// skipped (nothing drained, nothing overwritten), reported at warn on
    /// the dedicated prefix-fetch counter with phase="pg" — never on the
    /// alert-keyed flush-failures counter.
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn prefix_fetch_pg_failure_periodic_warns_on_dedicated_counter() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r13pfx2-pgfail-periodic.drv";
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        // Stamped entry, non-empty ring, PrefixState::Unchecked.
        let _exec_id = stamp_and_push(&buffers, drv_path, &[b"line-0"]);
        // Outage: closes all clones of the pool, including the flusher's.
        db.pool.close().await;

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher.flush_periodic().await;
        }

        // Snapshot skipped, buffer intact (retried next tick).
        assert!(puts.lock().unwrap().is_empty(), "no PUT on fetch failure");
        assert_eq!(buffers.active_count(), 1, "buffer intact for next tick");
        // Reporting: warn-level new message, no false loss claim.
        assert!(logs_contain("stored-coverage lookup failed"));
        assert!(!logs_contain("is lost"));
        // Routing: dedicated counter with phase="pg", alert counter untouched.
        let counters = all_counters(&snap);
        assert!(
            counters
                .iter()
                .all(|(n, _, _)| n != "rio_scheduler_log_flush_failures_total"),
            "pre-drain lookup failure must not hit the loss-alert counter: {counters:?}"
        );
        let pfx: Vec<_> = counters
            .iter()
            .filter(|(n, _, _)| n == "rio_scheduler_log_prefix_fetch_failures_total")
            .collect();
        assert_eq!(pfx.len(), 1, "exactly one prefix-fetch failure series");
        assert_eq!(pfx[0].2, 1);
        assert!(pfx[0].1.contains(&("phase".to_string(), "pg".to_string())));
        assert!(
            pfx[0]
                .1
                .contains(&("is_final".to_string(), "false".to_string()))
        );
        Ok(())
    }

    /// Ranges that abut exactly (gap == 0) get no synthetic marker line.
    #[tokio::test]
    async fn reconnect_with_no_gap_omits_marker() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let prefix: Vec<&[u8]> = vec![b"pfx-0", b"pfx-1", b"pfx-2"];
        let prefix_zst = compress_lines(&prefix.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Ok(prefix_zst));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx8-nogap.drv";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        let exec_id = failover_restamp_after_periodic(&flusher, &buffers, drv_path, &prefix).await;

        // Suffix starts exactly at prefix_end (line 3): nothing was lost.
        buffers.push(&mk_batch(drv_path, 3, &[b"sfx-3", b"sfx-4"]));
        flusher.flush_periodic().await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (_key, body) = captured.last().expect("merged PUT");
        assert_eq!(
            String::from_utf8(zstd::decode_all(&body[..])?)?,
            "pfx-0\npfx-1\npfx-2\nsfx-3\nsfx-4\n",
            "abutting ranges merge without a marker"
        );
        let row: (i64, i64) =
            sqlx::query_as("SELECT first_line, line_count FROM drv_logs WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(
            (row.0, row.1),
            (0, 5),
            "3 + 2, no marker slot (abutting: span == physical)"
        );
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 1);
        Ok(())
    }

    /// Same-leader ring eviction (no failover): the entry was marked
    /// checked by the first periodic's no-row reconcile, so the final
    /// flush takes today's path — no lookup, no merge, `.partial` deleted.
    /// Also pins that the steady state does not regress to per-tick
    /// SELECT/GETs and the same-tenure-eviction carve-out of
    /// `obs.log.stored-coverage-preserved` (the row in this shape was
    /// produced by this tenure from this very ring after its reconcile).
    #[tokio::test]
    async fn checked_entry_final_flush_skips_lookup() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (s3, puts, deletes, gets) = mock_s3_capturing_with_get(Ok(vec![]));
        let buffers = Arc::new(LogBuffers::new());
        let drv_path = "/nix/store/r11pfx9-evicted.drv";
        let drv_hash = "r11pfx9";

        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );
        // Same-leader: streamed from line 0, periodic #1 stores lines 0-2
        // and marks the entry checked via the no-row reconcile (one
        // point-SELECT, no GET).
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"pfx-0", b"pfx-1", b"pfx-2"]);
        flusher.flush_periodic().await;
        assert!(
            matches!(
                buffers.prefix_state(drv_path, exec_id),
                PrefixState::Checked
            ),
            "the first snapshot's no-row reconcile marks the entry checked"
        );

        // The build keeps spewing: line-count eviction drops the head past
        // line 2 (mirrors `ring_eviction_drops_oldest`).
        let big: Vec<Vec<u8>> = (0..=crate::logs::RING_CAPACITY)
            .map(|_| b"x".to_vec())
            .collect();
        let refs: Vec<&[u8]> = big.iter().map(|v| v.as_slice()).collect();
        buffers.push(&mk_batch(drv_path, 3, &refs));

        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;

        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "Checked entry must not trigger the final-flush fallback lookup"
        );
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("final PUT");
        assert!(key.ends_with(".log.zst") && !key.ends_with(".partial.log.zst"));
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert!(
            !decoded.contains("pfx") && !decoded.contains("[rio:"),
            "no merge of the leader's own stale .partial, no marker"
        );
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        // Eviction math: 3 small lines + (RING_CAPACITY + 1) pushed lines
        // exceed RING_CAPACITY by 4, so the head 4 lines (the 3 "pfx" +
        // the first big line at 3) are evicted: first_line = 4,
        // line_count = RING_CAPACITY.
        assert_eq!(
            (row.0, row.1, row.2),
            (4, i64::try_from(crate::logs::RING_CAPACITY)?, true),
            "row takes the drained snapshot's values"
        );
        assert_eq!(
            deletes.lock().unwrap().clone(),
            vec![format!("logs/{drv_hash}/{exec_id}.partial.log.zst")],
            "today's .partial delete is unchanged for the same-leader path"
        );
        Ok(())
    }

    /// A→B→A lease flap with a retained ring: the re-acquired ex-leader's
    /// ring still starts at line 0, but the interim leader durably extended
    /// the stored `drv_logs` row past what this ring holds (the worker only
    /// re-streams undelivered batches, so the retained ring has an interior
    /// hole where the interim-leader-only lines should be). The next
    /// periodic flush must fold the stored coverage in instead of
    /// overwriting it with the holed snapshot.
    // r[verify obs.log.stored-coverage-preserved]
    #[tokio::test]
    async fn aba_flap_retained_ring_folds_interim_leader_coverage_periodic() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r12aba1-flap-periodic.drv";
        let drv_hash = "r12aba1";
        // The interim leader B's stored `.partial` content: A's lines 0-2
        // plus the lines only B ever received (3-5). No internal gap marker
        // — the ranges in this fixture all abut, keeping the line arithmetic
        // skew-free.
        let stored: Vec<&[u8]> = vec![b"a-0", b"a-1", b"a-2", b"b-3", b"b-4", b"b-5"];
        let stored_zst = compress_lines(&stored.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Ok(stored_zst));
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // (1) A leads: the worker streams lines 0-2 and A's periodic flush
        // stores them. No stored row exists yet, so the entry is checked
        // without an S3 GET.
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"a-0", b"a-1", b"a-2"]);
        flusher.flush_periodic().await;
        assert!(
            matches!(
                buffers.prefix_state(drv_path, exec_id),
                PrefixState::Checked
            ),
            "first periodic finds no stored row and marks the entry checked"
        );
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(puts.lock().unwrap().len(), 1, "fixture: A's periodic PUT");

        // (2) A loses the lease; the interim leader B receives lines 3-5
        // and its periodic flush extends the stored row to cover [0..6).
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            6,
            24,
            false,
            None,
        )
        .await?;

        // (3) A re-acquires: recovery restamps the SAME exec onto the
        // retained entry (lines 0-2 still buffered). The prefix bookkeeping
        // latched during A's first tenure must be re-armed.
        buffers.set_exec(drv_path, exec_id, "test-worker");
        assert!(
            matches!(
                buffers.prefix_state(drv_path, exec_id),
                PrefixState::Unchecked
            ),
            "a same-exec recovery restamp must re-arm the stored-coverage check"
        );

        // (4) The worker reconnects to A and re-streams only undelivered
        // lines (6-7): the retained ring is now {0,1,2,6,7} — an interior
        // hole exactly where the interim-leader-only lines belong.
        buffers.push(&mk_batch(drv_path, 6, &[b"c-6", b"c-7"]));

        // (5) A's next periodic flush must fold B's stored coverage in
        // instead of overwriting B's `.partial` with the holed snapshot.
        flusher.flush_periodic().await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("merged periodic PUT");
        assert!(key.ends_with(".partial.log.zst"), "still a snapshot: {key}");
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(
            decoded, "a-0\na-1\na-2\nb-3\nb-4\nb-5\nc-6\nc-7\n",
            "the lines only the interim leader had must survive the re-acquired leader's flush"
        );
        assert!(!decoded.contains("[rio:"), "ranges abut: no gap marker");
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!((row.0, row.1, row.2), (0, 8, false));
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "stored blob fetched exactly once"
        );
        assert!(matches!(
            buffers.prefix_state(drv_path, exec_id),
            PrefixState::Cached(_)
        ));
        Ok(())
    }

    /// Same A→B→A shape as the periodic variant, but the first flush after
    /// re-acquisition is the FINAL: the pre-drain reconcile must fold B's
    /// stored coverage into the final blob — only then is finalizing the
    /// row and deleting the `.partial` safe.
    // r[verify obs.log.stored-coverage-preserved]
    #[tokio::test]
    async fn aba_flap_retained_ring_folds_interim_leader_coverage_final() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r12aba2-flap-final.drv";
        let drv_hash = "r12aba2";
        let stored: Vec<&[u8]> = vec![b"a-0", b"a-1", b"a-2", b"b-3", b"b-4", b"b-5"];
        let stored_zst = compress_lines(&stored.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, deletes, gets) = mock_s3_capturing_with_get(Ok(stored_zst));
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // Same fixture as the periodic variant through the worker's
        // post-re-acquisition re-stream (steps 1-4 there).
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"a-0", b"a-1", b"a-2"]);
        flusher.flush_periodic().await;
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            6,
            24,
            false,
            None,
        )
        .await?;
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.push(&mk_batch(drv_path, 6, &[b"c-6", b"c-7"]));

        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("final PUT");
        assert_eq!(key, &format!("logs/{drv_hash}/{exec_id}.log.zst"));
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(
            decoded, "a-0\na-1\na-2\nb-3\nb-4\nb-5\nc-6\nc-7\n",
            "the final blob must carry the lines only the interim leader had"
        );
        let row: (i64, i64, bool, Option<String>) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete, status FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!((row.0, row.1), (0, 8));
        assert!(row.2, "final flush finalizes the row");
        assert_eq!(row.3.as_deref(), Some("succeeded"));
        assert_eq!(
            deletes.lock().unwrap().clone(),
            vec![format!("logs/{drv_hash}/{exec_id}.partial.log.zst")],
            ".partial deleted only because the final blob now covers the stored content"
        );
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "stored blob fetched exactly once"
        );
        assert_eq!(buffers.active_count(), 0, "final flush drains");
        Ok(())
    }

    /// A→B→A flap, but the periodic tick fires in the gap BETWEEN the lease
    /// loop's synchronous `is_leader=true` store and the actor dequeuing
    /// `LeaderAcquired` (so neither the prefix re-arm nor the same-exec
    /// restamp has run and the entry still carries tenure-1's Checked
    /// latch). The tick must be deferred by the recovery gate; once the
    /// actor-side re-arm + recovery_complete happen, the next self-driven
    /// flush must fold the interim leader's stored coverage instead of
    /// having already shrunk it. Models the spawned loop (the gate lives
    /// there), unlike the sibling aba_flap_* tests which drive
    /// flush_periodic() directly after a restamp.
    // r[verify obs.log.stored-coverage-preserved]
    #[tokio::test]
    #[tracing_test::traced_test]
    async fn aba_flap_gap_tick_deferred_until_recovery_then_folds_interim_coverage()
    -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r14gap1-flap-gap-tick.drv";
        let drv_hash = "r14gap1";
        // Interim leader B's stored `.partial`: A's lines 0-2 plus the lines
        // only B received (3-5). Same arithmetic as the aba_flap_* siblings.
        let stored: Vec<&[u8]> = vec![b"a-0", b"a-1", b"a-2", b"b-3", b"b-4", b"b-5"];
        let stored_zst = compress_lines(&stored.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, _deletes, gets) = mock_s3_capturing_with_get(Ok(stored_zst));
        let buffers = Arc::new(LogBuffers::new());

        // Production-faithful gate: tenure 1 (leader, recovered) → on_lose →
        // on_acquire stores is_leader=true ONLY (recovery_complete stays
        // false until the "actor" runs recovery in phase 5).
        let state = crate::lease::LeaderState::default();
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            state.clone(),
        );

        // (1) Tenure A1: worker streams 0-2, A's periodic flush latches the
        // entry Checked and stores the row (line_count=3).
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"a-0", b"a-1", b"a-2"]);
        flusher.flush_periodic().await;
        assert_eq!(puts.lock().unwrap().len(), 1, "fixture: A's tenure-1 PUT");

        // (2) A loses; interim leader B extends the stored row to [0..6).
        state.on_lose();
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            6,
            24,
            false,
            None,
        )
        .await?;

        // (3) A re-acquires: the lease loop stores is_leader=true; the actor
        // has NOT dequeued LeaderAcquired (no rearm, no restamp).
        state.on_acquire(1);
        // Anti-vacuousness: the exact preconditions under which an un-gated
        // tick would shrink B's row.
        assert!(
            matches!(
                buffers.prefix_state(drv_path, exec_id),
                PrefixState::Checked
            ),
            "latch from tenure 1 must still be stale (no rearm/restamp yet)"
        );
        let (_, _, ring_lines, _, _) = buffers.snapshot(drv_path).expect("entry stamped");
        assert_eq!(
            ring_lines, 3,
            "retained ring is non-empty (an empty ring would also skip)"
        );
        let before: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            before,
            (0, 6, false),
            "B's coverage exceeds the retained ring"
        );

        // (4) The gap: periodic ticks fire while recovery is still pending.
        // Paused time + auto-advance drives the 30s interval; the gate
        // short-circuits before any PG/S3 I/O so paused time is safe here.
        tokio::time::pause();
        let (tx, rx) = mpsc::channel::<FlushRequest>(8);
        flusher.spawn(rx);
        tokio::time::sleep(Duration::from_secs(65)).await; // ticks at T=30, T=60
        tokio::time::resume();

        logs_assert(|lines: &[&str]| {
            let n = lines
                .iter()
                .filter(|l| l.contains("periodic flush deferred"))
                .count();
            if n >= 2 {
                Ok(())
            } else {
                Err(format!("two gap ticks → ≥2 deferrals, got {n}"))
            }
        });
        assert_eq!(puts.lock().unwrap().len(), 1, "gap ticks must not PUT");
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(buffers.active_count(), 1, "entry retained, not drained");
        let during: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            during,
            (0, 6, false),
            "interim leader's row must not shrink in the gap"
        );

        // (5) The actor dequeues LeaderAcquired: rearm (recover_from_pg part
        // 1 — this entry is in the not-restamped class), then recovery
        // completes. The worker re-streams only undelivered lines 6-7.
        assert_eq!(
            buffers.rearm_prefix_reconciliation(),
            1,
            "the stale latch existed"
        );
        state.set_recovery_complete(state.acquired_transitions());
        buffers.push(&mk_batch(drv_path, 6, &[b"c-6", b"c-7"]));

        // (6) Gate open: the next self-driven flush (channel-close arm —
        // same may_flush() gate, same flush_periodic()) folds B's coverage.
        drop(tx);
        tokio::time::timeout(Duration::from_secs(10), async {
            while !logs_contain("log flusher exited") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("flusher loop did not exit after channel close");

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("post-recovery merged PUT");
        assert!(key.ends_with(".partial.log.zst"), "still a snapshot: {key}");
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(
            decoded, "a-0\na-1\na-2\nb-3\nb-4\nb-5\nc-6\nc-7\n",
            "interim leader's lines survive once the gate opens"
        );
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "stored blob fetched once"
        );
        let after: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(after, (0, 8, false));
        Ok(())
    }

    /// A→B→A flap where the interim leader B receives lines but never flushes
    /// them: A's stored row ends BEFORE its retained head does, so after the
    /// reconcile the flush payload itself still carries an interior hole
    /// (B-only lines). The row must still describe the execution's true span —
    /// one past the highest line actually stored — so `since` cursors past the
    /// claimed-but-missing range are not told they are caught up; the blob
    /// then physically holds fewer lines than the row claims, which is the
    /// divergence the read path answers with a full re-serve.
    // r[verify obs.log.gap-span]
    #[tokio::test]
    async fn aba_flap_unflushed_interim_hole_periodic_records_true_span() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r13hole1-unflushed-interim.drv";
        let (s3, puts, _deletes, gets) = mock_s3_capturing_serving_last_put();
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // (1) Tenure A1: worker streams 0-2, A's periodic flush stores them
        //     (row {first_line: 0, line_count: 3}).
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"a-0", b"a-1", b"a-2"]);
        flusher.flush_periodic().await;
        assert_eq!(
            puts.lock().unwrap().len(),
            1,
            "fixture: A's first periodic PUT"
        );

        // (2) Lines 3-4 arrive after the tick — A's unflushed tail at lease loss.
        buffers.push(&mk_batch(drv_path, 3, &[b"a-3", b"a-4"]));

        // (3) Flap to B: the worker delivers 5-7 to B only; B's tenure is
        //     shorter than a periodic tick, so nothing is flushed and the
        //     stored row still ends at 3. (Nothing to do — that's the point.)

        // (4) Flap back to A: recovery restamps the same exec onto the
        //     retained entry and re-arms the stored-coverage check.
        buffers.set_exec(drv_path, exec_id, "test-worker");

        // (5) The worker reconnects to A and re-streams only undelivered
        //     lines (8-9): the ring is now {0..4, 8, 9} — an interior hole at
        //     5-7 that no stored row covers.
        buffers.push(&mk_batch(drv_path, 8, &[b"c-8", b"c-9"]));

        // (6) A's next periodic flush: reconcile folds A's own stored prefix
        //     (0-2), drops the superseded ring head, and uploads the rest.
        flusher.flush_periodic().await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("merged periodic PUT");
        assert!(key.ends_with(".partial.log.zst"), "still a snapshot: {key}");
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(
            decoded, "a-0\na-1\na-2\na-3\na-4\nc-8\nc-9\n",
            "blob carries the prefix plus everything this replica holds"
        );
        assert!(
            !decoded.contains("[rio:"),
            "an unflushed-interim hole gets no in-band marker"
        );
        // 7 physical lines vs a 10-line claimed span: the read path's
        // physical-vs-claimed check re-serves this blob from the start.
        assert_eq!(decoded.trim_end_matches('\n').split('\n').count(), 7);

        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            (row.0, row.1, row.2),
            (0, 10, false),
            "row end must be one past the highest stored line (9), not the physical count"
        );
        assert_eq!(
            gets.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "stored prefix fetched exactly once"
        );
        Ok(())
    }

    /// Same hole shape but the execution never produced a drv_logs row before
    /// the terminal (neither A's first tenure nor the interim leader ever
    /// flushed): there is no prefix to fold, the payload is the whole upload,
    /// and the final row must still claim the true span so a tail follower's
    /// `since` is not short-circuited past stored lines (`obs.log.gap-span`'s
    /// no-skip clause; the physical-vs-claimed divergence then drives the
    /// read-path re-serve).
    #[tokio::test]
    async fn interior_hole_with_no_stored_row_final_records_true_span() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r13hole2-no-row-final.drv";
        let drv_hash = "r13hole2";
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // Tenure A1 streams 0-4 (never flushed); the interim leader receives
        // 5-7 (also never flushed); A re-acquires (same-exec restamp) and the
        // worker re-streams from 8. Ring: {0..4, 8, 9}, no drv_logs row.
        let exec_id = stamp_and_push(
            &buffers,
            drv_path,
            &[b"a-0", b"a-1", b"a-2", b"a-3", b"a-4"],
        );
        buffers.set_exec(drv_path, exec_id, "test-worker");
        buffers.push(&mk_batch(drv_path, 8, &[b"c-8", b"c-9"]));

        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;

        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        let (key, body) = captured.last().expect("final PUT");
        assert_eq!(key, &format!("logs/{drv_hash}/{exec_id}.log.zst"));
        let decoded = String::from_utf8(zstd::decode_all(&body[..])?)?;
        assert_eq!(decoded, "a-0\na-1\na-2\na-3\na-4\nc-8\nc-9\n");
        assert!(
            !decoded.contains("[rio:"),
            "no marker without a recovered prefix"
        );

        let row: (i64, i64, bool, Option<String>) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete, status FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            (row.0, row.1),
            (0, 10),
            "no-prefix payload with a hole still claims the true span (end = 10)"
        );
        assert!(row.2, "final flush finalizes the row");
        assert_eq!(row.3.as_deref(), Some("succeeded"));
        assert_eq!(buffers.active_count(), 0, "final flush drains");
        Ok(())
    }

    /// Stored coverage extends past everything the retained ring holds and
    /// no new lines ever arrive (the worker never reconnected before the
    /// terminal): the reconcile truncates the ring to empty, the drain is
    /// empty, and the final degrades to the existing fresh-standby
    /// semantics — `finalize_empty_drain` stamps status/finished_at, the
    /// row keeps describing the stored `.partial`, `is_complete` stays
    /// false, and neither a new PUT nor a `.partial` delete happens.
    #[tokio::test]
    async fn stored_covers_ring_and_no_new_lines_final_keeps_partial() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r12aba7-covered.drv";
        let drv_hash = "r12aba7";
        let stored: Vec<&[u8]> = vec![b"s-0", b"s-1", b"s-2", b"s-3", b"s-4", b"s-5"];
        let stored_zst = compress_lines(&stored.iter().map(|l| l.to_vec()).collect::<Vec<_>>())?;
        let (s3, puts, deletes, gets) = mock_s3_capturing_with_get(Ok(stored_zst));
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        // A's retained ring holds only lines 0-2 (its unflushed tail at
        // lease loss); the interim leader's stored row covers [0..6).
        let exec_id = stamp_and_push(&buffers, drv_path, &[b"s-0", b"s-1", b"s-2"]);
        let partial_key = log_s3_key(drv_path, &exec_id, true);
        upsert_drv_log(
            &db.pool,
            exec_id,
            drv_hash,
            &partial_key,
            0,
            6,
            24,
            false,
            None,
        )
        .await?;
        // A re-acquires; the worker never reconnects; the terminal arrives.
        buffers.set_exec(drv_path, exec_id, "test-worker");
        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("failed".into()),
                lease_generation: 1,
            })
            .await;

        assert!(
            puts.lock().unwrap().is_empty(),
            "this tenure has nothing to add: no upload over the stored .partial"
        );
        assert!(
            deletes.lock().unwrap().is_empty(),
            "the stored .partial (the execution's only content) must not be deleted"
        );
        let row: (i64, i64, bool, Option<String>, Option<f64>) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete, status, \
                    EXTRACT(EPOCH FROM finished_at)::float8 \
             FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            (row.0, row.1),
            (0, 6),
            "row keeps describing the stored coverage"
        );
        assert!(!row.2, "is_complete stays false: stored content is partial");
        assert_eq!(row.3.as_deref(), Some("failed"));
        assert!(row.4.is_some(), "terminal stamp recorded");
        assert_eq!(gets.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(buffers.active_count(), 0, "the empty entry was drained");
        Ok(())
    }

    /// Defense in depth for the span computation: a ring carrying
    /// non-monotone worker numbering (only possible if the push_for
    /// ingestion gate regresses — simulated here via the legacy push(),
    /// which bypasses the gate) must neither panic the flusher nor wrap
    /// drv_logs.line_count negative. It records the physical line count and
    /// flags the fallback tripwire metric.
    // r[verify sched.executor.input-bounds+2]
    #[tokio::test]
    async fn non_monotone_ring_records_physical_count_not_wrapped_span() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r14mono1-out-of-order.drv";
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");
        // Legacy push() bypasses push_for's monotonicity gate: ring becomes
        // front=(1000,"late"), back=(0,"early") — the ingestion-regression shape.
        buffers.push(&mk_batch(drv_path, 1000, &[b"late"]));
        buffers.push(&mk_batch(drv_path, 0, &[b"early"]));

        let recorder = metrics_util::debugging::DebuggingRecorder::new();
        let snap = recorder.snapshotter();
        {
            let _guard = metrics::set_default_local_recorder(&recorder);
            flusher
                .flush_final(FlushRequest {
                    drv_path: drv_path.into(),
                    exec_id,
                    status: Some("succeeded".into()),
                    lease_generation: 1,
                })
                .await;
        }

        // Row is well-formed: physical count, never a negative/wrapped span.
        let row: (i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, is_complete FROM drv_logs WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            (row.0, row.1, row.2),
            (1000, 2, true),
            "physical-count fallback (2), not a wrapped span"
        );

        // Blob still uploaded with both lines, in arrival order.
        let captured: Vec<CapturedPut> = puts.lock().unwrap().clone();
        assert_eq!(captured.len(), 1);
        let decoded = String::from_utf8(zstd::decode_all(&captured[0].1[..])?)?;
        assert_eq!(decoded, "late\nearly\n");

        // Tripwire fired, labeled as a final flush.
        let counters = all_counters(&snap);
        let fb: Vec<_> = counters
            .iter()
            .filter(|(n, _, _)| n == "rio_scheduler_log_flush_span_fallback_total")
            .collect();
        assert_eq!(fb.len(), 1, "exactly one fallback series: {counters:?}");
        assert_eq!(fb[0].2, 1);
        assert!(fb[0].1.contains(&("kind".to_string(), "final".to_string())));
        Ok(())
    }

    /// Magnitude is deliberately NOT bounded at ingestion (forward jumps are
    /// legal and unbounded): a gate-legal batch numbered ≥ 2^63 must not
    /// sign-flip the drv_logs BIGINTs — the bind-site clamp records i64::MAX
    /// instead of a negative first_line.
    // r[verify sched.executor.input-bounds+2]
    #[tokio::test]
    async fn huge_monotone_line_numbers_clamp_at_bind_not_sign_flip() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let drv_path = "/nix/store/r14mono2-huge-line-numbers.drv";
        let (s3, puts, _deletes) = mock_s3_capturing();
        let buffers = Arc::new(LogBuffers::new());
        let flusher = LogFlusher::new(
            s3,
            "test-bucket".into(),
            db.pool.clone(),
            Arc::clone(&buffers),
            30,
            always_leader(),
        );

        let exec_id = Uuid::now_v7();
        buffers.set_exec(drv_path, exec_id, "test-worker");
        // Gate-legal input: ring is empty and the numbering does not overflow,
        // so push_for accepts it — this is exactly the shape the ingestion
        // gate is NOT meant to reject (magnitude is the clamp's job).
        assert!(buffers.push_for(
            drv_path,
            &mk_batch(drv_path, (i64::MAX as u64) + 5, &[b"huge"]),
            "test-worker"
        ));

        flusher
            .flush_final(FlushRequest {
                drv_path: drv_path.into(),
                exec_id,
                status: Some("succeeded".into()),
                lease_generation: 1,
            })
            .await;

        let row: (i64, i64, i64, bool) = sqlx::query_as(
            "SELECT first_line, line_count, total_bytes, is_complete FROM drv_logs \
             WHERE exec_id = $1",
        )
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(
            row.0,
            i64::MAX,
            "first_line clamps at i64::MAX, never negative"
        );
        assert_eq!(row.1, 1, "single-line span stays 1");
        assert!(row.2 > 0, "total_bytes stays physical and positive");
        assert!(row.3);
        assert_eq!(puts.lock().unwrap().len(), 1, "blob still uploaded");
        Ok(())
    }
}
