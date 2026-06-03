//! Build log streaming via gRPC.
//!
//! Buffers captured build-output lines and batches them into
//! `BuildLogBatch` messages (64 lines or 100ms, whichever comes first;
//! a batch may additionally carry up to `MARKER_HEADROOM` out-of-band
//! marker lines). Batches are sent on the scheduler `BuildExecution`
//! stream.
//!
//! Also enforces per-build log limits. `total_bytes` is a hard cap:
//! exceeding it returns [`AddLineResult::LimitExceeded`] and the caller
//! aborts the build with `BuildStatus::LogLimitExceeded`. `rate_lines_per_sec`
//! is a suppression threshold: excess lines in a 1s window are DROPPED
//! (not failed), and a single `[rio: N lines suppressed …]` marker is
//! injected at the next window reset. The size limit bounds infrastructure
//! cost; the rate limit bounds per-tick scheduler load without killing
//! legitimate bursty builds (kernel `make oldconfig` emits ~18k prompts in
//! one burst).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use rio_proto::types::{BuildLogBatch, BuildPhase, ExecutorMessage, executor_message};
use tokio::sync::mpsc;

/// Maximum lines per batch.
const MAX_BATCH_LINES: usize = 64;

/// Out-of-band marker headroom per emitted batch, on top of
/// [`MAX_BATCH_LINES`]: one rate-suppression marker plus one relay-shed
/// marker. An emitted batch holds at most `MAX_BATCH_LINES +
/// MARKER_HEADROOM` lines (`obs.log.batch-64-100ms+1` sanctions the
/// overshoot — markers are never deferred to a later batch, because
/// deferral detaches the count from the batch whose assembly drained
/// it, and never trigger an early flush).
///
/// Why the bound holds (enforced at [`LogBatcher::push_marker`], the
/// sole marker-insertion chokepoint): `add_line` flushes at
/// `MAX_BATCH_LINES`, so the buffer holds ≤ 63 lines at `add_line`
/// entry; a window-reset marker brings it to ≤ 64, the incoming line
/// to ≤ 65, which trips the flush; `flush` adds at most one shed
/// marker on top (the tally is drained exactly once per assembled
/// batch) → ≤ 66 lines emitted.
const MARKER_HEADROOM: usize = 2;

/// The rate-suppression marker line, shared by `add_line`'s
/// window-reset and [`LogBatcher::finish`]'s terminal drain so the two
/// sites cannot drift apart.
fn rate_suppression_marker(dropped: u64, rate_lines_per_sec: u64) -> String {
    format!("[rio: {dropped} lines suppressed by log_rate_limit ({rate_lines_per_sec} lines/s)]")
}

/// Maximum time to wait before flushing a partial batch.
pub(crate) const BATCH_TIMEOUT: Duration = Duration::from_millis(100);

/// Per-build log limits, enforced by [`LogBatcher`].
///
/// Both limits are **soft-off at 0** (unlimited).
#[derive(Debug, Clone, Copy)]
pub struct LogLimits {
    /// Max log lines per second before suppression kicks in. 0 = unlimited.
    ///
    /// Enforced via a 1-second tumbling window. Lines beyond the
    /// threshold within a window are dropped; a marker line is
    /// injected at the next window reset showing the drop count.
    /// `total_bytes` is the hard cap; this only bounds per-second
    /// scheduler-stream load.
    pub rate_lines_per_sec: u64,
    /// Max total log bytes across the whole build. 0 = unlimited.
    /// Exceeding this aborts the build (`BuildStatus::LogLimitExceeded`).
    pub total_bytes: u64,
}

impl LogLimits {
    /// No limits. For tests where log limiting isn't the subject under test.
    pub const UNLIMITED: Self = Self {
        rate_lines_per_sec: 0,
        total_bytes: 0,
    };
}

/// Result of [`LogBatcher::add_line`].
#[derive(Debug)]
pub enum AddLineResult {
    /// Line accepted (buffered, or dropped by rate suppression — either
    /// way the caller continues). Batch not yet full.
    Buffered,
    /// Line completed a batch. Caller must send it.
    BatchReady(AssembledBatch),
    /// `total_bytes` limit tripped. Caller must abort the build with
    /// `BuildStatus::LogLimitExceeded`; the typed trip's figures land
    /// in `error_msg` via exit classification.
    ///
    /// The line that tripped the limit is **not** buffered (we're done
    /// accepting lines). Any already-buffered lines are still in the
    /// batcher — caller should `flush()` them before aborting so the
    /// client sees output right up to the limit.
    LimitExceeded { trip: LogCapTrip },
}

/// WHICH log cap tripped, with the per-attempt figures the verdict
/// message carries (round-17 merged_bug_058 c2: the trip used to be a
/// pre-formatted string, so exit classification could only say "build
/// exceeded its log size limit" with no figures and no axis).
///
/// Oracle parity (PARITY-SWEEP in the introducing commit): CppNix
/// 2.34.7 `derivation-building-goal.cc:656/:948` checks cumulative
/// `logSize > maxLogSize` and reports only the LIMIT figure
/// (`:1230-1237` "killed after writing more than %d bytes"); rio
/// reports both sides of the comparison. The bytes check here is
/// PROSPECTIVE (the line that would cross is rejected) where the
/// oracle counts it first — registered nuance, same terminal verdict.
/// The line cap has no oracle counterpart (rio-specific, covered by
/// the spec rule's line-cap clause).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogCapTrip {
    /// `total_bytes` would be exceeded: the prospective cumulative
    /// size against the configured limit.
    Bytes { would_be: u64, limit: u64 },
    /// The accepted-line cap tripped: lines seen against the cap.
    Lines { seen: u64, cap: u64 },
}

impl std::fmt::Display for LogCapTrip {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LogCapTrip::Bytes { would_be, limit } => write!(
                f,
                "log_size_limit exceeded: {would_be} bytes > {limit} limit"
            ),
            LogCapTrip::Lines { seen, cap } => {
                write!(f, "log line cap exceeded: {seen} lines > {cap} cap")
            }
        }
    }
}

/// Log batcher that collects log lines, emits `BuildLogBatch` messages,
/// and enforces per-build log limits.
pub struct LogBatcher {
    /// Derivation path this batcher is collecting logs for.
    drv_path: String,
    /// Worker ID for the batch messages.
    executor_id: String,
    /// Accumulated log lines (raw bytes, may be non-UTF-8).
    lines: Vec<Vec<u8>>,
    /// Line number counter.
    next_line_number: u64,
    /// When the current batch started accumulating.
    batch_start: Instant,

    // --- limits ---
    limits: LogLimits,
    /// Lines accepted in the current rate window.
    lines_this_window: u64,
    /// Lines dropped (rate-suppressed) in the current rate window.
    pub(crate) lines_dropped_this_window: u64,
    /// Start of the current rate window.
    pub(crate) window_start: Instant,
    /// Total bytes across all lines ever added (including flushed batches).
    pub(crate) total_bytes: u64,
    /// Relay-shed tally shared with the build's [`SheddingLogSender`]
    /// (attached by the log loop). Drained inside [`flush`](Self::flush),
    /// so EVERY assembled batch carries the suppression marker for
    /// display messages shed since the last delivery — the
    /// `builder.relay.log-shed` "next delivered batch MUST carry a
    /// single suppression marker" clause holds at every call site by
    /// construction instead of by per-caller discipline. The drain is
    /// provisional: it travels as the [`AssembledBatch`] shed lease and
    /// is restored by the sender if the carrier batch itself fails to
    /// deliver. `None` only for batchers tested in isolation.
    relay_shed: Option<Arc<AtomicU64>>,
}

