//! The per-stream ingest state machine for `LogService.AppendLog`.
//!
//! One [`IngestSession`] owns one builder log stream's state: it accepts
//! [`BuildLogBatch`]es (enforcing the input bounds relocated from the
//! scheduler's recv-task gates), buffers accepted lines, fans them out to
//! live-tail subscribers, cuts immutable chunks to S3 + the
//! `drv_log_chunks` manifest, and reports the durable high-water mark
//! that the handler acks back to the builder. The gRPC handler (a
//! sibling task) drives it from a `select!` loop: `accept` per inbound
//! batch, `cut` on the size trigger / the periodic timer / stream end,
//! `should_abort` after each failed cut.
//!
//! Every counter emitted by this module is registered in
//! `lib.rs::describe_metrics()`. `rio_store_log_ingest_streams_aborted_
//! total{reason}` is named here (via [`AbortReason::as_label`]) but
//! emitted by the handler ([`super::service`]) at its abort sites,
//! which also own the lease-lost and chunk-cap reasons this module
//! cannot observe.

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use rio_proto::types::BuildLogBatch;
use sqlx::PgPool;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tonic::Status;
use uuid::Uuid;

use super::chunks::{LogChunkError, LogChunkStore, compress_lines, log_chunk_key};
use super::kernel::{AcceptVerdict, accept_verdict};

/// Max bytes per stored line. Longer lines are truncated at accept so a
/// single line cannot dominate the buffer or a chunk.
///
/// Ported verbatim from the retired scheduler-side ring buffer's
/// MAX_LINE_LEN; the value is part of the worker-facing contract and
/// is owned by the kernel ([`rio_log_kernel::MAX_LINE_CONTENT_BYTES`])
/// — the builder's `LogBatcher` pre-truncates to the SAME constant
/// before transmitting (anything past it is discarded here anyway, and
/// an untruncated multi-MiB line would exceed the log plane's decode
/// cap); this truncation stays as the enforcement for non-conforming
/// workers.
pub const MAX_LINE_LEN: usize = rio_log_kernel::MAX_LINE_CONTENT_BYTES as usize;

/// Per-line memory overhead charged in all buffer accounting: the
/// `u64` line-number key (8 bytes) plus the `Vec<u8>` header (ptr +
/// len + cap = 24 bytes on 64-bit). Without it a stream of 1-byte
/// lines would hold ~33x its accounted bytes resident before the cut
/// trigger fired.
///
/// Owned by the kernel ([`rio_log_kernel::PER_LINE_OVERHEAD_BYTES`])
/// so the THREE buffer axes here (cut trigger, lifetime cap,
/// `buffer_bytes`) and the cutter's chunk budget
/// ([`rio_log_kernel::bounded_contiguous_prefix_len`]) charge the SAME
/// formula — bug_298 was exactly the drift between them: the buffer
/// axes charged this overhead while the chunk budget charged bare
/// framing, so one committed chunk could legally pack
/// `MAX_CHUNK_PAYLOAD_BYTES / 4` near-empty lines for the read path to
/// materialize.
const PER_LINE_OVERHEAD: u64 = rio_log_kernel::PER_LINE_OVERHEAD_BYTES;

/// The buffer-accounting size of one (post-truncation) line —
/// [`rio_log_kernel::charged_line_cost`], the kernel's single byte-axis
/// formula. Used for the cut trigger, the per-execution lifetime cap,
/// and the incremental `buffer_bytes` bookkeeping — all three must
/// agree on the formula or the counter drifts across a drain/restore
/// round trip.
fn accounted_len(line: &[u8]) -> u64 {
    rio_log_kernel::charged_line_cost(line.len() as u64)
}

/// Default for [`IngestConfig::per_exec_byte_cap`]: 1 GiB of accepted
/// bytes (post-truncation content plus `PER_LINE_OVERHEAD` per line)
/// over the lifetime of one execution's log. Closes the
/// unbounded-stored-bytes hole the scheduler's 16 MiB ring cap used to
/// bound only the *resident* portion of.
pub const DEFAULT_PER_EXEC_BYTE_CAP: u64 = 1024 * 1024 * 1024;

/// Default for [`IngestConfig::cut_threshold_bytes`]: cut a chunk once
/// 8 MiB of uncompressed lines are buffered (≈2 MiB compressed at the
/// typical 4:1 log ratio). The other two cut triggers (the periodic
/// timer and stream end) live in the handler.
pub const DEFAULT_CUT_THRESHOLD_BYTES: u64 = 8 * 1024 * 1024;

/// Default for [`IngestConfig::cut_interval`]: the handler's periodic
/// cut timer. Doubles as the basis for the gray-failure staleness bound
/// (see [`IngestSession::should_abort`]).
pub const DEFAULT_CUT_INTERVAL: Duration = Duration::from_secs(60);

/// After this many consecutive failed cut attempts the stream is aborted
/// so the builder fails over to another replica (the gray-failure
/// bound — a replica that can accept batches but cannot commit them must
/// not silently absorb lines forever).
const MAX_CONSECUTIVE_CUT_FAILURES: u8 = 3;

/// Tuning knobs for one ingest session. The handler builds this from the
/// store's config; [`Default`] carries the design's production values.
#[derive(Debug, Clone)]
pub struct IngestConfig {
    /// Abort the stream (`RESOURCE_EXHAUSTED`) once this many bytes
    /// (post-truncation content + `PER_LINE_OVERHEAD` per line) have
    /// been accepted for the execution.
    pub per_exec_byte_cap: u64,
    /// `accept` reports `cut_due` once the buffer holds this many bytes
    /// (post-truncation content + `PER_LINE_OVERHEAD` per line,
    /// uncompressed).
    pub cut_threshold_bytes: u64,
    /// The handler's periodic cut cadence; `should_abort`'s staleness
    /// bound is 2x this.
    pub cut_interval: Duration,
}

impl Default for IngestConfig {
    fn default() -> Self {
        Self {
            per_exec_byte_cap: DEFAULT_PER_EXEC_BYTE_CAP,
            cut_threshold_bytes: DEFAULT_CUT_THRESHOLD_BYTES,
            cut_interval: DEFAULT_CUT_INTERVAL,
        }
    }
}

/// Outcome of [`IngestSession::accept`] for one batch.
///
/// The two `Rejected*` variants drop the batch but keep the stream open
/// (per-batch rejection, matching the scheduler's recv-task gates): an
/// in-order batch after a rejected one is accepted again. Stream-fatal
/// conditions (the byte cap) are `Err(Status)` instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptOutcome {
    /// The batch was buffered and fanned out. `cut_due` is the size
    /// trigger: the buffer now holds at least
    /// [`IngestConfig::cut_threshold_bytes`] and the handler should call
    /// [`IngestSession::cut`].
    Accepted { cut_due: bool },
    /// The batch starts at or below a line number the session already
    /// holds or has cut. Malformed worker input (the `LogBatcher`
    /// contract is strictly monotone numbering per execution); renumbering
    /// it would corrupt every downstream consumer of the ordering, so it
    /// is dropped.
    RejectedNonMonotone,
    /// The batch's line numbers would overflow `u64` or exceed
    /// `i64::MAX` (the manifest stores line numbers as `BIGINT`; a line
    /// number that cannot round-trip through `drv_log_chunks.first_line`
    /// would corrupt the read path's attribution).
    RejectedOverflow,
    /// Every line in the batch is numbered at or past the execution's
    /// recorded `final_line_count`: the build's log ends below this
    /// batch's first line, so nothing in it can be part of the log
    /// (`store.log.completeness-gate`). The legitimate late replay
    /// never sends these — the count is the builder's own post-footer
    /// high-water mark from its `CompletionReport`.
    RejectedPastFinal,
    /// The batch holds more lines than one chunk's charged capacity
    /// ([`rio_log_kernel::MAX_BATCH_LINES`]) — bug_298's admission
    /// axis. Dropped whole BEFORE any per-line truncation, accounting,
    /// or fan-out work (the rejection is the cheap path by design);
    /// the stream stays open. Honest builders flush at 64 lines, four
    /// orders of magnitude below the bound.
    RejectedOversizedBatch,
}

/// Why [`IngestSession::should_abort`] wants the stream torn down.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AbortReason {
    /// `MAX_CONSECUTIVE_CUT_FAILURES` cut attempts in a row failed:
    /// the replica cannot commit chunks (S3 or PG partition). The
    /// builder should fail over to another replica and replay.
    ConsecutiveCutFailures,
    /// Lines have been waiting in the buffer for more than 2x
    /// [`IngestConfig::cut_interval`] without a successful cut: the
    /// backstop for a wedged replica whose cut attempts are not even
    /// reaching the failure counter.
    StaleBuffer,
}

impl AbortReason {
    /// The `reason` label for `rio_store_log_ingest_streams_aborted_total`.
    pub fn as_label(self) -> &'static str {
        match self {
            AbortReason::ConsecutiveCutFailures => "cut_failures",
            AbortReason::StaleBuffer => "stale_buffer",
        }
    }
}