impl LogBatcher {
    /// Derivation path this batcher is collecting logs for. Exposed so
    /// the stderr loop can tag `BuildPhase` messages without threading
    /// `drv_path` separately through `read_build_stderr_loop`.
    pub fn drv_path(&self) -> &str {
        &self.drv_path
    }

    /// Create a new log batcher for the given derivation.
    ///
    /// `initial_line` seeds the line counter. The executor sends a
    /// `rio:` banner header (`crate::banner::HEADER_LINE_COUNT` lines)
    /// directly on `log_tx` *before* `run_native_lifecycle` constructs
    /// the batcher; seeding the counter lets the build's real output
    /// start numbering after the header instead of colliding at line 0.
    /// On an infra-transient retry the executor seeds at the prior
    /// attempt's `final_line_count` so output numbering continues
    /// monotonically across attempts. Tests that exercise the batcher
    /// in isolation pass `0`.
    pub fn new(
        drv_path: String,
        executor_id: String,
        limits: LogLimits,
        initial_line: u64,
    ) -> Self {
        let now = Instant::now();
        Self {
            drv_path,
            executor_id,
            lines: Vec::with_capacity(MAX_BATCH_LINES),
            next_line_number: initial_line,
            batch_start: now,
            limits,
            lines_this_window: 0,
            lines_dropped_this_window: 0,
            window_start: now,
            total_bytes: 0,
            relay_shed: None,
        }
    }

    /// Share the relay-shed tally with this batcher so
    /// [`flush`](Self::flush) injects the suppression marker for shed
    /// display messages. Called once by the log loop at entry.
    pub(crate) fn attach_relay_shed(&mut self, sender: &SheddingLogSender) {
        self.relay_shed = Some(Arc::clone(&sender.shed));
    }

    /// Lines accounted for so far: `initial_line` + every line ever
    /// flushed (including suppression markers, excluding currently
    /// buffered lines). Mid-build observation only — the terminal
    /// count is obtainable solely by consuming the batcher through
    /// [`finish`](Self::finish).
    #[cfg(test)]
    pub(crate) fn line_count(&self) -> u64 {
        self.next_line_number
    }

    /// Add a log line. Returns the batch if it's full, or a limit-exceeded
    /// signal if a limit tripped.
    ///
    /// Limit checks happen BEFORE buffering — a line that would exceed the
    /// size limit is rejected, not half-accepted.
    // r[impl builder.log-limit+4]
    pub fn add_line(&mut self, line: Vec<u8>) -> AddLineResult {
        // --- Rate limit (suppression, not abort) ---
        // Runs BEFORE the size check: a rate-dropped line is never
        // transmitted, so it has zero infrastructure cost and must not
        // count toward `total_bytes` (`r[builder.log-limit+4]`). Tumbling
        // window: if ≥ 1s has elapsed since window_start, reset. Instant
        // (monotonic) so NTP jumps don't spuriously trip/un-trip.
        if self.limits.rate_lines_per_sec > 0 {
            if self.window_start.elapsed() >= Duration::from_secs(1) {
                let dropped = std::mem::take(&mut self.lines_dropped_this_window);
                self.window_start = Instant::now();
                self.lines_this_window = 0;
                if dropped > 0 {
                    metrics::counter!("rio_builder_log_lines_suppressed_total").increment(dropped);
                    // push_marker (the sole marker mechanic) lands the
                    // marker ahead of `line`, never recurses through
                    // add_line, counts it toward `total_bytes`, and —
                    // when the buffer is empty — initializes
                    // `batch_start`, so a marker opening a batch waits
                    // out the full 100ms window instead of inheriting a
                    // stale `batch_start` and tick-flushing immediately.
                    // The marker does NOT count toward
                    // `lines_this_window` — at any R≥1 the window
                    // delivers R real lines + ≤1 marker, and `rate=1`
                    // doesn't degenerate into a marker-only loop.
                    let marker = rate_suppression_marker(dropped, self.limits.rate_lines_per_sec);
                    self.push_marker(marker);
                }
            }
            if self.lines_this_window >= self.limits.rate_lines_per_sec {
                self.lines_dropped_this_window += 1;
                return AddLineResult::Buffered;
            }
            self.lines_this_window += 1;
        }

        // --- Size limit ---
        // Check the PROSPECTIVE total, not the current one. A 100 MiB limit
        // with 99.9 MiB accumulated and a 1 MiB line coming in should reject
        // that line, not accept it and trip on the NEXT one (which would put
        // us at 100.9 MiB — over the limit we're supposed to enforce). If
        // rate-limiting accepted this line above, `lines_this_window` is
        // already incremented; harmless because `LimitExceeded` aborts the
        // build (batcher state is dead).
        if self.limits.total_bytes > 0 {
            let prospective = self.total_bytes.saturating_add(line.len() as u64);
            if prospective > self.limits.total_bytes {
                return AddLineResult::LimitExceeded {
                    trip: LogCapTrip::Bytes {
                        would_be: prospective,
                        limit: self.limits.total_bytes,
                    },
                };
            }
        }

        // --- Accept the line ---
        if self.lines.is_empty() {
            self.batch_start = Instant::now();
        }
        self.total_bytes += line.len() as u64;
        self.lines.push(line);

        if self.lines.len() >= MAX_BATCH_LINES {
            AddLineResult::BatchReady(self.flush())
        } else {
            AddLineResult::Buffered
        }
    }

    /// Check if the batch timeout has elapsed and flush if so.
    pub fn maybe_flush(&mut self) -> Option<AssembledBatch> {
        if !self.lines.is_empty() && self.batch_start.elapsed() >= BATCH_TIMEOUT {
            Some(self.flush())
        } else {
            None
        }
    }

    /// Flush buffered lines as a batch.
    ///
    /// Drains the relay-shed tally first (when attached): the assembled
    /// batch carries one `[rio: N log messages shed …]` marker covering
    /// every display message shed since the last flush, so the
    /// `builder.relay.log-shed` marker clause is true wherever a batch
    /// is assembled — `BatchReady`, the periodic `flush_tick`, the
    /// phase-boundary flush, and [`finish`](Self::finish) — instead of
    /// depending on each delivery site remembering to inject it.
    ///
    /// The drain is a destructive read of shared evidence UPSTREAM of
    /// the authoritative event the spec's marker clause names (the next
    /// **delivered** batch), so the drained count travels WITH the
    /// batch as `AssembledBatch::shed_lease` and is settled by
    /// [`SheddingLogSender::try_send_batch`]: sink-accepted → consumed;
    /// shed/closed → restored to the tally so the count rides the next
    /// delivered batch's marker instead of dying with its carrier.
    ///
    /// Does NOT drain `lines_dropped_this_window` — drops belong to the
    /// rate window, not the batch. Draining here would let the 100ms
    /// `flush_tick` emit a fragmentary marker mid-window whenever some
    /// accepted lines are still buffered alongside drops (any
    /// `rate % MAX_BATCH_LINES != 0`), violating the spec's "single
    /// marker at window reset" (`r[builder.log-limit+4]`). Only
    /// `add_line`'s window-reset and [`finish`](Self::finish) drain
    /// drops.
    // r[impl builder.relay.log-shed]
    pub fn flush(&mut self) -> AssembledBatch {
        let shed_lease = match &self.relay_shed {
            Some(shed) => shed.swap(0, Ordering::Relaxed),
            None => 0,
        };
        if shed_lease > 0 {
            self.push_marker(format!(
                "[rio: {shed_lease} log messages shed (scheduler link backpressure)]"
            ));
        }

        let first_line_number = self.next_line_number;
        self.next_line_number += self.lines.len() as u64;

        let lines = std::mem::take(&mut self.lines);

        AssembledBatch {
            batch: BuildLogBatch {
                derivation_path: self.drv_path.clone(),
                lines,
                first_line_number,
                executor_id: self.executor_id.clone(),
            },
            shed_lease,
        }
    }

    /// Consume the batcher: drain the final suppression window and the
    /// relay-shed tally, and return the terminal batch (if anything
    /// resulted) plus the final line count.
    ///
    /// This is the ONLY source of the terminal line count — exit paths
    /// cannot skip the terminal drain, because the count they need is
    /// behind the consumption. A build whose final burst exceeds the
    /// rate and then exits within the same 1s window never triggers
    /// `add_line`'s window-reset; without the unconditional drain here
    /// the marker + metric would be lost (silent truncation,
    /// undercounted `rio_builder_log_lines_suppressed_total`) — and a
    /// terminal drain gated on buffered lines loses them exactly when
    /// the buffer happens to be empty.
    // r[impl builder.log-limit+4]
    pub fn finish(mut self) -> (Option<AssembledBatch>, u64) {
        let dropped = std::mem::take(&mut self.lines_dropped_this_window);
        if dropped > 0 {
            metrics::counter!("rio_builder_log_lines_suppressed_total").increment(dropped);
            let marker = rate_suppression_marker(dropped, self.limits.rate_lines_per_sec);
            self.push_marker(marker);
        }
        // flush() drains the relay-shed tally (when attached), so the
        // terminal batch also carries the trailing shed marker that
        // would otherwise have no later batch to ride.
        let batch = self.flush();
        let count = self.next_line_number;
        if batch.lines.is_empty() {
            // An empty batch cannot carry a lease: a nonzero drain
            // pushes a marker line, making the batch nonempty — so
            // dropping it here loses nothing.
            debug_assert_eq!(batch.shed_lease, 0);
            (None, count)
        } else {
            (Some(batch), count)
        }
    }

    /// Whether there are buffered lines waiting to be sent.
    ///
    /// Gates the periodic `flush_tick` and the phase-boundary flush in
    /// the executor's log loop. Deliberately does NOT check
    /// `lines_dropped_this_window`: it's not this tick's job to emit the
    /// marker (only window-reset / [`finish`](Self::finish) do), and
    /// reporting "pending" with no lines would send an empty batch every
    /// 100ms.
    pub fn has_pending(&self) -> bool {
        !self.lines.is_empty()
    }

    /// Inject an out-of-band marker line. THE sole marker-insertion
    /// chokepoint: the rate-suppression markers (`add_line`'s
    /// window-reset, [`finish`](Self::finish)'s terminal drain) and the
    /// relay-shed marker ([`flush`](Self::flush)) all land here —
    /// private, so no path outside the batcher can grow a batch past
    /// the cap or skip the mechanics below.
    ///
    /// Mechanics, uniform for every marker:
    /// - pushed directly — never through [`add_line`](Self::add_line),
    ///   so it cannot recurse and cannot itself be rate-dropped;
    /// - counts toward `total_bytes` (it IS transmitted; the same
    ///   accepted ~one-marker overshoot of the size limit applies) and
    ///   consumes a line number through the normal flush path;
    /// - initializes `batch_start` when it opens a batch, so a
    ///   marker-opened batch waits out the full 100ms window instead of
    ///   inheriting a stale `batch_start` and tick-flushing immediately;
    /// - keeps the emitted batch within `MAX_BATCH_LINES +
    ///   MARKER_HEADROOM` lines (see [`MARKER_HEADROOM`] for why the
    ///   bound holds structurally; the debug assert pins it under
    ///   test).
    fn push_marker(&mut self, text: String) {
        debug_assert!(
            self.lines.len() < MAX_BATCH_LINES + MARKER_HEADROOM,
            "marker would exceed the sanctioned batch headroom: {} lines buffered",
            self.lines.len(),
        );
        let marker = text.into_bytes();
        if self.lines.is_empty() {
            self.batch_start = Instant::now();
        }
        self.total_bytes += marker.len() as u64;
        self.lines.push(marker);
    }
}

/// A batch as assembled by [`LogBatcher::flush`], carrying the
/// relay-shed count its assembly drained from the shared tally — the
/// **shed lease**.
///
/// `flush` performs a destructive read (`swap(0)`) of the tally and
/// bakes the count into a marker line, but whether that batch is ever
/// *delivered* is decided later, by
/// [`SheddingLogSender::try_send_batch`]. The lease keeps the
/// subtraction provisional until that settlement: sink-accepted →
/// consumed (the marker is the count's carrier of record); shed or
/// closed → restored to the tally via `fetch_add`, so the count rides
/// the next delivered batch's marker instead of vanishing with its
/// carrier (round-16 bug_065).
///
/// Constructible only by `flush` (private fields): a raw
/// [`BuildLogBatch`] cannot reach [`SheddingLogSender::try_send_batch`]
/// at all, so no assembly path can opt out of settlement. Banners —
/// batches built outside the batcher, with no drained count — go
/// through the lease-free [`SheddingLogSender::try_send_banner`].
/// Read access is via `Deref`; there is deliberately no way to take
/// the inner batch out without settling.
///
/// The lease must be settled against the sender whose tally it was
/// drained from (`LogBatcher::attach_relay_shed` shares exactly that
/// `Arc`, and clones share one tally — in the executor there is one
/// tally per build's display stream).
#[must_use = "an unsent AssembledBatch strands its shed lease — settle it via try_send_batch"]
#[derive(Debug)]
pub struct AssembledBatch {
    batch: BuildLogBatch,
    shed_lease: u64,
}

impl AssembledBatch {
    /// The drained relay-shed count this batch's marker carries.
    #[cfg(test)]
    pub(crate) fn shed_lease(&self) -> u64 {
        self.shed_lease
    }
}

impl std::ops::Deref for AssembledBatch {
    type Target = BuildLogBatch;

    fn deref(&self) -> &BuildLogBatch {
        &self.batch
    }
}

/// Outcome of a [`SheddingLogSender`] submission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogSendOutcome {
    /// Delivered to the sink.
    Sent,
    /// The sink was full: the message was dropped and tallied. The
    /// caller continues — display traffic never blocks the worker.
    Shed,
    /// The sink is closed (worker shutdown). Senders should stop.
    Closed,
}

/// Best-effort sender for the *display* stream: `BuildLogBatch` and
/// `BuildPhase` only.
///
/// The type IS the boundary: control messages (`CompletionReport`,
/// `PrefetchComplete`, `WorkAssignmentAck`, `ExecutorRegister`) cannot
/// be constructed through it — they keep their awaited, guaranteed
/// sends on the raw sink sender. Display messages go through
/// `try_send`: a full sink (scheduler-link backpressure filling the
/// permanent 256-slot buffer) sheds the message, counts it in
/// `rio_builder_log_messages_shed_total`, and the next delivered batch
/// carries one suppression marker line — guaranteed across carrier
/// death by the [`AssembledBatch`] shed lease, settled in
/// [`try_send_batch`](Self::try_send_batch) — so a degraded scheduler
/// link degrades the log stream, never the build or its enforcement.
///
/// Clones share one shed tally: the banner senders and the log loop
/// report through the same counter, and the loop's marker covers both.
// r[impl builder.relay.log-shed]
#[derive(Clone)]
pub struct SheddingLogSender {
    tx: mpsc::Sender<ExecutorMessage>,
    shed: Arc<AtomicU64>,
}