/// A chunk-cut failure. The handler logs it, lets the failure counter
/// (already incremented by [`IngestSession::cut`]) feed
/// [`IngestSession::should_abort`], and keeps the stream open until the
/// abort threshold is reached — every failure mode here is either
/// transient (S3/PG blips, retried by the next cut) or terminal for the
/// replica (in which case the abort hands the builder to a healthier
/// one).
#[derive(Debug, thiserror::Error)]
pub enum CutError {
    /// zstd compression failed (effectively unreachable for in-memory
    /// I/O, but the encoder API is fallible).
    #[error("compressing log chunk: {0}")]
    Compress(#[source] LogChunkError),
    /// The compression task was cancelled or panicked.
    #[error("log chunk compression task failed: {0}")]
    Join(#[source] tokio::task::JoinError),
    /// The S3 PUT failed.
    #[error("storing log chunk: {0}")]
    Store(#[source] LogChunkError),
    /// The `drv_log_chunks` manifest INSERT failed. The object may
    /// already be durable in S3 — that is fine, the retry burns a new
    /// seq and the unreferenced object is invisible to readers (the S3
    /// lifecycle rule collects it).
    #[error("recording log chunk manifest row: {0}")]
    Manifest(#[source] sqlx::Error),
}

/// One live fan-out delivery: the accepted batch plus the ingest
/// session's accepted high-water mark at the instant it was fanned out
/// (the **coverage floor**). The floor is what lets a subscriber type
/// a forward jump (merged_bug_187): every line below it was accepted —
/// a gap there means the lossy fan-out dropped batches for THIS
/// subscriber and a backfill from manifest ∪ live buffer recovers it;
/// every line at or above it was never accepted — an ingest-admitted
/// hole no view can serve (`rio_log_kernel::visit_fanout_batch`).
pub struct FanBatch {
    /// The session's accepted high-water mark before this batch.
    pub coverage_floor: u64,
    /// The accepted batch, post-truncation (exactly what will be
    /// stored).
    pub batch: BuildLogBatch,
}

/// State shared between an ingest session and its live-tail subscribers.
///
/// ONE mutex over the buffer, the in-flight staging area, and the
/// subscriber list. The seam invariant this lock enforces: **at every
/// instant, every accepted line is in exactly one of three places — the
/// `buffer` (not yet being cut), `in_flight` (drained by a cut whose
/// commit has not resolved), or a manifest-visible chunk.** A `TailLog`
/// reader that registers a subscriber and snapshots `in_flight ++
/// buffer` in one critical section ([`Self::subscribe`]) and then reads
/// the manifest therefore sees every line at least once (possibly
/// twice, when a cut commits between the snapshot and the manifest
/// read — the reader's line-number dedup removes the overlap; never
/// zero times).
///
/// The lock is a `std::sync::Mutex` and is never held across an
/// `.await`: the cutter stages the drained run into `in_flight` under
/// it, does the compress/PUT/INSERT outside it, and re-acquires it to
/// either clear `in_flight` (commit succeeded) or fold `in_flight` back
/// onto the front of `buffer` (commit failed).
pub struct IngestShared {
    /// Accepted-but-not-yet-drained lines, in accept order.
    /// `(absolute line number, post-truncation bytes)`. Line numbers are
    /// non-decreasing and strictly increasing across batches; forward
    /// gaps are legal and are always WORKER-ADMITTED holes — a jump the
    /// builder transmitted and the ingest accepted (merged_bug_275:
    /// builder-side suppression never skips numbers; the old
    /// "(suppressed lines)" attribution here was wrong — see
    /// rio-builder's contiguity property test).
    buffer: Vec<(u64, Vec<u8>)>,
    /// The contiguous run drained by the cut currently in flight, if
    /// any. Kept here — not in a `cut()` local — so a subscriber that
    /// registers while the commit is awaiting S3/PG still sees these
    /// lines in its snapshot. Older than everything in `buffer`.
    in_flight: Vec<(u64, Vec<u8>)>,
    /// Sum of [`accounted_len`] over `buffer` (NOT `in_flight`: lines
    /// being cut must not re-trigger `cut_due`). Maintained
    /// incrementally so the trigger check is O(1).
    buffer_bytes: u64,
    /// When the oldest not-yet-durable line (in `in_flight` or
    /// `buffer`) was accepted. `None` only when both are empty.
    /// Deliberately conservative: a partial drain (a cut that stopped at
    /// a forward gap) leaves it at the *older* timestamp, which can only
    /// make the staleness abort fire earlier.
    oldest_pending_since: Option<Instant>,
    /// Live-tail subscribers. Each accepted batch is `try_send`-fanned
    /// to every sender; a full queue drops the batch for that subscriber
    /// only. Closed senders are pruned on the next fan-out. The payload
    /// is `Arc`-shared: the fan-out batch is built once per accepted
    /// batch (and only when at least one subscriber exists — most
    /// builds are unwatched) and the per-subscriber cost is a refcount
    /// bump, not a multi-MB lines clone inside the critical section.
    subscribers: Vec<mpsc::Sender<Arc<FanBatch>>>,
    /// Batches dropped because a subscriber's queue was full. Exposed
    /// for the handler's `rio_store_log_tail_dropped_total` gauge
    /// reconciliation and asserted by tests; the counter metric is also
    /// emitted at the drop site.
    pub tail_dropped: u64,
    /// Bumped whenever the fan-out drops batches, so a subscriber
    /// whose BURST-END batches were dropped can run its backfill
    /// without waiting for the next accepted batch (which for a build
    /// that has gone quiet may never come) — merged_bug_187's
    /// parked-tail half. A `watch` epoch, NOT a `Notify`: the serve
    /// loop is frequently parked in `send_lines` (a stalled reader is
    /// exactly the dropping case) when the drop happens, and an
    /// edge-triggered wake fired with no waiter is lost; the watch
    /// channel latches — `changed()` observes any bump made while the
    /// loop was away.
    drop_epoch: watch::Sender<u64>,
}

impl IngestShared {
    /// Register a live-tail subscriber and atomically snapshot every
    /// accepted line that is not yet manifest-visible: the in-flight
    /// run (if a cut is mid-commit) followed by the buffer. The
    /// returned snapshot plus the manifest's chunks plus everything
    /// subsequently delivered on `tx` is the complete log (with
    /// possible line-number overlap between the three, removed by the
    /// reader's dedup — never a gap).
    pub fn subscribe(
        &mut self,
        tx: mpsc::Sender<Arc<FanBatch>>,
        since: u64,
    ) -> Vec<(u64, Vec<u8>)> {
        let snapshot = self.snapshot_since(since);
        self.subscribers.push(tx);
        snapshot
    }

    /// A latching subscription to fan-out drop epochs (see the field
    /// doc). Held by serve loops outside the lock.
    pub fn drop_watch(&self) -> watch::Receiver<u64> {
        self.drop_epoch.subscribe()
    }

    /// The not-yet-manifest-visible lines (the in-flight run followed by
    /// the buffer) WITHOUT registering a subscriber. For one-shot
    /// (non-follow) reads that want the latest output but no live
    /// subscription — registering a sender whose receiver is
    /// immediately dropped would be pruned on the next fan-out anyway,
    /// but never registering it is cheaper and clearer.
    pub fn snapshot(&self) -> Vec<(u64, Vec<u8>)> {
        self.snapshot_since(0)
    }

    /// [`Self::snapshot`] bounded below: only lines at or past `since`
    /// are cloned, so a reader that is already deep into the log does
    /// not pay (inside the lock) for a clone of lines its cursor will
    /// immediately discard (merged_bug_187's in-lock amplification
    /// half).
    pub fn snapshot_since(&self, since: u64) -> Vec<(u64, Vec<u8>)> {
        self.in_flight
            .iter()
            .chain(self.buffer.iter())
            .filter(|(n, _)| *n >= since)
            .cloned()
            .collect()
    }

    /// Fold the in-flight run back onto the front of the buffer (the
    /// in-flight lines are older than everything in `buffer`) and
    /// re-account its bytes. Used by the cut's failure arm and by the
    /// next cut's recovery from an abandoned (cancelled-mid-await)
    /// predecessor.
    fn restore_in_flight(&mut self) {
        if self.in_flight.is_empty() {
            return;
        }
        let restored_bytes: u64 = self.in_flight.iter().map(|(_, l)| accounted_len(l)).sum();
        let mut restored = std::mem::take(&mut self.in_flight);
        restored.append(&mut self.buffer);
        self.buffer = restored;
        self.buffer_bytes += restored_bytes;
        // oldest_pending_since already covers in-flight lines (it is
        // only cleared when both vecs are empty), so nothing to update.
    }
}

/// The per-stream ingest state machine. See the module doc for how the
/// handler drives it.
pub struct IngestSession {
    pub exec_id: Uuid,
    pub session_id: Uuid,
    /// The normalized 32-char `drv_log_hash` form from `GateOk` — the
    /// chunk-key prefix, NOT the DAG key.
    drv_hash: String,
    shared: Arc<Mutex<IngestShared>>,
    config: IngestConfig,
    /// The next chunk seq to *attempt*. Incremented per cut attempt, not
    /// per cut success: a failed attempt's seq is burned forever so no
    /// S3 key is ever re-PUT with a different line range (the
    /// [`super::chunks::PutOutcome::Existed`] caller contract).
    next_seq: u32,
    /// Committed chunks the execution already had at open time (the
    /// gate's durable seed). Added to `next_seq` by
    /// [`Self::chunk_attempts`] so the per-execution chunk cap, like
    /// the byte cap, survives reconnects.
    prior_chunks: u32,
    /// The lowest acceptable `first_line_number` for the next batch:
    /// one past the last accepted line. Forward gaps are allowed
    /// (`first_line_number > high_water_line`); going backwards is not.
    high_water_line: u64,
    /// The exclusive upper bound on acceptable line numbers: the
    /// execution's recorded `final_line_count`, once known. Seeded
    /// from the open-time gate for an already-terminal execution (the
    /// late replay) and refreshed by the handler's heartbeat tick for
    /// a seal that lands mid-stream. `None` until the scheduler stamps
    /// the lifecycle row terminal with a known count — until then no
    /// ceiling applies.
    final_line_count: Option<u64>,
    /// Total accounted bytes accepted over the EXECUTION's lifetime:
    /// seeded from the durable manifest at open
    /// (`GateOk::prior_accounted_bytes`), grown per accepted batch,
    /// never decremented on cut. Compared against
    /// [`IngestConfig::per_exec_byte_cap`].
    accepted_bytes: u64,
    consecutive_cut_failures: u8,
}

impl IngestSession {
    // r[impl store.log.caps-durable]
    /// The ONLY constructor, and it takes a `GateOk`: a session
    /// cannot exist without the gate's durable-account read, so the
    /// per-execution caps are seeded from the committed manifest —
    /// never from zero — on every open (merged_bug_207). `next_seq`
    /// stays session-scoped (chunk keys are
    /// `(exec_id, session_id, chunk_seq)`); the durable chunk count
    /// rides separately in `prior_chunks` and is added back by
    /// [`Self::chunk_attempts`].
    pub fn new(gate_ok: &super::gate::GateOk, session_id: Uuid, config: IngestConfig) -> Self {
        let mut s = Self {
            exec_id: gate_ok.exec_id,
            session_id,
            drv_hash: gate_ok.drv_hash.clone(),
            shared: Arc::new(Mutex::new(IngestShared {
                buffer: Vec::new(),
                in_flight: Vec::new(),
                buffer_bytes: 0,
                oldest_pending_since: None,
                subscribers: Vec::new(),
                tail_dropped: 0,
                drop_epoch: watch::channel(0u64).0,
            })),
            config,
            next_seq: 0,
            prior_chunks: gate_ok.prior_chunks,
            high_water_line: 0,
            final_line_count: None,
            accepted_bytes: gate_ok.prior_accounted_bytes,
            consecutive_cut_failures: 0,
        };
        if let Some(n) = gate_ok.final_line_count {
            // The execution is already terminal with a recorded end
            // (the late-replay case): the session is born knowing its
            // append ceiling. A negative count is unrepresentable on
            // the write side (the scheduler stores the proto's u64);
            // clamp defensively rather than wrap.
            // r[impl store.log.completeness-gate]
            s.set_final_line_count(n.max(0) as u64);
        }
        s
    }

    /// The per-append ceiling, if the execution's recorded end is
    /// known yet. The handler's periodic refresh uses `is_none()` to
    /// decide whether it still needs to consult the lifecycle row.
    pub fn final_line_count(&self) -> Option<u64> {
        self.final_line_count
    }

    /// The lowest acceptable `first_line_number` for the next batch
    /// (one past the last accepted line). Read-only: only
    /// [`Self::accept`] raises it. Exposed so the model-conformance
    /// projection (`mbt_tests`) can compare it against the model's
    /// `highWater` without reaching into the private field.
    pub fn high_water_line(&self) -> u64 {
        self.high_water_line
    }

    /// Record the execution's `final_line_count` as the exclusive
    /// upper bound on acceptable line numbers
    /// (`store.log.completeness-gate`: accepted lines numbered at or
    /// past it are dropped). The count is stamped once at terminal and
    /// never changes, so the first observed value wins and later calls
    /// are no-ops.
    pub fn set_final_line_count(&mut self, n: u64) {
        self.final_line_count.get_or_insert(n);
    }

    /// The shared buffer + subscriber-list handle. The `TailLog` path
    /// clones this to [`IngestShared::subscribe`] under the same lock
    /// the cutter drains under.
    pub fn shared(&self) -> &Arc<Mutex<IngestShared>> {
        &self.shared
    }

    /// Is the buffer at or over the size-triggered cut threshold right
    /// now? The handler treats the size trigger as **level-triggered**:
    /// after a cut it re-checks this and keeps cutting, because a
    /// buffer containing forward line-number gaps drains one contiguous
    /// run per [`Self::cut`] call and may still be over the threshold
    /// afterwards.
    pub fn cut_due(&self) -> bool {
        self.lock_shared().buffer_bytes >= self.config.cut_threshold_bytes
    }

    /// True iff nothing is buffered. The AppendLog driver's
    /// inbound-idle abort is gated on this: with an empty buffer an
    /// idle-abort can lose nothing, while a non-empty buffer's
    /// liveness is owned by the cut path's bounded ack send.
    pub fn buffer_is_empty(&self) -> bool {
        self.lock_shared().buffer_bytes == 0
    }

    /// How many chunk-cut attempts this session has made (== the number
    /// of S3 keys it may have created — a failed attempt's PUT may have
    /// committed even though its manifest row was never written). The
    /// handler compares this against `log_max_chunks_per_exec` to bound
    /// the per-execution object count against a builder fabricating
    /// forward gaps to force one chunk per run.
    pub fn chunk_attempts(&self) -> u32 {
        self.prior_chunks.saturating_add(self.next_seq)
    }

    /// The session's tuning knobs (the handler reads `cut_interval` for
    /// its periodic timer).
    pub fn config(&self) -> &IngestConfig {
        &self.config
    }

    /// Recover the shared lock even if a holder panicked: the data is a
    /// plain buffer with no invariants that a panic mid-update could
    /// break (the only multi-field update is append + byte-count, and a
    /// torn one of those costs at most one miscounted batch).
    fn lock_shared(&self) -> MutexGuard<'_, IngestShared> {
        self.shared.lock().unwrap_or_else(|e| e.into_inner())
    }

    // r[impl store.log.ingest-bounds]
    /// Validate, buffer, and fan out one batch.
    ///
    /// `Ok(Accepted { cut_due })` / `Ok(Rejected*)` keep the stream
    /// open; `Err` is stream-fatal (the handler converts it to the
    /// stream's error status). Synchronous: nothing here awaits, and the
    /// shared lock is held only for the append + fan-out.
    pub fn accept(&mut self, batch: BuildLogBatch) -> Result<AcceptOutcome, Status> {
        if batch.lines.is_empty() {
            // Nothing to buffer, nothing to check (matching the
            // scheduler's gate, which skips the numbering checks for
            // empty batches). Report the current size trigger so an
            // empty keepalive batch cannot mask a due cut.
            let cut_due = self.lock_shared().buffer_bytes >= self.config.cut_threshold_bytes;
            return Ok(AcceptOutcome::Accepted { cut_due });
        }

        // -- The input gates, delegated to the pure kernel
        // (`kernel::accept_verdict`). The numbering is worker-supplied
        // and the worker is untrusted; everything downstream (the
        // manifest's [first_line, first_line + line_count) arithmetic,
        // the read path's attribution, the completeness fold) relies on
        // it being monotone and representable as BIGINT. Once the
        // execution's lifecycle row is terminal with a known
        // `final_line_count`, that count is one past the last line the
        // build produced (the builder's own post-footer high-water mark
        // from its CompletionReport): lines numbered at or past it
        // cannot be part of the log and are dropped, so a builder
        // holding a still-current token for a terminal-but-incomplete
        // execution can fill the gap below the recorded end but never
        // grow the log past it. A batch starting at or past the ceiling
        // is dropped whole; one that straddles it keeps only the lines
        // below it. Lines accepted before the ceiling is learned (the
        // seal lands mid-stream and the handler's refresh has not
        // observed it yet) are the bounded residual disclosed at the
        // refresh site. The kernel owns the verdict; this block owns
        // the per-verdict metrics and logging.
        // r[impl store.log.completeness-gate]
        let line_count = batch.lines.len() as u64;
        let end = match accept_verdict(
            self.high_water_line,
            self.final_line_count,
            batch.first_line_number,
            line_count,
        ) {
            AcceptVerdict::RejectedOverflow => {
                metrics::counter!(
                    "rio_store_log_ingest_rejected_total",
                    "reason" => "line_number_overflow"
                )
                .increment(1);
                tracing::debug!(
                    exec_id = %self.exec_id,
                    first_line_number = batch.first_line_number,
                    lines = batch.lines.len(),
                    "rejected log batch: line numbering would overflow"
                );
                return Ok(AcceptOutcome::RejectedOverflow);
            }
            AcceptVerdict::RejectedNonMonotone => {
                metrics::counter!(
                    "rio_store_log_ingest_rejected_total",
                    "reason" => "non_monotonic"
                )
                .increment(1);
                tracing::debug!(
                    exec_id = %self.exec_id,
                    first_line_number = batch.first_line_number,
                    high_water_line = self.high_water_line,
                    "rejected log batch: non-monotone first_line_number"
                );
                return Ok(AcceptOutcome::RejectedNonMonotone);
            }
            AcceptVerdict::RejectedPastFinal => {
                metrics::counter!(
                    "rio_store_log_ingest_rejected_total",
                    "reason" => "past_final_line_count"
                )
                .increment(1);
                tracing::debug!(
                    exec_id = %self.exec_id,
                    first_line_number = batch.first_line_number,
                    // RejectedPastFinal is only reachable with a known
                    // ceiling; `unwrap_or` keeps the log site total.
                    final_line_count = self.final_line_count.unwrap_or(0),
                    "rejected log batch: every line is at or past the recorded final_line_count"
                );
                return Ok(AcceptOutcome::RejectedPastFinal);
            }
            AcceptVerdict::RejectedOversizedBatch => {
                metrics::counter!(
                    "rio_store_log_ingest_rejected_total",
                    "reason" => "oversized_batch"
                )
                .increment(1);
                tracing::warn!(
                    exec_id = %self.exec_id,
                    first_line_number = batch.first_line_number,
                    lines = batch.lines.len(),
                    max_batch_lines = rio_log_kernel::MAX_BATCH_LINES,
                    "rejected log batch: more lines than one chunk's charged capacity"
                );
                return Ok(AcceptOutcome::RejectedOversizedBatch);
            }
            AcceptVerdict::Accepted { end } => end,
        };
        let mut batch_lines = batch.lines;
        if end < batch.first_line_number + line_count {
            // Straddles the recorded end: keep [first, end), drop
            // [end, first + line_count). `end` was clamped to the
            // ceiling by the kernel, so this branch fires exactly when
            // the un-clamped end exceeded it.
            let keep = (end - batch.first_line_number) as usize;
            let dropped = batch_lines.len() - keep;
            batch_lines.truncate(keep);
            metrics::counter!(
                "rio_store_log_ingest_rejected_total",
                "reason" => "past_final_line_count"
            )
            .increment(1);
            tracing::debug!(
                exec_id = %self.exec_id,
                first_line_number = batch.first_line_number,
                final_line_count = self.final_line_count.unwrap_or(0),
                dropped,
                "truncated log batch at the recorded final_line_count"
            );
        }

        // -- Truncation BEFORE byte accounting, so an oversized line
        // cannot defeat the byte cap (the scheduler's gate has the same
        // ordering for the same reason).
        let lines: Vec<Vec<u8>> = batch_lines
            .into_iter()
            .map(|line| {
                if line.len() > MAX_LINE_LEN {
                    // Slice-then-to_vec, not clone-then-truncate:
                    // Vec::truncate does not shrink the allocation.
                    line[..MAX_LINE_LEN].to_vec()
                } else {
                    line
                }
            })
            .collect();
        // Two byte counts: `content_bytes` is what the data-volume
        // metric reports; `batch_bytes` adds PER_LINE_OVERHEAD per line
        // and is what every resource bound (the cut trigger, the
        // lifetime cap, buffer_bytes) is charged — they bound memory and
        // storage, and a line costs its header even when its content is
        // one byte. Both (and the accepted-lines metric) are computed
        // over the post-ceiling, post-truncation lines: only what is
        // actually buffered is counted or charged.
        let accepted_line_count = lines.len() as u64;
        let content_bytes: u64 = lines.iter().map(|l| l.len() as u64).sum();
        let batch_bytes: u64 = lines.iter().map(|l| accounted_len(l)).sum();

        // -- The per-execution accepted-bytes cap. Stream-fatal: the
        // builder gets RESOURCE_EXHAUSTED and gives up on the log (the
        // build itself is unaffected).
        // r[impl store.log.caps-durable]
        // Same status code + metadata class as the gate's open-time
        // check: the cap travels with the EXECUTION, so a retry on
        // another replica cannot succeed — FAILED_PRECONDITION (the
        // builder classifies it permanent), never RESOURCE_EXHAUSTED
        // (reserved for per-replica capacity).
        if self.accepted_bytes.saturating_add(batch_bytes) > self.config.per_exec_byte_cap {
            metrics::counter!(
                "rio_store_log_ingest_rejected_total",
                "reason" => "byte_cap"
            )
            .increment(1);
            let mut status = Status::failed_precondition(format!(
                "AppendLog: execution exceeded the {}-byte log ingest cap",
                self.config.per_exec_byte_cap
            ));
            status.metadata_mut().insert(
                rio_proto::LOG_REJECT_METADATA_KEY,
                tonic::metadata::MetadataValue::from_static("cap"),
            );
            return Err(status);
        }

        // -- Buffer + fan out under one critical section. The fan-out
        // batch carries the truncated lines so subscribers see exactly
        // what will be stored. It is built lazily — only when at least
        // one subscriber exists (most builds are unwatched) — and
        // `Arc`-shared so the per-subscriber cost inside the critical
        // section is a refcount bump, not a lines clone.
        let cut_due = {
            let mut shared = self.lock_shared();
            if shared.buffer.is_empty() && shared.in_flight.is_empty() {
                shared.oldest_pending_since = Some(Instant::now());
            }

            // Fan out BEFORE the lines move into the buffer (the
            // buffer extend consumes `lines`). Pruning closed
            // subscribers and counting drops on full ones; try_send
            // never blocks, so a slow reader can never backpressure
            // ingest.
            if !shared.subscribers.is_empty() {
                let fanout = Arc::new(FanBatch {
                    // The session's accepted high-water mark BEFORE
                    // this batch: the subscriber-side gap classifier
                    // (`visit_fanout_batch`) splits any observed jump
                    // at this floor — below it the store accepted the
                    // lines (a drop, recoverable), at or above it the
                    // worker never sent them (an admitted hole).
                    coverage_floor: self.high_water_line,
                    batch: BuildLogBatch {
                        derivation_path: batch.derivation_path,
                        lines: lines.clone(),
                        first_line_number: batch.first_line_number,
                        executor_id: batch.executor_id,
                    },
                });
                let mut dropped = 0u64;
                shared
                    .subscribers
                    .retain(|tx| match tx.try_send(Arc::clone(&fanout)) {
                        Ok(()) => true,
                        Err(mpsc::error::TrySendError::Full(_)) => {
                            dropped += 1;
                            true
                        }
                        Err(mpsc::error::TrySendError::Closed(_)) => false,
                    });
                if dropped > 0 {
                    shared.tail_dropped += dropped;
                    metrics::counter!("rio_store_log_tail_dropped_total").increment(dropped);
                    // Burst-end drops must serve without new output:
                    // bump the latched epoch so every subscriber's
                    // serve loop backfills as soon as it next selects,
                    // even if it was mid-send when the drop happened.
                    shared.drop_epoch.send_modify(|n| *n = n.wrapping_add(1));
                }
            }

            shared.buffer.extend((batch.first_line_number..).zip(lines));
            shared.buffer_bytes += batch_bytes;
            shared.buffer_bytes >= self.config.cut_threshold_bytes
        };

        self.high_water_line = end;
        self.accepted_bytes += batch_bytes;
        metrics::counter!("rio_store_log_ingest_lines_total").increment(accepted_line_count);
        metrics::counter!("rio_store_log_ingest_bytes_total").increment(content_bytes);

        Ok(AcceptOutcome::Accepted { cut_due })
    }

    // r[impl store.log.chunk-immutable]
    /// Cut the longest contiguous prefix of the buffer into one
    /// immutable chunk: compress, PUT, record the manifest row, and
    /// return the durable-through line number for the ack.
    ///
    /// `Ok(None)` means the buffer was empty (no seq is consumed).
    /// `Ok(Some(n))` means lines through `n` are durable. A buffer
    /// containing a forward gap drains one contiguous run per call —
    /// the handler's drain-on-stream-end loop calls `cut` until it
    /// returns `Ok(None)` — because a chunk's manifest row describes a
    /// contiguous `[first_line, first_line + line_count)` and a chunk
    /// spanning a gap would mis-attribute every post-gap line.
    ///
    /// On failure the staged run is folded back onto the front of the
    /// buffer (in front of anything that arrived during the attempt),
    /// the failure counter feeding [`Self::should_abort`] is bumped, and
    /// the attempt's seq number stays burned (see `next_seq`).
    ///
    /// Not cancellation-safe in the "no work is lost" sense — if the
    /// future is dropped between the drain and the commit resolving, the
    /// staged run stays in `in_flight` (still visible to subscribers,
    /// never lost) and the *next* `cut` call folds it back into the
    /// buffer before draining. The handler should still drive each
    /// `cut` to completion rather than racing it in a `select!`.
    pub async fn cut(
        &mut self,
        store: &dyn LogChunkStore,
        pool: &PgPool,
    ) -> Result<Option<u64>, CutError> {
        // Stage the contiguous prefix into `in_flight` under the lock;
        // do the slow work outside it. The lines stay in the shared
        // struct so a subscriber registering mid-commit still sees them.
        let (first_line, lines) = {
            let mut shared = self.lock_shared();
            // A predecessor cut abandoned mid-await (cancelled future)
            // leaves its run staged: neither committed nor restored.
            // Fold it back in front of the buffer — the line order is
            // preserved (in-flight lines are older) — and re-drain, so
            // an abandoned cut degrades to a retried cut instead of
            // silently overwriting the staged run below.
            shared.restore_in_flight();
            if shared.buffer.is_empty() {
                return Ok(None);
            }
            // Bounded in BOTH dimensions (store.log.write-read-bound):
            // contiguity decides what one manifest row may describe,
            // and the framed-payload bound keeps the chunk decodable by
            // the read path. An over-bound contiguous run drains as
            // multiple chunks across the level-triggered cut loops
            // (`cut_while_due`, the final drain) — each call takes the
            // next bounded slice.
            let run_len = super::kernel::bounded_contiguous_prefix_len(
                shared.buffer.iter().map(|(n, l)| (*n, l.len() as u64)),
                super::kernel::MAX_CHUNK_PAYLOAD_BYTES,
            );
            let rest = shared.buffer.split_off(run_len);
            shared.in_flight = std::mem::replace(&mut shared.buffer, rest);
            let drained_bytes: u64 = shared.in_flight.iter().map(|(_, l)| accounted_len(l)).sum();
            shared.buffer_bytes -= drained_bytes;
            // `oldest_pending_since` is NOT cleared here: the staged
            // lines are still pending (not durable) until the commit
            // succeeds.
            //
            // The run is contiguous: line i is line `first_line + i`.
            // Clone the bytes for the compression task; the originals
            // stay in `in_flight` for subscriber visibility and for the
            // failure restore. (The clone is not new cost — the previous
            // shape cloned the same lines to keep a restore copy.)
            let first_line = shared.in_flight[0].0;
            let lines: Vec<Vec<u8>> = shared.in_flight.iter().map(|(_, l)| l.clone()).collect();
            (first_line, lines)
        };
        let line_count = lines.len() as u64;
        let last_line = first_line + line_count - 1;

        // The seq is consumed by the ATTEMPT. Taken after the emptiness
        // check (an empty cut burns nothing) but before anything that
        // can fail.
        let seq = self.next_seq;
        self.next_seq += 1;

        match self.commit_chunk(store, pool, seq, first_line, lines).await {
            Ok(()) => {
                let mut shared = self.lock_shared();
                // The staged run is durable and manifest-visible: drop
                // it (freeing the line allocations and the staging vec).
                shared.in_flight = Vec::new();
                if shared.buffer.is_empty() {
                    shared.oldest_pending_since = None;
                }
                drop(shared);
                self.consecutive_cut_failures = 0;
                metrics::counter!("rio_store_log_chunks_written_total").increment(1);
                Ok(Some(last_line))
            }
            Err(e) => {
                // Fold the staged run back onto the front of the buffer:
                // lines that arrived during the failed attempt are
                // behind it and must stay behind it.
                self.lock_shared().restore_in_flight();
                self.consecutive_cut_failures = self.consecutive_cut_failures.saturating_add(1);
                metrics::counter!("rio_store_log_chunk_write_failures_total").increment(1);
                Err(e)
            }
        }
    }

    /// The fallible middle of [`Self::cut`]: compress, PUT, INSERT.
    /// Factored out so the caller's failure arm restores the buffer in
    /// exactly one place.
    async fn commit_chunk(
        &self,
        store: &dyn LogChunkStore,
        pool: &PgPool,
        seq: u32,
        first_line: u64,
        lines: Vec<Vec<u8>>,
    ) -> Result<(), CutError> {
        let line_count = lines.len() as i64;
        // The durable per-execution account (`store.log.caps-durable`):
        // the same content + PER_LINE_OVERHEAD formula every in-memory
        // bound charges, persisted with the manifest row so the next
        // open's gate seed resumes the cap exactly where this session
        // left it.
        let accounted_bytes: i64 = lines
            .iter()
            .map(|l| accounted_len(l))
            .sum::<u64>()
            .min(i64::MAX as u64) as i64;

        // A few MiB of zstd takes ~10-50 ms: long enough to stall a
        // tokio worker, so compress on the blocking pool.
        let blob = tokio::task::spawn_blocking(move || compress_lines(&lines))
            .await
            .map_err(CutError::Join)?
            .map_err(CutError::Compress)?;
        let byte_size = blob.len() as i64;

        // Object first, manifest row second: a crash between the two
        // leaves an unreferenced object (collected by the S3 lifecycle
        // rule), never a manifest row pointing at nothing.
        let key = log_chunk_key(&self.drv_hash, &self.exec_id, &self.session_id, seq);
        store.put(&key, blob).await.map_err(CutError::Store)?;

        // ON CONFLICT DO NOTHING: the (exec_id, session_id, chunk_seq)
        // PK makes a re-INSERT after a lost response idempotent. Runtime
        // query: drv_log_chunks is store-owned (no cross-service
        // contract to enforce).
        sqlx::query(
            "INSERT INTO drv_log_chunks \
             (exec_id, session_id, chunk_seq, first_line, line_count, byte_size, s3_key, \
              accounted_bytes) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
             ON CONFLICT DO NOTHING",
        )
        .bind(self.exec_id)
        .bind(self.session_id)
        .bind(seq as i32)
        .bind(first_line as i64)
        .bind(line_count)
        .bind(byte_size)
        .bind(&key)
        .bind(accounted_bytes)
        .execute(pool)
        .await
        .map_err(CutError::Manifest)?;

        Ok(())
    }

    /// A cut attempt was abandoned by the driver's watchdog (the cut
    /// future hung past its bound and was dropped): count it exactly
    /// like an answered cut failure. The dropped future never reached
    /// the `Err` arm of [`Self::cut`], so without this the wedge
    /// produced zero countable failures (merged_bug_119); the staged
    /// run is folded back by the next cut's `restore_in_flight` — an
    /// abandoned cut degrades to a retried cut.
    pub fn note_cut_abandoned(&mut self) {
        self.consecutive_cut_failures = self.consecutive_cut_failures.saturating_add(1);
        metrics::counter!("rio_store_log_chunk_write_failures_total").increment(1);
    }

    /// The gray-failure bound: should the handler abort this stream with
    /// `UNAVAILABLE` so the builder fails over to a replica that can
    /// actually commit chunks?
    ///
    /// The trigger is `MAX_CONSECUTIVE_CUT_FAILURES` cut attempts in a
    /// row that failed OR were abandoned by the watchdog (a hung cut is
    /// counted via [`Self::note_cut_abandoned`], so a wedged replica
    /// always produces countable failures). The staleness check is a
    /// backstop CONDITIONED on at least one observed failure
    /// (merged_bug_119: pure staleness with zero failures is a healthy
    /// paced multi-run drain — a multi-MiB buffer legitimately takes
    /// several cut intervals to drain one run at a time and must not be
    /// aborted mid-drain). Aborting drops the in-memory buffer — that
    /// is safe, the builder's retransmit buffer still holds every
    /// un-acked line and replays it to the next replica.
    pub fn should_abort(&self) -> Option<AbortReason> {
        if self.consecutive_cut_failures >= MAX_CONSECUTIVE_CUT_FAILURES {
            return Some(AbortReason::ConsecutiveCutFailures);
        }
        let shared = self.lock_shared();
        if self.consecutive_cut_failures > 0
            && let Some(since) = shared.oldest_pending_since
            && since.elapsed() > 2 * self.config.cut_interval
        {
            return Some(AbortReason::StaleBuffer);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use rio_test_support::TestDb;
    use sqlx::PgPool;
    use uuid::Uuid;

    use super::*;
    use crate::logs::chunks::{
        LogChunkError, LogChunkStore, MemoryLogChunkStore, PutOutcome, decompress_lines,
    };

    /// Tiny thresholds so tests exercise the trigger logic without
    /// allocating MiBs: the cut-size trigger fires at 1 KiB and the
    /// per-execution cap at 1 MiB. The production defaults live in
    /// [`IngestConfig::default`].
    fn test_config() -> IngestConfig {
        IngestConfig {
            per_exec_byte_cap: 1024 * 1024,
            cut_threshold_bytes: 1024,
            cut_interval: Duration::from_secs(60),
        }
    }

    fn new_session(config: IngestConfig) -> IngestSession {
        new_session_with_seed(config, 0, 0)
    }

    /// A session born from a synthetic `GateOk` carrying a durable
    /// seed, as after a reconnect to an execution with committed
    /// chunks.
    fn new_session_with_seed(
        config: IngestConfig,
        prior_accounted_bytes: u64,
        prior_chunks: u32,
    ) -> IngestSession {
        IngestSession::new(
            &super::super::gate::GateOk {
                drv_hash: "0cnyg10nhcqdl6ck2dwgmnzh7lcyhkzm".to_string(),
                exec_id: Uuid::now_v7(),
                final_line_count: None,
                prior_accounted_bytes,
                prior_chunks,
            },
            Uuid::now_v7(),
            config,
        )
    }

    /// `n` small, content-identifiable lines starting at line `first`.
    /// Line `i`'s content is `line-{i}` so order and attribution
    /// assertions are meaningful after a decompress.
    fn batch(first: u64, n: usize) -> BuildLogBatch {
        BuildLogBatch {
            derivation_path: String::new(),
            lines: (0..n as u64)
                // wrapping_add: the overflow tests construct batches whose
                // line numbers deliberately exceed u64; the helper must not
                // be the thing that panics.
                .map(|i| format!("line-{}", first.wrapping_add(i)).into_bytes())
                .collect(),
            first_line_number: first,
            executor_id: String::new(),
        }
    }

    /// One batch of `n` lines of exactly `line_len` bytes each, for the
    /// size-trigger and truncation tests.
    fn sized_batch(first: u64, n: usize, line_len: usize) -> BuildLogBatch {
        BuildLogBatch {
            derivation_path: String::new(),
            lines: (0..n).map(|_| vec![b'x'; line_len]).collect(),
            first_line_number: first,
            executor_id: String::new(),
        }
    }

    fn buffered_line_count(session: &IngestSession) -> usize {
        session.shared().lock().unwrap().buffer.len()
    }

    async fn manifest_rows(pool: &PgPool, exec: Uuid) -> Vec<(i64, i64, String)> {
        sqlx::query_as(
            "SELECT first_line, line_count, s3_key FROM drv_log_chunks \
             WHERE exec_id = $1 ORDER BY first_line, chunk_seq",
        )
        .bind(exec)
        .fetch_all(pool)
        .await
        .unwrap()
    }

    /// A [`LogChunkStore`] that fails the first `failures` puts with a
    /// retryable backend error, then delegates to an in-memory store.
    /// Gets and deletes always delegate.
    struct FailNTimesStore {
        inner: MemoryLogChunkStore,
        remaining_failures: AtomicU32,
    }

    impl FailNTimesStore {
        fn new(failures: u32) -> Self {
            Self {
                inner: MemoryLogChunkStore::default(),
                remaining_failures: AtomicU32::new(failures),
            }
        }
    }

    #[async_trait::async_trait]
    impl LogChunkStore for FailNTimesStore {
        async fn put(&self, key: &str, body: Vec<u8>) -> Result<PutOutcome, LogChunkError> {
            // Tests drive cuts serially; a relaxed load/store pair is fine.
            let remaining = self.remaining_failures.load(Ordering::Relaxed);
            if remaining > 0 {
                self.remaining_failures
                    .store(remaining - 1, Ordering::Relaxed);
                return Err(LogChunkError::Backend(anyhow::anyhow!(
                    "injected put failure ({remaining} remaining)"
                )));
            }
            self.inner.put(key, body).await
        }

        async fn get(&self, key: &str) -> Result<Vec<u8>, LogChunkError> {
            self.inner.get(key).await
        }

        async fn delete_batch(&self, keys: &[String]) -> Result<(), LogChunkError> {
            self.inner.delete_batch(keys).await
        }
    }

    // ------------------------------------------------------------------
    // accept(): input gates
    // ------------------------------------------------------------------

    #[test]
    fn rejects_non_monotone_line_numbers() {
        let mut session = new_session(test_config());
        assert!(matches!(
            session.accept(batch(100, 10)).unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        // A batch starting at or below the high-water mark (110) is a
        // protocol violation: dropped, counted, the stream stays open.
        assert!(matches!(
            session.accept(batch(50, 10)).unwrap(),
            AcceptOutcome::RejectedNonMonotone
        ));
        assert_eq!(
            buffered_line_count(&session),
            10,
            "a rejected batch must not be appended"
        );
        // The session is still usable: the next in-order batch is accepted.
        assert!(matches!(
            session.accept(batch(110, 10)).unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        assert_eq!(buffered_line_count(&session), 20);
    }

    #[test]
    fn rejects_line_number_overflow() {
        let mut session = new_session(test_config());
        // first_line_number + lines.len() would overflow u64.
        assert!(matches!(
            session.accept(batch(u64::MAX - 5, 10)).unwrap(),
            AcceptOutcome::RejectedOverflow
        ));
        // A line number that fits u64 but not the manifest's BIGINT is
        // rejected for the same reason (it could not round-trip through
        // `drv_log_chunks.first_line`).
        assert!(matches!(
            session.accept(batch(i64::MAX as u64, 1)).unwrap(),
            AcceptOutcome::RejectedOverflow
        ));
        assert_eq!(buffered_line_count(&session), 0);
        // Still usable.
        assert!(matches!(
            session.accept(batch(0, 1)).unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
    }

    /// store.log.completeness-gate, the per-append comparison: once the
    /// execution's recorded `final_line_count` is known, accepted lines
    /// numbered at or past it are dropped — a batch starting at or past
    /// the ceiling is rejected whole, a batch straddling it keeps only
    /// the lines below it, and lines below it are unaffected. The
    /// stream stays open throughout (per-batch rejection, like the
    /// other input gates).
    // r[verify store.log.completeness-gate]
    #[test]
    fn drops_lines_at_or_past_final_line_count() {
        let mut session = new_session(test_config());
        // The build's recorded end: 8 lines, 0..=7.
        session.set_final_line_count(8);

        // Entirely below the ceiling: accepted untouched.
        assert!(matches!(
            session.accept(batch(0, 5)).unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        assert_eq!(buffered_line_count(&session), 5);

        // Straddles the ceiling: lines 5..=7 kept, 8 and 9 dropped.
        assert!(matches!(
            session.accept(batch(5, 5)).unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        assert_eq!(
            buffered_line_count(&session),
            8,
            "a batch straddling final_line_count must keep only the lines below it"
        );

        // Entirely at/past the ceiling: dropped whole, nothing buffered.
        assert!(matches!(
            session.accept(batch(8, 3)).unwrap(),
            AcceptOutcome::RejectedPastFinal
        ));
        assert_eq!(
            buffered_line_count(&session),
            8,
            "a batch starting at or past final_line_count must not be buffered"
        );

        // The first observed value wins: a later (different) set is a
        // no-op, so the ceiling cannot be moved once learned.
        session.set_final_line_count(100);
        assert!(matches!(
            session.accept(batch(8, 3)).unwrap(),
            AcceptOutcome::RejectedPastFinal
        ));
    }

    // r[verify store.log.write-read-bound+2]
    /// bug_298's admission red: pre-fix, a 4,194,304-empty-line batch
    /// (16 MiB of bare frames, ~134 MiB charged) was `Accepted` and
    /// buffered whole — the recorded red asserted `RejectedOversizedBatch`
    /// and got `Accepted { cut_due: true }`. Post-fix it is rejected at
    /// admission BEFORE any per-line work, the stream stays open (the
    /// next in-order batch is accepted), and nothing was buffered.
    #[test]
    fn oversized_batch_rejected_stream_stays_open() {
        let mut session = new_session(test_config());
        let oversized = BuildLogBatch {
            derivation_path: String::new(),
            // 4M empty lines: Vec::new() is allocation-free, so the
            // fixture itself is cheap — the point is the COUNT.
            lines: vec![Vec::new(); 4_194_304],
            first_line_number: 0,
            executor_id: String::new(),
        };
        assert!(matches!(
            session.accept(oversized).unwrap(),
            AcceptOutcome::RejectedOversizedBatch
        ));
        assert_eq!(buffered_line_count(&session), 0, "nothing buffered");
        // Per-batch semantics: the session is not poisoned and the
        // high-water mark did not move.
        assert!(matches!(
            session.accept(batch(0, 3)).unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        assert_eq!(buffered_line_count(&session), 3);
    }

    #[test]
    fn enforces_per_exec_byte_cap() {
        let mut session = new_session(IngestConfig {
            per_exec_byte_cap: 1000,
            ..test_config()
        });
        // 600 bytes: under the cap.
        assert!(matches!(
            session.accept(sized_batch(0, 6, 100)).unwrap(),
            AcceptOutcome::Accepted { .. }
        ));
        // 600 more would exceed 1000: the stream aborts with the
        // permanent cap class (the cap travels with the execution; a
        // retry elsewhere cannot succeed).
        let err = session.accept(sized_batch(6, 6, 100)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        assert_eq!(
            err.metadata()
                .get(rio_proto::LOG_REJECT_METADATA_KEY)
                .unwrap(),
            "cap"
        );
    }

    /// `store.log.caps-durable`: a session born from a GateOk carrying
    /// a durable seed resumes the byte account where the previous
    /// session left it — the cap cannot be reset by reconnecting.
    // r[verify store.log.caps-durable]
    #[test]
    fn byte_cap_resumes_from_durable_seed() {
        let mut session = new_session_with_seed(
            IngestConfig {
                per_exec_byte_cap: 1000,
                ..test_config()
            },
            // 600 accounted bytes already durable from prior sessions.
            600,
            1,
        );
        // 600 more would exceed 1000 even though THIS session has
        // accepted nothing yet.
        let err = session.accept(sized_batch(0, 6, 100)).unwrap_err();
        assert_eq!(err.code(), tonic::Code::FailedPrecondition);
        // And the chunk-attempt account includes the durable chunk.
        assert_eq!(session.chunk_attempts(), 1);
    }

    // ------------------------------------------------------------------
    // cut(): the durable path
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn cuts_on_byte_threshold() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let mut session = new_session(test_config());
        let exec = session.exec_id;

        // 10 lines x (50 content + 32 overhead) = 820 accounted bytes:
        // under the 1 KiB test threshold.
        match session.accept(sized_batch(0, 10, 50)).unwrap() {
            AcceptOutcome::Accepted { cut_due } => {
                assert!(!cut_due, "820 accounted B < 1 KiB threshold");
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
        // 20 more lines x 82 = 1640: crosses the threshold.
        match session.accept(sized_batch(10, 20, 50)).unwrap() {
            AcceptOutcome::Accepted { cut_due } => {
                assert!(cut_due, "2460 accounted B >= 1 KiB threshold");
            }
            other => panic!("expected Accepted, got {other:?}"),
        }

        let acked = session.cut(&store, &db.pool).await.unwrap();
        assert_eq!(acked, Some(29), "30 lines 0..=29 were drained");
        assert_eq!(store.len(), 1, "exactly one chunk object");
        assert_eq!(
            manifest_rows(&db.pool, exec).await.len(),
            1,
            "exactly one manifest row"
        );
        assert_eq!(
            buffered_line_count(&session),
            0,
            "the cut drained everything buffered at cut time"
        );

        // Lines accepted after the cut wait for the next cut.
        session.accept(batch(30, 5)).unwrap();
        assert_eq!(buffered_line_count(&session), 5);
    }

    // r[verify store.log.write-read-bound+2]
    /// bug_098 (red-first): a contiguous run whose framed payload
    /// exceeds the read path's bound must split across multiple chunks,
    /// each decodable. The recorded red: the unbounded cutter drained
    /// the whole ~17 MiB run into ONE chunk whose framed payload the
    /// read path refuses — `decompress_lines` failed with "chunk
    /// decompresses past the 16777216-byte bound" on a chunk the write
    /// path had just committed.
    #[tokio::test]
    async fn cut_splits_run_at_payload_bound() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let mut session = new_session(IngestConfig {
            per_exec_byte_cap: 64 * 1024 * 1024,
            ..test_config()
        });

        // 290 contiguous lines x 60 000 B ≈ 17.4 MiB content: past the
        // 16 MiB shared write/read bound (lines stay under the 64 KiB
        // line cap, so nothing here is truncated).
        for i in 0..29u64 {
            session.accept(sized_batch(i * 10, 10, 60_000)).unwrap();
        }
        let mut cuts = 0;
        while session.cut(&store, &db.pool).await.unwrap().is_some() {
            cuts += 1;
            assert!(cuts < 100, "the cut loop must terminate");
        }

        assert!(
            store.len() >= 2,
            "an over-bound run must split into multiple chunks, got {}",
            store.len()
        );
        for key in store.keys() {
            let blob = store.get(&key).await.expect("stored chunk fetch");
            let lines = decompress_lines(&blob).expect(
                "every committed chunk must round-trip within the read \
                 bound — the write path may never commit an unreadable \
                 chunk",
            );
            assert!(!lines.is_empty());
        }
        let rows = manifest_rows(&db.pool, session.exec_id).await;
        let total: i64 = rows.iter().map(|(_, n, _)| n).sum();
        assert_eq!(
            total, 290,
            "every accepted line is durable across the split"
        );
    }

    #[tokio::test]
    async fn cut_on_empty_buffer_is_a_noop() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let mut session = new_session(test_config());

        assert_eq!(session.cut(&store, &db.pool).await.unwrap(), None);
        assert!(store.is_empty(), "an empty cut writes nothing");
        assert!(manifest_rows(&db.pool, session.exec_id).await.is_empty());

        // The no-op did not burn a seq: the first real chunk is 00000000.
        session.accept(batch(0, 10)).unwrap();
        session.cut(&store, &db.pool).await.unwrap();
        let keys = store.keys();
        assert_eq!(keys.len(), 1);
        assert!(
            keys[0].ends_with("/00000000.zst"),
            "empty cuts must not consume seq numbers, got {}",
            keys[0]
        );
    }

    #[tokio::test]
    async fn ack_carries_durable_through_line() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let mut session = new_session(test_config());

        session.accept(batch(0, 100)).unwrap();
        assert_eq!(
            session.cut(&store, &db.pool).await.unwrap(),
            Some(99),
            "the ack is the last drained line number"
        );
    }

    // r[verify store.log.ingest-bounds]
    #[tokio::test]
    async fn truncates_oversized_lines() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        // The truncation test needs a per-exec cap above MAX_LINE_LEN.
        let mut session = new_session(IngestConfig {
            per_exec_byte_cap: 10 * 1024 * 1024,
            ..test_config()
        });

        let oversized = vec![b'y'; MAX_LINE_LEN + 1024];
        session
            .accept(BuildLogBatch {
                derivation_path: String::new(),
                lines: vec![oversized.clone()],
                first_line_number: 0,
                executor_id: String::new(),
            })
            .unwrap();
        session.cut(&store, &db.pool).await.unwrap();

        let keys = store.keys();
        let lines = decompress_lines(&store.get(&keys[0]).await.unwrap()).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(
            lines[0].len(),
            MAX_LINE_LEN,
            "an oversized line is stored truncated to exactly MAX_LINE_LEN"
        );
        assert_eq!(&lines[0][..], &oversized[..MAX_LINE_LEN]);
    }

    #[tokio::test]
    async fn cut_splits_at_forward_gaps() {
        // Forward gaps between batches are part of the protocol contract
        // (suppressed/undelivered lines). A chunk's manifest row
        // describes a CONTIGUOUS [first_line, first_line + line_count)
        // and the read path attributes line numbers as first_line +
        // index, so a single chunk must never span a gap: the cut drains
        // only the longest contiguous prefix and leaves the rest for the
        // next cut.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = MemoryLogChunkStore::default();
        let mut session = new_session(test_config());
        let exec = session.exec_id;

        session.accept(batch(0, 100)).unwrap();
        // A forward gap: lines 100..150 were suppressed.
        session.accept(batch(150, 50)).unwrap();

        assert_eq!(
            session.cut(&store, &db.pool).await.unwrap(),
            Some(99),
            "the first cut drains only the contiguous prefix [0, 100)"
        );
        assert_eq!(
            buffered_line_count(&session),
            50,
            "the post-gap lines stay buffered"
        );
        assert_eq!(
            session.cut(&store, &db.pool).await.unwrap(),
            Some(199),
            "the second cut drains the post-gap run [150, 200)"
        );

        let rows = manifest_rows(&db.pool, exec).await;
        assert_eq!(
            rows.iter().map(|(f, c, _)| (*f, *c)).collect::<Vec<_>>(),
            vec![(0, 100), (150, 50)],
            "two manifest rows, each describing a contiguous run; the gap is visible to the completeness fold"
        );
        assert_eq!(store.len(), 2);
    }

    // ------------------------------------------------------------------
    // cut(): failure handling
    // ------------------------------------------------------------------

    #[tokio::test]
    async fn aborts_after_three_failed_cuts() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = FailNTimesStore::new(u32::MAX);
        let mut session = new_session(test_config());

        session.accept(batch(0, 100)).unwrap();

        for attempt in 1..=3u8 {
            assert!(
                session.cut(&store, &db.pool).await.is_err(),
                "attempt {attempt} must fail"
            );
            if attempt < 3 {
                assert!(
                    session.should_abort().is_none(),
                    "no abort before the third consecutive failure"
                );
            }
        }
        assert!(
            matches!(
                session.should_abort(),
                Some(AbortReason::ConsecutiveCutFailures)
            ),
            "three consecutive failed cuts must abort the stream"
        );
        assert_eq!(
            buffered_line_count(&session),
            100,
            "every accepted line is still buffered: nothing was lost"
        );
        assert!(store.inner.is_empty());
    }

    // r[verify store.log.chunk-immutable]
    #[tokio::test]
    async fn failed_cut_burns_the_seq() {
        // The PutOutcome::Existed caller contract: a seq number is never
        // re-PUT with a different payload, because a retried cut (whose
        // buffer may have grown) gets a fresh seq. The failed attempt's
        // seq is burned forever.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = FailNTimesStore::new(1);
        let mut session = new_session(test_config());

        session.accept(batch(0, 100)).unwrap();
        assert!(session.cut(&store, &db.pool).await.is_err());
        assert_eq!(session.cut(&store, &db.pool).await.unwrap(), Some(99));

        let keys = store.inner.keys();
        assert_eq!(keys.len(), 1, "exactly one object was committed");
        assert!(
            keys[0].ends_with("/00000001.zst"),
            "the retry uses the NEXT seq, not the failed attempt's: {}",
            keys[0]
        );
        assert!(
            !keys.iter().any(|k| k.ends_with("/00000000.zst")),
            "the failed attempt's seq is burned: no object exists under it"
        );
    }

    #[tokio::test]
    async fn failed_cut_restores_lines_in_order() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = FailNTimesStore::new(1);
        let mut session = new_session(test_config());
        let exec = session.exec_id;

        session.accept(batch(0, 100)).unwrap();
        assert!(session.cut(&store, &db.pool).await.is_err());
        // More lines arrive while the failed cut's lines sit restored at
        // the front of the buffer.
        session.accept(batch(100, 50)).unwrap();
        assert_eq!(session.cut(&store, &db.pool).await.unwrap(), Some(149));

        let keys = store.inner.keys();
        assert_eq!(keys.len(), 1);
        let lines = decompress_lines(&store.inner.get(&keys[0]).await.unwrap()).unwrap();
        assert_eq!(lines.len(), 150, "restored + new lines in one chunk");
        assert_eq!(lines[0], b"line-0".to_vec());
        assert_eq!(lines[99], b"line-99".to_vec());
        assert_eq!(lines[100], b"line-100".to_vec());
        assert_eq!(lines[149], b"line-149".to_vec());
        assert_eq!(
            manifest_rows(&db.pool, exec).await,
            vec![(0, 150, keys[0].clone())]
        );
    }

    // ------------------------------------------------------------------
    // Subscriber fan-out
    // ------------------------------------------------------------------

    #[test]
    fn subscriber_receives_batches_in_order() {
        let mut session = new_session(test_config());

        // Lines accepted before the subscriber arrives are in its
        // snapshot, not its channel.
        session.accept(batch(0, 2)).unwrap();
        let (tx, mut rx) = mpsc::channel(16);
        let snapshot = session.shared().lock().unwrap().subscribe(tx, 0);
        assert_eq!(
            snapshot.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            vec![0, 1],
            "subscribe() snapshots the already-buffered lines atomically with registration"
        );

        session.accept(batch(2, 3)).unwrap();
        session.accept(batch(5, 1)).unwrap();

        let first = rx.try_recv().expect("first post-subscribe batch");
        assert_eq!(first.batch.first_line_number, 2);
        assert_eq!(first.batch.lines.len(), 3);
        let second = rx.try_recv().expect("second post-subscribe batch");
        assert_eq!(second.batch.first_line_number, 5);
        assert!(rx.try_recv().is_err(), "no further batches");
    }

    #[test]
    fn slow_subscriber_is_dropped_not_blocking() {
        let mut session = new_session(test_config());
        let (tx, mut rx) = mpsc::channel(1);
        session.shared().lock().unwrap().subscribe(tx, 0);

        for i in 0..10u64 {
            assert!(
                matches!(
                    session.accept(batch(i, 1)).unwrap(),
                    AcceptOutcome::Accepted { .. }
                ),
                "a slow subscriber must never cause a batch to be rejected"
            );
        }

        assert_eq!(
            buffered_line_count(&session),
            10,
            "every batch was accepted and buffered regardless of the subscriber"
        );
        // The capacity-1 channel got exactly one batch; the other 9 were
        // dropped for this subscriber only.
        assert_eq!(rx.try_recv().unwrap().batch.first_line_number, 0);
        assert!(rx.try_recv().is_err());
        assert_eq!(
            session.shared().lock().unwrap().tail_dropped,
            9,
            "dropped fan-out batches are counted"
        );
    }

    // ------------------------------------------------------------------
    // The seam: in-flight visibility and resource accounting
    // ------------------------------------------------------------------

    /// A [`LogChunkStore`] whose `put` signals the test that it has been
    /// entered, then parks until the test releases it. Lets a test hold
    /// a cut in the "drained but not yet committed" state and observe
    /// what a concurrently-registering subscriber sees.
    struct BlockingPutStore {
        inner: MemoryLogChunkStore,
        /// `put` adds a permit on entry; the test acquires it to learn
        /// the drain has happened and the commit is in flight.
        entered: tokio::sync::Semaphore,
        /// The test adds a permit to let `put` proceed. Semaphores
        /// rather than oneshots: permits added before the waiter
        /// arrives are not lost.
        release: tokio::sync::Semaphore,
    }

    impl BlockingPutStore {
        fn new() -> Self {
            Self {
                inner: MemoryLogChunkStore::default(),
                entered: tokio::sync::Semaphore::new(0),
                release: tokio::sync::Semaphore::new(0),
            }
        }
    }

    #[async_trait::async_trait]
    impl LogChunkStore for BlockingPutStore {
        async fn put(&self, key: &str, body: Vec<u8>) -> Result<PutOutcome, LogChunkError> {
            self.entered.add_permits(1);
            self.release
                .acquire()
                .await
                .expect("release semaphore closed")
                .forget();
            self.inner.put(key, body).await
        }

        async fn get(&self, key: &str) -> Result<Vec<u8>, LogChunkError> {
            self.inner.get(key).await
        }

        async fn delete_batch(&self, keys: &[String]) -> Result<(), LogChunkError> {
            self.inner.delete_batch(keys).await
        }
    }

    #[tokio::test]
    async fn subscribe_during_in_flight_cut_sees_drained_lines() {
        // The seam invariant: a line is always in the buffer, in the
        // in-flight staging area, or in a manifest-visible chunk. A
        // subscriber registering while a cut is mid-commit must see the
        // drained lines in its snapshot — if it did not, and the reader
        // went on to receive later lines, its `since_line = last + 1`
        // reconnect cursor would be past the gap and the missing lines
        // would be unrecoverable forever.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = std::sync::Arc::new(BlockingPutStore::new());
        let mut session = new_session(test_config());
        let shared = session.shared().clone();

        session.accept(batch(0, 100)).unwrap();

        // Drive the cut on its own task so the test body can interleave
        // with the parked PUT.
        let cut_store = store.clone();
        let cut_pool = db.pool.clone();
        let cut = tokio::spawn(async move {
            let acked = session.cut(cut_store.as_ref(), &cut_pool).await;
            (session, acked)
        });

        // Once put() has been entered, the drain has happened: the lines
        // are out of `buffer` and staged in `in_flight`.
        store.entered.acquire().await.unwrap().forget();
        {
            let mut guard = shared.lock().unwrap();
            assert!(
                guard.buffer.is_empty(),
                "precondition: the drain has moved the lines out of the buffer"
            );
            let (tx, _rx) = mpsc::channel(16);
            let snapshot = guard.subscribe(tx, 0);
            assert_eq!(
                snapshot.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
                (0..100).collect::<Vec<u64>>(),
                "a subscriber registering mid-commit must see the in-flight lines in its snapshot"
            );
        }

        // Let the PUT complete and the cut succeed.
        store.release.add_permits(1);
        let (session, acked) = cut.await.unwrap();
        assert_eq!(acked.unwrap(), Some(99));
        assert!(
            session.shared().lock().unwrap().in_flight.is_empty(),
            "a committed cut clears the staging area"
        );
    }

    #[tokio::test]
    async fn failed_cut_keeps_lines_visible_to_mid_commit_subscriber() {
        // The other half of the seam: a subscriber that registered while
        // the (about-to-fail) cut was in flight saw the lines via
        // in_flight; after the failure folds them back into the buffer
        // they must still be there for the NEXT subscriber. No
        // interleaving needed — this just pins that the failure restore
        // feeds subscribe() too.
        let db = TestDb::new(&crate::MIGRATOR).await;
        let store = FailNTimesStore::new(1);
        let mut session = new_session(test_config());

        session.accept(batch(0, 10)).unwrap();
        assert!(session.cut(&store, &db.pool).await.is_err());

        let (tx, _rx) = mpsc::channel(16);
        let snapshot = session.shared().lock().unwrap().subscribe(tx, 0);
        assert_eq!(
            snapshot.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
            (0..10).collect::<Vec<u64>>(),
            "after a failed cut the restored lines are back in the snapshot"
        );
    }

    #[test]
    fn cut_trigger_accounts_for_per_line_overhead() {
        // A stream of 1-byte lines costs ~33 bytes of memory per line
        // (the content byte + the line-number key + the Vec header).
        // The cut trigger and the lifetime cap charge that overhead so
        // a tiny-lines stream cannot hold ~33x its accounted bytes
        // resident: at the 1 KiB test threshold the trigger fires after
        // ceil(1024 / 33) = 32 one-byte lines, not after 1024 of them.
        let mut session = new_session(test_config());

        // 31 lines x (1 + 32) = 1023 accounted bytes: one short.
        match session.accept(sized_batch(0, 31, 1)).unwrap() {
            AcceptOutcome::Accepted { cut_due } => {
                assert!(!cut_due, "31 one-byte lines = 1023 accounted B < 1024");
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
        // One more line crosses it. 32 bytes of CONTENT against a
        // 1024-byte threshold proves the overhead is what is being
        // counted.
        match session.accept(sized_batch(31, 1, 1)).unwrap() {
            AcceptOutcome::Accepted { cut_due } => {
                assert!(
                    cut_due,
                    "32 one-byte lines = 1056 accounted B >= 1024, despite only 32 content bytes"
                );
            }
            other => panic!("expected Accepted, got {other:?}"),
        }
    }

    /// merged_bug_119 (spurious-fire half, red-first): a healthy paced
    /// multi-run drain — old pending lines, ZERO cut failures — must
    /// NOT trip the StaleBuffer abort. Staleness is a backstop
    /// conditioned on at least one observed failure.
    #[tokio::test]
    async fn paced_multirun_drain_never_trips_stale_buffer() {
        let mut session = new_session(IngestConfig {
            per_exec_byte_cap: 1024 * 1024,
            cut_threshold_bytes: 1024,
            cut_interval: Duration::from_millis(10),
        });
        session.accept(batch(0, 5)).unwrap();
        // Real-time wait past 2x the (tiny) cut interval: the buffer is
        // now "stale" by age, with zero failures — a paced multi-run
        // drain mid-progress.
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(
            session.should_abort(),
            None,
            "pure staleness with zero cut failures is a healthy drain, not a wedge"
        );
        // With an observed failure the same staleness DOES abort
        // (the backstop still exists for wedges).
        session.note_cut_abandoned();
        assert_eq!(session.should_abort(), Some(AbortReason::StaleBuffer));
    }

    /// merged_bug_119 (parked-cut half): a cut whose PUT hangs forever
    /// is abandoned at the watchdog bound and counted like a failure;
    /// three abandonments trip ConsecutiveCutFailures, and the staged
    /// run folds back so nothing is lost. (The do_cut watchdog wires
    /// timeout(cut_interval) + note_cut_abandoned; this exercises the
    /// composition against a parked store. Pre-fix RED: the cut future
    /// pends forever — the timeout wrapper did not exist and
    /// note_cut_abandoned did not exist, so a wedged replica produced
    /// zero countable failures.)
    #[tokio::test]
    async fn parked_cut_does_not_starve_abort() {
        struct BlockingPutStore;
        #[async_trait::async_trait]
        impl LogChunkStore for BlockingPutStore {
            async fn put(&self, _key: &str, _body: Vec<u8>) -> Result<PutOutcome, LogChunkError> {
                std::future::pending::<()>().await;
                unreachable!("the parked PUT never answers")
            }
            async fn get(&self, key: &str) -> Result<Vec<u8>, LogChunkError> {
                Err(LogChunkError::NotFound {
                    key: key.to_string(),
                })
            }
            async fn delete_batch(&self, _keys: &[String]) -> Result<(), LogChunkError> {
                Ok(())
            }
        }

        let db = TestDb::new(&crate::MIGRATOR).await;
        let mut session = new_session(IngestConfig {
            per_exec_byte_cap: 1024 * 1024,
            cut_threshold_bytes: 1024,
            cut_interval: Duration::from_millis(20),
        });
        session.accept(batch(0, 5)).unwrap();
        let store = BlockingPutStore;

        for round in 1..=3u8 {
            let bound = session.config().cut_interval;
            let outcome = tokio::time::timeout(bound, session.cut(&store, &db.pool)).await;
            assert!(
                outcome.is_err(),
                "the parked cut must hit the watchdog bound (round {round})"
            );
            session.note_cut_abandoned();
        }
        assert_eq!(
            session.should_abort(),
            Some(AbortReason::ConsecutiveCutFailures),
            "three abandoned cuts trip the failover abort"
        );
        // Nothing was lost: the next cut entry folds the staged run
        // back (restore_in_flight) — all 5 lines are still pending.
        {
            let mut shared = session.shared().lock().unwrap();
            shared.restore_in_flight();
            assert_eq!(shared.buffer.len(), 5, "the abandoned run folded back");
        }
    }
}