impl SheddingLogSender {
    pub fn new(tx: mpsc::Sender<ExecutorMessage>) -> SheddingLogSender {
        SheddingLogSender {
            tx,
            shed: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Submit an assembled batch and SETTLE its shed lease at the
    /// authoritative event — the delivery decision (`r[builder.relay.log-shed]`
    /// says the marker rides the next **delivered** batch; assembly is
    /// not delivery):
    ///
    /// - `Sent`: the sink accepted the carrier; the lease is consumed —
    ///   the marker line is now the count's carrier of record.
    /// - `Shed`: the carrier died between the tally read and
    ///   settlement; the lease is restored to the tally (plus the usual
    ///   `+1` for the carrier itself), so the next delivered batch's
    ///   marker reports the full count. No metric increment for the
    ///   restore — those messages were already counted in
    ///   `rio_builder_log_messages_shed_total` when they originally
    ///   shed; the tally only feeds marker display.
    /// - `Closed`: worker shutdown; the lease is restored for
    ///   accounting honesty (`Closed` is not a shed: no `+1`, no
    ///   metric). If the loop exits before another delivery, the
    ///   restored count is bounded display loss — the metric retains
    ///   the truth.
    pub fn try_send_batch(&self, batch: AssembledBatch) -> LogSendOutcome {
        let AssembledBatch { batch, shed_lease } = batch;
        let outcome = self.try_send(
            ExecutorMessage {
                msg: Some(executor_message::Msg::LogBatch(batch)),
            },
            "log_batch",
        );
        if outcome != LogSendOutcome::Sent && shed_lease > 0 {
            self.shed.fetch_add(shed_lease, Ordering::Relaxed);
        }
        outcome
    }

    /// Submit a banner batch — built outside the [`LogBatcher`], so it
    /// carries no shed lease. The ONLY way a non-`flush` batch reaches
    /// the sink; routing a flushed batch through here is impossible
    /// (its lease is locked inside [`AssembledBatch`]).
    pub fn try_send_banner(&self, batch: BuildLogBatch) -> LogSendOutcome {
        self.try_send_batch(AssembledBatch {
            batch,
            shed_lease: 0,
        })
    }

    pub fn try_send_phase(&self, phase: BuildPhase) -> LogSendOutcome {
        self.try_send(
            ExecutorMessage {
                msg: Some(executor_message::Msg::Phase(phase)),
            },
            "phase",
        )
    }

    /// Drain the shed tally. Test-only: in production the tally is
    /// drained exclusively by [`LogBatcher::flush`] into an
    /// [`AssembledBatch`] lease — a second destructive reader would
    /// race counts away from the marker path.
    #[cfg(test)]
    pub(crate) fn take_shed(&self) -> u64 {
        self.shed.swap(0, Ordering::Relaxed)
    }

    fn try_send(&self, msg: ExecutorMessage, kind: &'static str) -> LogSendOutcome {
        match self.tx.try_send(msg) {
            Ok(()) => LogSendOutcome::Sent,
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.shed.fetch_add(1, Ordering::Relaxed);
                metrics::counter!("rio_builder_log_messages_shed_total", "kind" => kind)
                    .increment(1);
                LogSendOutcome::Shed
            }
            Err(mpsc::error::TrySendError::Closed(_)) => LogSendOutcome::Closed,
        }
    }
}

// r[verify obs.log.batch-64-100ms+1]
#[cfg(test)]
mod tests {
    use super::*;

    fn mk(limits: LogLimits) -> LogBatcher {
        LogBatcher::new("drv-path".into(), "worker-1".into(), limits, 0)
    }

    /// `initial_line` seeds `next_line_number` so the first flush's
    /// `first_line_number` follows the worker's `rio:` banner header
    /// (sent directly on `log_tx`, outside the batcher) instead of
    /// colliding at line 0.
    #[test]
    fn initial_line_offsets_first_batch() {
        let mut batcher = LogBatcher::new("drv".into(), "w".into(), LogLimits::UNLIMITED, 3);
        assert_eq!(batcher.line_count(), 3, "seeded before any lines");
        batcher.add_line(b"a".to_vec());
        let batch = batcher.flush();
        assert_eq!(batch.first_line_number, 3);
        assert_eq!(batcher.line_count(), 4, "advances past the seed");
    }

    // -----------------------------------------------------------------------
    // Batching (unchanged behavior, new return type)
    // -----------------------------------------------------------------------

    #[test]
    fn test_batcher_accumulates_lines() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        for i in 0..63 {
            let result = batcher.add_line(format!("line {i}").into_bytes());
            assert!(matches!(result, AddLineResult::Buffered));
        }
        assert!(batcher.has_pending());
    }

    #[test]
    fn test_batcher_emits_at_64_lines() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        for i in 0..63 {
            assert!(matches!(
                batcher.add_line(format!("line {i}").into_bytes()),
                AddLineResult::Buffered
            ));
        }
        match batcher.add_line(b"line 63".to_vec()) {
            AddLineResult::BatchReady(batch) => {
                assert_eq!(batch.lines.len(), 64);
                assert_eq!(batch.first_line_number, 0);
                assert_eq!(batch.derivation_path, "drv-path");
                assert_eq!(batch.executor_id, "worker-1");
            }
            other => panic!("expected BatchReady, got {other:?}"),
        }
    }

    #[test]
    fn test_batcher_line_numbers_increment() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        for i in 0..64 {
            batcher.add_line(format!("line {i}").into_bytes());
        }
        batcher.add_line(b"next".to_vec());
        let batch = batcher.flush();
        assert_eq!(batch.first_line_number, 64);
        assert_eq!(batch.lines.len(), 1);
    }

    #[test]
    fn test_batcher_flush_partial() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        batcher.add_line(b"line 0".to_vec());
        batcher.add_line(b"line 1".to_vec());
        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 2);
        assert_eq!(batch.first_line_number, 0);
        assert!(!batcher.has_pending());
    }

    #[test]
    fn test_batcher_maybe_flush_no_timeout() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        batcher.add_line(b"line".to_vec());
        assert!(batcher.maybe_flush().is_none());
    }

    #[test]
    fn test_batcher_maybe_flush_empty() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        assert!(batcher.maybe_flush().is_none());
    }

    /// Verify the 100ms timeout causes a flush of a partial batch.
    /// Uses real time (LogBatcher uses Instant, not tokio::time).
    #[test]
    fn test_batcher_100ms_timeout_flush() {
        let mut batcher = mk(LogLimits::UNLIMITED);
        assert!(matches!(
            batcher.add_line(b"line 0".to_vec()),
            AddLineResult::Buffered
        ));
        assert!(batcher.maybe_flush().is_none());
        std::thread::sleep(std::time::Duration::from_millis(110));
        let batch = batcher.maybe_flush().expect("should flush after 100ms");
        assert_eq!(batch.lines.len(), 1);
        assert_eq!(batch.lines[0], b"line 0");
        assert!(!batcher.has_pending());
    }

    // -----------------------------------------------------------------------
    // Rate limiting
    // -----------------------------------------------------------------------

    // r[verify builder.log-limit+4]
    #[test]
    fn rate_limit_drops_excess_within_window() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 5,
            total_bytes: 0,
        });
        // 10 lines in <1s: first 5 buffered, next 5 dropped. ALL return
        // Buffered (drop is not an error).
        for i in 0..10 {
            match batcher.add_line(vec![i as u8]) {
                AddLineResult::Buffered => {}
                other => panic!("line {i} should be Buffered, got {other:?}"),
            }
        }
        let (batch, count) = batcher.finish();
        let batch = batch.expect("buffered lines + marker");
        assert_eq!(batch.lines.len(), 6, "5 buffered + 1 suppression marker");
        assert_eq!(batch.lines[4], vec![4u8], "last buffered is index 4");
        let marker = std::str::from_utf8(&batch.lines[5]).unwrap();
        assert!(
            marker.contains("5 lines suppressed"),
            "finish() emits marker for drops never followed by window-reset: {marker}"
        );
        assert_eq!(count, 6, "terminal count covers lines + marker");
    }

    /// Regression: build ends within the suppression window → finish()
    /// must emit the marker (was lost: only add_line()'s window-reset
    /// drained it).
    #[test]
    fn rate_limit_finish_emits_marker_when_build_ends_in_window() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 3,
            total_bytes: 0,
        });
        for _ in 0..7 {
            batcher.add_line(b"l".to_vec());
        }
        // No sleep, no further add_line — straight to consumption.
        // has_pending() is true here because 3 lines are buffered; it
        // does NOT report the 4 drops.
        assert!(batcher.has_pending());
        let (batch, _) = batcher.finish();
        let batch = batch.expect("buffered lines + marker");
        assert_eq!(batch.lines.len(), 4, "3 accepted + 1 marker");
        let marker = std::str::from_utf8(&batch.lines[3]).unwrap();
        assert!(marker.contains("4 lines suppressed"));
    }

    /// merged_bug_022 regression (the deleted daemon-era property,
    /// restored): a final suppression window whose accepted lines were
    /// ALL flushed already — empty buffer, drops pending — must still
    /// emit the marker + metric. The old exit path gated the terminal
    /// flush on `has_pending()`, which only sees buffered lines, so
    /// exactly this state lost the marker silently.
    // r[verify builder.log-limit+4]
    #[test]
    fn finish_drains_drops_with_empty_buffer() {
        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let mut b = mk(LogLimits {
            rate_lines_per_sec: 2,
            total_bytes: 0,
        });
        // 2 accepted + 3 dropped in the window…
        for _ in 0..5 {
            b.add_line(b"x".to_vec());
        }
        // …and the accepted lines are flushed mid-window (tick): the
        // buffer is now empty while 3 drops are pending.
        let mid = b.flush();
        assert_eq!(mid.lines.len(), 2);
        assert!(
            !b.has_pending(),
            "the lost-marker precondition: empty buffer"
        );

        let (batch, count) = b.finish();
        let batch = batch.expect("the marker must still be emitted");
        assert_eq!(batch.lines.len(), 1, "exactly the suppression marker");
        assert!(
            std::str::from_utf8(&batch.lines[0])
                .unwrap()
                .contains("3 lines suppressed"),
        );
        assert_eq!(count, 3, "2 lines + 1 marker numbered");
        assert_eq!(rec.get("rio_builder_log_lines_suppressed_total{}"), 3);
    }

    /// finish() with nothing pending — no lines, no drops, no shed —
    /// produces no batch (an empty terminal batch would be a useless
    /// 100-byte message per build).
    #[test]
    fn empty_finish_sends_nothing() {
        let b = mk(LogLimits {
            rate_lines_per_sec: 3,
            total_bytes: 0,
        });
        let (batch, count) = b.finish();
        assert!(batch.is_none());
        assert_eq!(count, 0);
    }

    /// The relay-shed marker is injected inside flush() itself (when the
    /// tally is attached): the batch being assembled carries it — no
    /// delivery site can forget, and the marker can never trail with no
    /// later batch to ride.
    // r[verify builder.relay.log-shed]
    #[test]
    fn marker_injected_inside_flush() {
        let (tx, _rx) = mpsc::channel(1);
        let sender = SheddingLogSender::new(tx);
        let mut b = mk(LogLimits::UNLIMITED);
        b.attach_relay_shed(&sender);

        b.add_line(b"real".to_vec());
        // Two display sends shed (the single sink slot is empty but we
        // never deliver — simulate by filling it).
        assert_eq!(
            sender.try_send_phase(BuildPhase {
                derivation_path: "/rio/store/aaa-x.drv".into(),
                phase: "buildPhase".into(),
            }),
            LogSendOutcome::Sent
        );
        for _ in 0..2 {
            assert_eq!(
                sender.try_send_phase(BuildPhase {
                    derivation_path: "/rio/store/aaa-x.drv".into(),
                    phase: "buildPhase".into(),
                }),
                LogSendOutcome::Shed
            );
        }

        let batch = b.flush();
        assert_eq!(batch.lines.len(), 2, "real line + shed marker, same batch");
        assert!(
            std::str::from_utf8(&batch.lines[1])
                .unwrap()
                .contains("2 log messages shed"),
        );
        // The tally drained: the next flush carries no marker.
        b.add_line(b"next".to_vec());
        assert_eq!(b.flush().lines.len(), 1);
    }

    /// Regression: mid-window tick `flush()` with buffered lines + drops
    /// must NOT drain drops (would emit a fragmentary marker, then a
    /// second one at window-reset — violates `r[builder.log-limit+4]`'s
    /// "single marker at window reset"). Only `add_line`'s reset and
    /// `final_flush()` drain.
    // r[verify builder.log-limit+4]
    #[test]
    fn rate_limit_single_marker_per_window_under_mixed_tick_flush() {
        let mut b = mk(LogLimits {
            rate_lines_per_sec: 5,
            total_bytes: 0,
        });
        // Window 0: 5 accepted + 3 suppressed. lines.len()==5 (< MAX_BATCH_LINES).
        for _ in 0..8 {
            b.add_line(b"x".to_vec());
        }
        assert_eq!(b.lines_dropped_this_window, 3);
        // Mid-window tick flush — must NOT emit a marker.
        let batch = b.flush();
        assert_eq!(batch.lines.len(), 5, "tick flush must not inject marker");
        assert_eq!(
            b.lines_dropped_this_window, 3,
            "drops survive tick flush (belong to window, not batch)"
        );
        // 2 more suppressed in same window.
        for _ in 0..2 {
            b.add_line(b"x".to_vec());
        }
        assert_eq!(b.lines_dropped_this_window, 5);
        // Force window reset (back-date window_start; LogBatcher uses
        // real Instant so paused tokio-time wouldn't help).
        b.window_start = Instant::now() - Duration::from_secs(2);
        b.add_line(b"y".to_vec());
        // add_line's reset injected ONE marker for the full 5 drops.
        let batch = b.flush();
        let markers: Vec<_> = batch
            .lines
            .iter()
            .filter(|l| l.starts_with(b"[rio:"))
            .collect();
        assert_eq!(markers.len(), 1, "exactly one marker per window");
        assert!(
            std::str::from_utf8(markers[0])
                .unwrap()
                .contains("5 lines suppressed"),
            "marker reports total window drops, not a fragment"
        );
        assert_eq!(b.lines_dropped_this_window, 0, "window-reset drained drops");
    }

    #[test]
    fn rate_limit_marker_on_window_reset() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 3,
            total_bytes: 0,
        });
        // Fill the window: 3 accepted, 4 dropped.
        for _ in 0..7 {
            assert!(matches!(
                batcher.add_line(b"l".to_vec()),
                AddLineResult::Buffered
            ));
        }
        // Real-time sleep > 1s → window resets. (Instant doesn't advance
        // under paused tokio-time; rate limiting against wall-clock is
        // the point.)
        std::thread::sleep(Duration::from_millis(1100));
        // Next line: marker injected first (does NOT count toward this
        // window's quota), then the line. Window quota is 3 → 3 real
        // lines fit; the marker is extra.
        for i in 0..3 {
            match batcher.add_line(b"m".to_vec()) {
                AddLineResult::Buffered => {}
                other => panic!("post-reset line {i} should be accepted, got {other:?}"),
            }
        }
        let batch = batcher.flush();
        // 3 (window 1) + 1 marker + 3 (window 2) = 7.
        assert_eq!(batch.lines.len(), 7);
        let marker = std::str::from_utf8(&batch.lines[3]).unwrap();
        assert!(
            marker.contains("4 lines suppressed") && marker.contains("log_rate_limit"),
            "marker at index 3 should report 4 drops: {marker}"
        );
    }

    #[test]
    fn rate_limit_no_marker_when_nothing_dropped() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 3,
            total_bytes: 0,
        });
        for _ in 0..3 {
            batcher.add_line(b"l".to_vec());
        }
        std::thread::sleep(Duration::from_millis(1100));
        for _ in 0..3 {
            batcher.add_line(b"m".to_vec());
        }
        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 6, "no marker injected — nothing dropped");
        assert!(batch.lines.iter().all(|l| l == b"l" || l == b"m"));
    }

    #[test]
    fn rate_limit_zero_means_unlimited() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 0,
        });
        // 1000 lines in rapid succession — no trip.
        for _ in 0..1000 {
            // Some of these will be BatchReady (every 64th), none should trip.
            if let AddLineResult::LimitExceeded { trip } = batcher.add_line(b"x".to_vec()) {
                panic!("rate=0 should be unlimited, got: {trip}")
            }
        }
    }

    // -----------------------------------------------------------------------
    // Size limiting
    // -----------------------------------------------------------------------

    #[test]
    fn size_limit_trips_on_prospective_total() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 100,
        });
        // 50 + 40 = 90 bytes, under limit.
        assert!(matches!(
            batcher.add_line(vec![b'a'; 50]),
            AddLineResult::Buffered
        ));
        assert!(matches!(
            batcher.add_line(vec![b'b'; 40]),
            AddLineResult::Buffered
        ));
        // 90 + 20 = 110 > 100 — trips BEFORE buffering (prospective check).
        match batcher.add_line(vec![b'c'; 20]) {
            AddLineResult::LimitExceeded { trip } => {
                let reason = trip.to_string();
                assert!(reason.contains("log_size_limit"));
                assert!(reason.contains("110"), "should show prospective total");
                assert!(reason.contains("100"), "should show the limit");
                assert_eq!(
                    trip,
                    LogCapTrip::Bytes {
                        would_be: 110,
                        limit: 100
                    },
                    "the typed figures are the per-attempt evidence"
                );
            }
            other => panic!("should trip size limit, got {other:?}"),
        }
        // The 90-byte pre-trip content is still buffered.
        let batch = batcher.flush();
        assert_eq!(batch.lines.len(), 2);
        assert_eq!(batch.lines[0].len() + batch.lines[1].len(), 90);
    }

    #[test]
    fn size_limit_exactly_at_threshold_is_ok() {
        // Edge case: exactly hitting the limit should be accepted.
        // Only EXCEEDING it trips. (`>` not `>=` in the check.)
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 100,
        });
        assert!(matches!(
            batcher.add_line(vec![b'x'; 100]),
            AddLineResult::Buffered
        ));
        // Next byte trips.
        assert!(matches!(
            batcher.add_line(vec![b'y'; 1]),
            AddLineResult::LimitExceeded { .. }
        ));
    }

    #[test]
    fn size_limit_tracks_across_batches() {
        // total_bytes accumulates across flushed batches, not just the
        // current buffer. 64-line batch flush doesn't reset the counter.
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 70, // just over one full batch of 1-byte lines
        });
        for i in 0..64 {
            match batcher.add_line(vec![b'x'; 1]) {
                AddLineResult::Buffered => {}
                AddLineResult::BatchReady(_) => {
                    assert_eq!(i, 63, "batch should flush on 64th line")
                }
                AddLineResult::LimitExceeded { .. } => panic!("64 bytes < 70 limit"),
            }
        }
        // Now at 64 bytes. 6 more fit (= 70), 7th trips.
        for _ in 0..6 {
            assert!(matches!(
                batcher.add_line(vec![b'x'; 1]),
                AddLineResult::Buffered
            ));
        }
        assert!(matches!(
            batcher.add_line(vec![b'x'; 1]),
            AddLineResult::LimitExceeded { .. }
        ));
    }

    #[test]
    fn size_limit_zero_means_unlimited() {
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 0,
            total_bytes: 0,
        });
        // 10 MiB across many lines — no trip.
        for _ in 0..100 {
            if let AddLineResult::LimitExceeded { trip } = batcher.add_line(vec![b'x'; 100_000]) {
                panic!("size=0 should be unlimited, got: {trip}")
            }
        }
    }

    #[test]
    fn rate_dropped_lines_dont_count_toward_size() {
        // A rate-suppressed line is never buffered or transmitted, so it
        // costs nothing — it must NOT count toward total_bytes.
        let mut batcher = mk(LogLimits {
            rate_lines_per_sec: 1,
            total_bytes: 10,
        });
        // First line (5 bytes) buffered.
        assert!(matches!(
            batcher.add_line(vec![b'x'; 5]),
            AddLineResult::Buffered
        ));
        // 100 dropped lines (rate=1, window full). None counted toward size.
        for _ in 0..100 {
            assert!(matches!(
                batcher.add_line(vec![b'y'; 5]),
                AddLineResult::Buffered
            ));
        }
        // total_bytes is still 5 — the 100 dropped lines contributed
        // nothing. (The old final-probe via a 6-byte LimitExceeded no
        // longer applies: rate now runs first, so a 6-byte line in this
        // saturated window would be rate-dropped, not size-rejected.)
        assert_eq!(
            batcher.total_bytes, 5,
            "dropped lines must not pollute size"
        );
    }

    /// bug_140 regression: a large line arriving while the rate window is
    /// full would have been rate-dropped (zero transmitted bytes) — it
    /// must NOT trip the size limit.
    // r[verify builder.log-limit+4]
    #[test]
    fn rate_dropped_large_line_does_not_trip_size() {
        let mut b = mk(LogLimits {
            rate_lines_per_sec: 1,
            total_bytes: 100,
        });
        assert!(matches!(b.add_line(vec![b'x'; 5]), AddLineResult::Buffered));
        // Rate window full. 1 MiB line would trip size if checked first —
        // but it's rate-dropped: zero transmitted bytes, zero size cost.
        match b.add_line(vec![b'y'; 1_000_000]) {
            AddLineResult::Buffered => {}
            other => panic!("rate-dropped line must not trip size limit: {other:?}"),
        }
        assert_eq!(b.lines_dropped_this_window, 1);
        assert_eq!(b.total_bytes, 5);
    }

    /// bug_141 regression: at `rate=1`, after the first overflow the
    /// suppression marker must NOT consume the entire 1-line quota and
    /// black-hole all subsequent real lines.
    #[test]
    fn rate_one_recovers_after_overflow() {
        let mut b = mk(LogLimits {
            rate_lines_per_sec: 1,
            total_bytes: 0,
        });
        b.add_line(b"a".to_vec()); // accepted
        b.add_line(b"b".to_vec()); // dropped (window full)
        assert_eq!(b.lines_dropped_this_window, 1);
        // Force window reset (back-date window_start; LogBatcher uses
        // real Instant so paused tokio-time wouldn't help).
        b.window_start = Instant::now() - Duration::from_secs(2);
        // Next line: marker injected (does NOT consume quota) + real
        // line accepted.
        assert!(matches!(b.add_line(b"c".to_vec()), AddLineResult::Buffered));
        assert_eq!(
            b.lines_dropped_this_window, 0,
            "real line accepted, not dropped"
        );
        let batch = b.flush();
        // a, marker, c — real line c MUST be present.
        assert_eq!(batch.lines.last().unwrap(), b"c");
        assert!(batch.lines.iter().any(|l| l.starts_with(b"[rio:")));
    }

    // -- push_marker / SheddingLogSender -------------------------------------

    /// The relay-shed marker mirrors the rate-marker mechanics: counts
    /// toward total_bytes, consumes a line number through flush, never
    /// recurses through add_line.
    #[test]
    fn push_marker_counts_bytes_and_line_numbers() {
        let mut b = mk(LogLimits::UNLIMITED);
        b.add_line(b"real".to_vec());
        let before = b.total_bytes;
        b.push_marker("[rio: 3 log messages shed (scheduler link backpressure)]".into());
        assert_eq!(
            b.total_bytes - before,
            "[rio: 3 log messages shed (scheduler link backpressure)]".len() as u64,
            "the marker is transmitted, so it counts toward the byte cap"
        );
        let batch = b.flush();
        assert_eq!(batch.lines.len(), 2);
        assert!(batch.lines[1].starts_with(b"[rio: 3 log messages shed"));
        // The marker consumed a line number: the next batch starts at 2.
        b.add_line(b"next".to_vec());
        assert_eq!(b.flush().first_line_number, 2);
    }

    /// A marker pushed into an empty batcher still flushes via the
    /// batch timeout (batch_start is initialized).
    #[test]
    fn push_marker_into_empty_batcher_is_flushable() {
        let mut b = mk(LogLimits::UNLIMITED);
        b.push_marker("[rio: 1 log messages shed (scheduler link backpressure)]".into());
        assert!(b.has_pending());
        let batch = b.flush();
        assert_eq!(batch.lines.len(), 1);
    }

    /// merged_bug_026 worst case, sanctioned: a batch already at 63
    /// real lines takes the window-reset rate marker (64), the incoming
    /// real line (65, trips the flush), and the relay-shed marker (66)
    /// — exactly `MAX_BATCH_LINES + MARKER_HEADROOM`, never more, with
    /// 64 real lines. The `push_marker` debug assert runs live on every
    /// insertion in this test.
    // r[verify obs.log.batch-64-100ms+1]
    #[test]
    fn emitted_batch_caps_at_max_plus_marker_headroom() {
        let (tx, _rx) = mpsc::channel(1);
        let sender = SheddingLogSender::new(tx);
        let mut b = mk(LogLimits {
            rate_lines_per_sec: 63,
            total_bytes: 0,
        });
        b.attach_relay_shed(&sender);

        // Window 0: 63 accepted (quota exactly), 7 dropped.
        for _ in 0..70 {
            assert!(matches!(b.add_line(b"x".to_vec()), AddLineResult::Buffered));
        }
        assert_eq!(b.lines.len(), 63, "63 buffered, none flushed yet");
        assert_eq!(b.lines_dropped_this_window, 7);

        // One display message shed since the last flush.
        assert_eq!(
            sender.try_send_phase(BuildPhase {
                derivation_path: "/rio/store/aaa-x.drv".into(),
                phase: "buildPhase".into(),
            }),
            LogSendOutcome::Sent
        );
        assert_eq!(
            sender.try_send_phase(BuildPhase {
                derivation_path: "/rio/store/aaa-x.drv".into(),
                phase: "buildPhase".into(),
            }),
            LogSendOutcome::Shed
        );

        // Force the window reset; the next real line stacks marker +
        // line onto the 63-line buffer and trips the flush, which adds
        // the shed marker.
        b.window_start = Instant::now() - Duration::from_secs(2);
        let batch = match b.add_line(b"final".to_vec()) {
            AddLineResult::BatchReady(batch) => batch,
            other => panic!("65 buffered lines must trip the flush, got {other:?}"),
        };

        assert_eq!(
            batch.lines.len(),
            MAX_BATCH_LINES + MARKER_HEADROOM,
            "the sanctioned worst case is exactly 64 real + 2 markers"
        );
        let markers: Vec<_> = batch
            .lines
            .iter()
            .filter(|l| l.starts_with(b"[rio:"))
            .collect();
        assert_eq!(markers.len(), MARKER_HEADROOM);
        assert_eq!(
            batch.lines.len() - markers.len(),
            MAX_BATCH_LINES,
            "real lines never exceed the cap — only markers ride above it"
        );
        assert!(
            std::str::from_utf8(markers[0])
                .unwrap()
                .contains("7 lines suppressed"),
        );
        assert!(
            std::str::from_utf8(markers[1])
                .unwrap()
                .contains("1 log messages shed"),
        );
        // All 66 lines consumed line numbers through the normal path.
        b.add_line(b"next".to_vec());
        assert_eq!(b.flush().first_line_number, 66);
    }

    /// merged_bug_026 secondary heal: the window-reset rate marker now
    /// goes through `push_marker`, which initializes `batch_start` when
    /// the marker opens the batch. Pre-fix the raw push left
    /// `batch_start` stale from the previous batch, so the next 100ms
    /// tick flushed the marker-opened batch immediately instead of
    /// waiting out the window.
    // r[verify obs.log.batch-64-100ms+1]
    #[test]
    fn window_reset_marker_initializes_batch_start() {
        let mut b = mk(LogLimits {
            rate_lines_per_sec: 2,
            total_bytes: 0,
        });
        // 2 accepted + 2 dropped, then the accepted lines tick-flush:
        // empty buffer, drops pending.
        for _ in 0..4 {
            b.add_line(b"x".to_vec());
        }
        assert_eq!(b.flush().lines.len(), 2);
        assert!(!b.has_pending());

        // Stale batch_start (from the flushed batch) + due window reset.
        b.batch_start = Instant::now() - Duration::from_secs(5);
        b.window_start = Instant::now() - Duration::from_secs(2);
        b.add_line(b"z".to_vec());
        assert!(b.has_pending(), "marker + line buffered");
        assert!(
            b.maybe_flush().is_none(),
            "the marker-opened batch must wait out its own 100ms window — \
             a stale batch_start here means the marker bypassed push_marker"
        );
    }

    /// Display sends shed on a full sink (counted, metric'd) and report
    /// Closed on a dropped sink; the tally drains once.
    #[test]
    fn shedding_sender_sheds_counts_and_closes() {
        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let (tx, mut rx) = mpsc::channel(1);
        let sender = SheddingLogSender::new(tx);
        let batch = || BuildLogBatch {
            derivation_path: "/rio/store/aaa-x.drv".into(),
            executor_id: "w0".into(),
            first_line_number: 0,
            lines: vec![b"l".to_vec()],
        };
        assert_eq!(sender.try_send_banner(batch()), LogSendOutcome::Sent);
        assert_eq!(sender.try_send_banner(batch()), LogSendOutcome::Shed);
        assert_eq!(
            sender.try_send_phase(BuildPhase {
                derivation_path: "/rio/store/aaa-x.drv".into(),
                phase: "buildPhase".into(),
            }),
            LogSendOutcome::Shed
        );
        assert_eq!(sender.take_shed(), 2);
        assert_eq!(sender.take_shed(), 0, "the tally drains once");
        assert_eq!(
            rec.get("rio_builder_log_messages_shed_total{kind=log_batch}"),
            1
        );
        assert_eq!(
            rec.get("rio_builder_log_messages_shed_total{kind=phase}"),
            1
        );

        // Closed sink: the loop-exit signal, not a shed.
        rx.close();
        assert_eq!(sender.try_send_banner(batch()), LogSendOutcome::Closed);
        assert_eq!(sender.take_shed(), 0, "Closed is not tallied as shed");
    }

    /// THE bug_065 carrier-kill pin (fix-discipline R2 AUTHORITATIVE
    /// EVENT: a chokepoint performing a destructive read of shared
    /// evidence upstream of settlement MUST carry a typed restore path
    /// proven by a test that kills the carrier between read and
    /// settlement — this is that test). `flush()` swaps the tally (the
    /// destructive read) and bakes the count into the carrier batch;
    /// the carrier is then killed (shed on the full sink) BEFORE the
    /// delivery decision; the lease must flow back so the next
    /// DELIVERED marker reports every shed message. Pre-fix the count
    /// died with the carrier: the tenant saw "1 message shed" when ≥6
    /// were.
    // r[verify builder.relay.log-shed]
    #[test]
    fn shed_lease_restored_when_carrier_killed_between_read_and_settlement() {
        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let (tx, mut rx) = mpsc::channel(1);
        let sender = SheddingLogSender::new(tx);
        let mut b = mk(LogLimits::UNLIMITED);
        b.attach_relay_shed(&sender);

        let phase = || BuildPhase {
            derivation_path: "/rio/store/aaa-x.drv".into(),
            phase: "buildPhase".into(),
        };
        // Occupy the only sink slot, then shed 5 display messages.
        assert_eq!(sender.try_send_phase(phase()), LogSendOutcome::Sent);
        for _ in 0..5 {
            assert_eq!(sender.try_send_phase(phase()), LogSendOutcome::Shed);
        }

        // The destructive read: assembly drains the tally into the
        // carrier's marker + lease.
        b.add_line(b"real".to_vec());
        let carrier = b.flush();
        assert_eq!(carrier.shed_lease(), 5);
        assert!(
            std::str::from_utf8(&carrier.lines[1])
                .unwrap()
                .contains("5 log messages shed"),
        );

        // Kill the carrier between read and settlement: the sink is
        // still full, so the batch itself sheds.
        assert_eq!(sender.try_send_batch(carrier), LogSendOutcome::Shed);
        // The restore did NOT re-increment the metric — only the
        // carrier's own shed did (the 5 were counted when they
        // originally shed).
        assert_eq!(
            rec.get("rio_builder_log_messages_shed_total{kind=log_batch}"),
            1
        );
        assert_eq!(
            rec.get("rio_builder_log_messages_shed_total{kind=phase}"),
            5
        );

        // Sink drains; the next delivered batch's marker carries the
        // FULL count: 5 restored + 1 for the dead carrier. Conservation:
        // every shed message is reported by exactly one delivered
        // marker.
        let _ = rx.try_recv().expect("slot frees");
        b.add_line(b"after".to_vec());
        let next = b.flush();
        assert_eq!(next.shed_lease(), 6, "5 restored + 1 carrier");
        assert!(
            std::str::from_utf8(&next.lines[1])
                .unwrap()
                .contains("6 log messages shed"),
        );
        assert_eq!(sender.try_send_batch(next), LogSendOutcome::Sent);
        assert_eq!(
            sender.take_shed(),
            0,
            "settled: nothing strands in the tally"
        );
    }

    /// Settlement consumes the lease on `Sent`: once the carrier is
    /// sink-accepted, the tally stays drained — no double-count on the
    /// following batch.
    // r[verify builder.relay.log-shed]
    #[test]
    fn shed_lease_consumed_on_sent() {
        let (tx, mut rx) = mpsc::channel(1);
        let sender = SheddingLogSender::new(tx);
        let mut b = mk(LogLimits::UNLIMITED);
        b.attach_relay_shed(&sender);

        let phase = || BuildPhase {
            derivation_path: "/rio/store/aaa-x.drv".into(),
            phase: "buildPhase".into(),
        };
        assert_eq!(sender.try_send_phase(phase()), LogSendOutcome::Sent);
        for _ in 0..3 {
            assert_eq!(sender.try_send_phase(phase()), LogSendOutcome::Shed);
        }
        let _ = rx.try_recv().expect("slot frees");

        b.add_line(b"real".to_vec());
        let carrier = b.flush();
        assert_eq!(carrier.shed_lease(), 3);
        assert_eq!(sender.try_send_batch(carrier), LogSendOutcome::Sent);
        assert_eq!(sender.take_shed(), 0, "lease consumed at delivery");
        // The following batch carries no marker.
        b.add_line(b"next".to_vec());
        assert_eq!(b.flush().lines.len(), 1);
    }

    /// `Closed` settlement: the lease is restored (accounting honesty —
    /// the count was never delivered) but `Closed` is NOT a shed: no
    /// `+1` for the carrier, no metric increment. Bounded display loss
    /// at worker shutdown; the metric retains the truth.
    // r[verify builder.relay.log-shed]
    #[test]
    fn shed_lease_restored_on_closed_without_shed_count() {
        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);

        let (tx, mut rx) = mpsc::channel(1);
        let sender = SheddingLogSender::new(tx);
        let mut b = mk(LogLimits::UNLIMITED);
        b.attach_relay_shed(&sender);

        let phase = || BuildPhase {
            derivation_path: "/rio/store/aaa-x.drv".into(),
            phase: "buildPhase".into(),
        };
        assert_eq!(sender.try_send_phase(phase()), LogSendOutcome::Sent);
        for _ in 0..2 {
            assert_eq!(sender.try_send_phase(phase()), LogSendOutcome::Shed);
        }

        b.add_line(b"real".to_vec());
        let carrier = b.flush();
        assert_eq!(carrier.shed_lease(), 2);

        rx.close();
        assert_eq!(sender.try_send_batch(carrier), LogSendOutcome::Closed);
        assert_eq!(
            sender.take_shed(),
            2,
            "lease restored verbatim — Closed adds no carrier count"
        );
        assert_eq!(
            rec.get("rio_builder_log_messages_shed_total{kind=log_batch}"),
            0,
            "Closed is not a shed"
        );
    }

    /// The guaranteed path is the raw sink sender: a CompletionReport
    /// send BLOCKS on a full sink (backpressure) instead of shedding —
    /// pinned with a timeout probe that must elapse, then delivery
    /// once the sink drains.
    #[tokio::test]
    async fn completion_send_blocks_not_sheds() {
        use rio_proto::types::{CompletionReport, executor_message};

        let (tx, mut rx) = mpsc::channel::<ExecutorMessage>(1);
        // Fill the sink with a display message.
        let shedder = SheddingLogSender::new(tx.clone());
        assert_eq!(
            shedder.try_send_banner(BuildLogBatch {
                derivation_path: "/rio/store/aaa-x.drv".into(),
                executor_id: "w0".into(),
                first_line_number: 0,
                lines: vec![b"l".to_vec()],
            }),
            LogSendOutcome::Sent
        );

        let completion = ExecutorMessage {
            msg: Some(executor_message::Msg::Completion(CompletionReport {
                drv_path: "/rio/store/aaa-x.drv".into(),
                ..Default::default()
            })),
        };
        let send = tokio::spawn(async move { tx.send(completion).await });
        // The guaranteed send must still be pending while the sink is
        // full…
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!send.is_finished(), "a control send must block, not shed");
        // …and deliver once the sink drains.
        let _ = rx.recv().await;
        send.await
            .expect("send task")
            .expect("completion delivered after drain");
        assert!(matches!(
            rx.recv().await.unwrap().msg,
            Some(executor_message::Msg::Completion(_))
        ));
    }
}
