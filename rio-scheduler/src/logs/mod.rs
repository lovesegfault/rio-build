//! Per-derivation build-log ring buffers.
//!
//! Lives **outside** the DAG actor so that a chatty build (10k lines/sec)
//! can't fill the actor's bounded mpsc(10_000) channel with log traffic and
//! trip the 80%/60% backpressure hysteresis. The BuildExecution recv task
// r[impl obs.log.batch-64-100ms]
//! writes directly here; the actor only touches this indirectly (via the
//! `ForwardLogBatch` command for gateway-forward, and via the completion
//! flush trigger added in a later commit).
//!
//! ## Ordering guarantees
//!
//! DashMap is a sharded lock — writes to **different** drv_path keys are
//! truly concurrent. Writes to the **same** drv_path are serialized by the
//! shard's RwLock. In practice, a derivation builds on exactly one worker
//! at a time, and that worker's single BuildExecution recv task is the only
//! writer for that drv_path → no intra-key contention.
//!
//! Line ordering within a buffer is the `first_line_number` from the worker
//! (LogBatcher increments it monotonically per batch). We don't sort — we
//! assume batches arrive in order (same TCP stream, same task, no reorder).
//! If they don't, `read_since` will expose the gap to the caller, which is
//! correct (the caller sees what we have).

use std::collections::VecDeque;

use dashmap::{DashMap, DashSet};
use rio_nix::store_path::StorePath;
use rio_proto::types::BuildLogBatch;
use uuid::Uuid;

mod flush;
pub use flush::{FlushRequest, LogFlusher};

/// Extract the 32-char nixbase32 store-path hash from a derivation
/// identifier for use as the PG `drv_logs.drv_hash` column and the
/// `{drv_hash}` component of the S3 key.
///
/// This is the SINGLE source of truth shared by the flusher (write side,
/// [`log_s3_key`] + `upsert_drv_log`) and `AdminService.GetDerivationLogs`
/// (read side, PG lookup) so the derivation can never drift. Before this
/// helper existed, the write side keyed on the full `/nix/store/...` path
/// while the read side keyed on the basename — the PG lookup never matched
/// and S3 keys had embedded `//nix/store/` (double-slash from
/// `format!("{prefix}/.../{full_path}")`).
///
/// Accepts any of:
/// - full store path `/nix/store/{hash}-{name}.drv` → `{hash}`
/// - basename `{hash}-{name}.drv` → `{hash}`
/// - bare hash `{hash}` → unchanged
///
/// Idempotent: `drv_log_hash(drv_log_hash(s)) == drv_log_hash(s)`.
/// [`LogBuffers`] keys on this hash for both push and read so the
/// ring-buffer and S3 paths resolve client input identically.
pub fn drv_log_hash(s: &str) -> String {
    // Full store path → parsed hash_part. Validates nixbase32 + length.
    if let Ok(sp) = StorePath::parse(s) {
        return sp.hash_part();
    }
    // Not a parseable store path (no prefix, short test hash, or invalid
    // name char). Best-effort: strip `/nix/store/` if present, then take
    // the part before the first `-`. No `-` → already hash-shaped.
    let base = rio_nix::store_path::basename(s).unwrap_or(s);
    base.split_once('-')
        .map(|(h, _)| h)
        .unwrap_or(base)
        .to_string()
}

/// Construct the canonical S3 key for a derivation execution's log blob:
/// `logs/{drv_hash}/{exec_id}.log.zst` (or `.partial.log.zst` for periodic
/// snapshots). One blob per execution.
// r[impl obs.log.exec-keyed]
///
/// The `logs/` segment is fixed (peer to rio-store's `chunks/` in the same
/// bucket); rio assumes a dedicated bucket, so there is no configurable
/// prefix.
///
/// `drv_path` is normalized via [`drv_log_hash`] (full path / basename /
/// bare hash). `exec_id` is the per-execution UUIDv7 minted by
/// `assign_to_worker`. `partial` is `true` for periodic (30s) snapshots of
/// an active build, `false` for the completion flush — the final blob
/// supersedes the `.partial` and the flusher best-effort deletes the
/// latter.
pub fn log_s3_key(drv_path: &str, exec_id: &Uuid, partial: bool) -> String {
    let suffix = if partial {
        ".partial.log.zst"
    } else {
        ".log.zst"
    };
    format!("logs/{}/{}{}", drv_log_hash(drv_path), exec_id, suffix)
}

/// Max lines retained per derivation. Beyond this, oldest lines are evicted.
///
/// Sizing: 100k lines × ~100 bytes/line (typical build output) ≈ 10 MiB per
/// active derivation. With ~50 concurrent active derivations (realistic upper
/// bound for a single scheduler before backpressure kicks in on the actor
/// channel), that's ~500 MiB peak — acceptable for a scheduler process that
/// typically has GBs of headroom.
///
/// This cap exists for the pathological case: a build that spews millions of
/// lines before the size-limit check on the worker kills it. We don't want
/// one runaway build to OOM the scheduler while the worker-side limit catches up.
pub(crate) const RING_CAPACITY: usize = 100_000;

// r[impl obs.log.ring-byte-cap]
/// Max bytes retained per derivation. Beyond this, oldest lines are
/// evicted. The worker is NOT trusted (executor_service.rs threat
/// model) — `RING_CAPACITY` alone bounds line COUNT, so a hostile
/// worker sending ~40 single-line ~256 MiB batches could pin ~10 GiB
/// without ever hitting line-count eviction (bug_080). 16 MiB matches
/// the doc's "~10 MiB" intent with headroom.
pub(crate) const RING_BYTE_CAP: usize = 16 * 1024 * 1024;

/// Max bytes per stored line. Longer lines are truncated at push so a
/// single line can't blow [`RING_BYTE_CAP`] on its own.
pub(crate) const MAX_LINE_LEN: usize = 64 * 1024;

/// (absolute line number, line bytes). Line number is the worker-assigned
/// `first_line_number + offset_within_batch` — absolute across the whole
/// build, not batch-local.
type Line = (u64, Vec<u8>);

/// Pre-failover log content recovered from the prior leader's `.partial`
/// blob (see `flush.rs` "recovered prefix"). Held compressed; decompressed
/// into the outgoing frame at flush time. Bounded by the prior leader's own
/// ring caps (the blob was a snapshot of a capped ring buffer).
pub(crate) struct RecoveredPrefix {
    pub(crate) first_line: u64,
    pub(crate) line_count: u64,
    pub(crate) total_bytes: u64,
    pub(crate) compressed: Vec<u8>,
}

/// Recovered-prefix state of an entry, read by the flusher.
pub(crate) enum PrefixState {
    /// Never looked for a stored prefix for this execution.
    Unchecked,
    /// Looked; none needed or none found. Don't look again.
    Checked,
    /// A stored prefix exists and MUST be prepended to every flush of this
    /// execution.
    Cached(std::sync::Arc<RecoveredPrefix>),
}

/// Per-derivation ring buffer with intrinsic byte-tracking.
/// `bytes` is `lines.iter().map(|(_, l)| l.len()).sum()` — maintained
/// incrementally so [`LogBuffers::push`] doesn't re-sum on every batch.
#[derive(Default)]
struct RingBuf {
    lines: VecDeque<Line>,
    bytes: usize,
    /// Per-execution identifier minted at `assign_to_worker`. Carrier for
    /// the periodic flush path (which is tick-driven and has no actor
    /// `FlushRequest` to read per-drv context from). `None` on entries
    /// created by [`LogBuffers::push`]'s `or_default()` (legacy path,
    /// tests only) — the flusher MUST skip those.
    exec_id: Option<Uuid>,
    /// Executor assigned this drv. The `(executor, drv)` binding check
    /// in [`LogBuffers::push_for`] compares against the calling stream's
    /// executor — a batch from any other source is dropped.
    /// r[impl sched.log.batch-binding]
    assigned_executor: Option<String>,
    /// See [`RecoveredPrefix`]. Set at most once per execution **per
    /// tenure** by the flusher's stored-coverage reconciliation; cleared by
    /// **any** restamp (cross-exec, or the same-exec restamp recovery
    /// performs at lease re-acquisition) and by the acquisition-time
    /// re-arm ([`LogBuffers::rearm_prefix_reconciliation`]), which clears
    /// every retained entry at lease acquisition. Ignored by
    /// `push_into`/eviction — never counted against the ring's line/byte
    /// caps.
    recovered_prefix: Option<std::sync::Arc<RecoveredPrefix>>,
    /// The flusher already looked for a stored prefix **this tenure**
    /// (whatever the outcome) — avoids a per-tick SELECT for evicted long
    /// logs. Cleared by any restamp and by the acquisition-time re-arm,
    /// like `recovered_prefix`. Once latched,
    /// same-tenure ring eviction past the stored row keeps the pre-existing
    /// accepted head-loss behavior (the row in that shape was produced by
    /// this tenure from this very ring after its reconcile); prior-tenure
    /// content stays covered regardless because it lives in
    /// `recovered_prefix`, not the ring.
    prefix_checked: bool,
    /// A final `FlushRequest` for this entry's execution is pending with the
    /// flusher: enqueued by the actor's `terminal_log_epilogue` and not yet
    /// resolved, or deferred (finalize guard could not read `drv_logs`) and
    /// retained for retry. `handle_cleanup_terminal_build` must not discard
    /// the entry while this is set — the flusher's processing of the request
    /// (drain, already-finalized refusal, empty-entry reap, or retention-cap
    /// drop) is the entry's reaper. Exception: a request the flusher drops
    /// because its enqueueing lease tenure ended (`flush_final`'s tenure
    /// pin) leaves the entry and this mark in place — the entry may be the
    /// live execution's buffer on a re-acquired leader, so its reaper is
    /// then the live tenure's own final, the drv's next dispatch discard,
    /// or process exit — unless the entry is still sealed for that exec (no
    /// restamp in the current tenure adopted it), in which case it is
    /// reaped: outright by the tenure-drop arm when empty, and — when
    /// non-empty — by the periodic flush, either once its snapshot UPSERT
    /// is refused because another tenure already finalized the execution,
    /// or by the sealed-empty reap at the empty-snapshot early-return once
    /// the stored-coverage reconcile has emptied its ring (this mark keeps
    /// doing its cleanup-skip job until then; an exec no tenure ever
    /// finalizes keeps being snapshotted only while its ring keeps lines
    /// past the stored coverage). Set at
    /// enqueue by the actor and
    /// re-asserted at deferral by the flusher (both exec-guarded); cleared by
    /// any `set_exec` restamp — cross-exec, or the same-exec restamp recovery
    /// performs at lease re-acquisition (the prior tenure's retained final
    /// can no longer resolve the entry) — and removed with the entry.
    final_pending: bool,
}

/// Per-derivation log ring buffers, keyed by [`drv_log_hash`] of the
/// derivation path.
///
/// Every accessor normalizes its `drv_path` argument through
/// [`drv_log_hash`] before the DashMap/DashSet op, so callers may pass
/// any of full store path / basename / bare hash and resolve to the
/// same buffer — the same normalizer the S3 read path uses, so the two
/// data sources can never disagree on input shape (bug_126: a basename
/// passed to `rio-cli logs` for an active build used to miss the ring
/// buffer keyed on the full path and fall through to a misleading
/// `not_found`).
///
/// A derivation is built exactly once even if N builds want it (DAG
/// merging), so one ring buffer per drv_hash is correct — the S3 flush
/// writes one blob and one `drv_logs` PG row per execution, keyed by
/// `(drv_hash, exec_id)`.
pub struct LogBuffers {
    buffers: DashMap<String, RingBuf>,
    /// Tombstone set: derivations that have reached a terminal state.
    /// [`Self::push`] / [`Self::push_for`] drop batches for sealed
    /// paths so a late `LogBatch` (still in flight on the
    /// BuildExecution stream after the worker sent CompletionReport)
    /// cannot recreate a buffer that the flusher already drained.
    /// Cleared by `LogFlusher::flush_final` once the final resolves,
    /// by the discard-family reaps ([`Self::discard`], the conditional
    /// entry reaps, terminal cleanup), and by any [`Self::set_exec`] restamp —
    /// cross-exec because the seal belongs to the execution being
    /// replaced, same-exec because at lease re-acquisition the prior
    /// tenure's pending final can no longer drain the entry.
    sealed: DashSet<String>,
}

impl LogBuffers {
    pub fn new() -> Self {
        Self {
            buffers: DashMap::new(),
            sealed: DashSet::new(),
        }
    }

    /// Push a batch. Evicts oldest lines if the buffer exceeds `RING_CAPACITY`.
    ///
    /// Drops the batch entirely if `drv_path` is [`Self::seal`]ed
    /// (terminal completion already fired). The late lines are lost —
    /// the build is done, the flusher has (or will) upload the final
    /// snapshot, and a few trailing batched lines are not worth an
    /// unbounded entry-count leak.
    pub fn push(&self, batch: &BuildLogBatch) {
        let key = drv_log_hash(&batch.derivation_path);
        if self.sealed.contains(&key) {
            return;
        }
        // `entry()` locks the shard's write lock for the duration of the
        // closure. For the same-key case (one worker per drv_path), this is
        // uncontended. For cross-key, DashMap's sharding means we rarely
        // block other drv_paths.
        let mut buf = self.buffers.entry(key).or_default();
        Self::push_into(&mut buf, batch);
    }

    /// Append batch lines to a ring buffer, truncating long lines and
    /// evicting oldest when over capacity. Shared by [`Self::push`]
    /// (legacy, `or_default` entry) and [`Self::push_for`] (gated entry,
    /// single-lock — caller already holds the shard write lock so the
    /// binding check and the write are atomic).
    fn push_into(buf: &mut RingBuf, batch: &BuildLogBatch) {
        let base = batch.first_line_number;
        for (i, line) in batch.lines.iter().enumerate() {
            // Slice-then-to_vec, NOT clone-then-truncate: `Vec::clone`
            // allocates `len()` bytes and `truncate` does not shrink
            // capacity, so an oversized line would be fully allocated and
            // the retained `Vec` would keep that capacity while
            // `buf.bytes` counts only `MAX_LINE_LEN` — the byte cap would
            // bound *accounted* bytes, not *allocated* bytes. The recv
            // arm already truncates worker-supplied lines before they get
            // here; this is defense in depth for the legacy `push()` path
            // (tests, future callers). r[impl sched.executor.input-bounds+2]
            let l = line[..line.len().min(MAX_LINE_LEN)].to_vec();
            buf.bytes += l.len();
            // Saturating: defense in depth for the legacy `push()` path
            // (tests, future callers); production batches are pre-validated
            // by `push_for`'s overflow arm.
            buf.lines.push_back((base.saturating_add(i as u64), l));
        }

        // Evict oldest lines if over capacity (line count OR bytes).
        // `pop_front` is O(1) on VecDeque. We evict AFTER push (not
        // before) so a single batch larger than RING_CAPACITY doesn't
        // leave the buffer empty — instead it keeps the tail of that
        // batch.
        while buf.lines.len() > RING_CAPACITY || buf.bytes > RING_BYTE_CAP {
            let Some((_, l)) = buf.lines.pop_front() else {
                break;
            };
            buf.bytes -= l.len();
        }
    }

    /// Drain all lines for a derivation, removing the buffer entry.
    ///
    /// Returns `None` if the buffer doesn't exist (never logged anything,
    /// or already drained). The production completion flush goes through
    /// [`Self::drain_if_exec`] instead (the unconditional remove here is
    /// exactly the TOCTOU that method closes); `drain` remains for tests
    /// and any future caller that owns the entry's whole lifecycle.
    ///
    /// Returns `(first_line, line_count, total_bytes, lines_in_order)`.
    /// `first_line` is the worker-assigned line number of `lines[0]` —
    /// non-zero iff ring eviction kicked in (>RING_CAPACITY lines emitted).
    /// `line_count` may be less than the total emitted by the worker for
    /// the same reason. The S3 blob and PG `drv_logs` row carry
    /// `first_line` so the read path operates in the same true-line-number
    /// space as the ring buffer (bug_084: previously the offset was
    /// discarded here and `try_s3` treated the client's `since` cursor as
    /// a 0-based blob index → silent log-tail loss after eviction).
    /// (`last_line` is not exposed here — the production flush paths that
    /// need the payload's true end use [`Self::drain_if_exec`] /
    /// `snapshot`.)
    pub fn drain(&self, drv_path: &str) -> Option<(u64, u64, u64, Vec<Vec<u8>>)> {
        let (_key, rb) = self.buffers.remove(&drv_log_hash(drv_path))?;
        let (first_line, _last_line, line_count, total_bytes, lines) = Self::into_drained(rb);
        Some((first_line, line_count, total_bytes, lines))
    }

    /// Drain the buffer for `drv_path` **only if** it is currently stamped
    /// with `expected`. Returns `None` both when no entry exists and when
    /// the entry's `exec_id` differs — the caller can't distinguish, and
    /// doesn't need to: both mean "the execution this request was pinned
    /// to is not the live one".
    ///
    /// This is the atomic form of `exec_id() == Some(expected)` followed
    /// by `drain()`. The two-step form is a TOCTOU: `assign_to_worker`'s
    /// re-dispatch (`discard()` + `set_exec()` with a fresh UUIDv7) can
    /// land between the flusher's read and its drain, in which case the
    /// unconditional `remove` deletes the *freshly stamped* entry and
    /// returns `Some((0, 0, 0, []))` — the new execution's ring buffer is
    /// gone before its worker sends a byte, and every subsequent
    /// `push_for` rejects with `no_assignment`. `remove_if` runs the
    /// comparison and the removal under the same shard write-lock, so a
    /// removed entry is guaranteed to have carried `expected`.
    /// [`Self::discard_if_owned_by`] is the sibling pattern for the
    /// disconnect-cleanup path (predicate on `assigned_executor` instead
    /// of `exec_id`).
    ///
    /// Like [`Self::drain`], this does not touch `sealed` — the caller
    /// owns the seal decision (see `flush_final`: unseal only after a
    /// successful drain; a refused drain means the live execution owns
    /// the seal, or there is no entry and `CleanupTerminalBuild` is the
    /// backstop).
    ///
    /// Returns `(first_line, last_line, line_count, total_bytes, lines)`;
    /// `last_line` is the highest worker-assigned line number present —
    /// the production flush path records the row span from it so an
    /// interior hole still counts toward the execution's true end
    /// (`obs.log.gap-span`).
    #[allow(
        clippy::type_complexity,
        reason = "ordering mirrors span()'s (first, last, count); the flusher \
                  destructures it once into FlushPayload fields — a named \
                  struct here would just duplicate that type"
    )]
    pub fn drain_if_exec(
        &self,
        drv_path: &str,
        expected: Uuid,
    ) -> Option<(u64, u64, u64, u64, Vec<Vec<u8>>)> {
        let (_key, rb) = self
            .buffers
            .remove_if(&drv_log_hash(drv_path), |_, e| e.exec_id == Some(expected))?;
        Some(Self::into_drained(rb))
    }

    /// Shared tail of [`Self::drain`] / [`Self::drain_if_exec`]: unpack a
    /// removed ring buffer into the `(first_line, last_line, line_count,
    /// total_bytes, lines)` tuple the flusher uploads. `last_line` is the
    /// highest worker-assigned line number present — equal to
    /// `first_line + line_count - 1` only when the payload is contiguous,
    /// larger when it carries an interior hole (lines delivered only to an
    /// interim leader during an A→B→A flap). first/last are meaningless
    /// zeros when `line_count == 0`.
    fn into_drained(rb: RingBuf) -> (u64, u64, u64, u64, Vec<Vec<u8>>) {
        let first_line = rb.lines.front().map(|(n, _)| *n).unwrap_or(0);
        let last_line = rb.lines.back().map(|(n, _)| *n).unwrap_or(0);
        let line_count = rb.lines.len() as u64;
        let total_bytes = rb.bytes as u64;
        let lines: Vec<Vec<u8>> = rb.lines.into_iter().map(|(_n, bytes)| bytes).collect();
        (first_line, last_line, line_count, total_bytes, lines)
    }

    /// Read lines with line number ≥ `since`, non-consuming.
    ///
    /// For `AdminService.GetDerivationLogs` — lets a late-joining dashboard
    /// client catch up from the ring buffer without blocking on S3.
    ///
    /// Returns `None` if no buffer exists for `drv_path` (derivation not
    /// active / already drained → caller falls through to S3). Returns
    /// `Some(vec)` if the buffer exists — `vec` MAY be empty when the
    /// caller is caught up (`since` ≥ newest line). The distinction
    /// matters: empty-because-absent → try S3; empty-because-caught-up
    /// → tell the client to re-poll. Conflating the two (the old `Vec`
    /// signature) made `try_ring_buffer` map "caught up on an active
    /// build" → S3 → `NotFound`.
    pub fn read_since(&self, drv_path: &str, since: u64) -> Option<Vec<Line>> {
        let buf = self.buffers.get(&drv_log_hash(drv_path))?;
        // Could binary-search for `since` since line numbers are monotone,
        // but `read_since` is called by dashboard polls (infrequent) on a
        // buffer that's already in-memory. Linear scan is fine until
        // profiling says otherwise; premature optimization would just
        // obscure the invariant that makes bisection valid.
        Some(
            buf.lines
                .iter()
                .filter(|(n, _)| *n >= since)
                .cloned()
                .collect(),
        )
    }

    /// Stamp a ring-buffer entry with the execution metadata, creating it
    /// if absent.
    ///
    /// Called at `assign_to_worker` immediately after [`Self::discard`]
    /// (which removes the prior entry and un-seals) and by recovery for
    /// each active assignment loaded from PG. Creates the entry — it is
    /// the carrier for the periodic flush, which runs before the
    /// worker's first `BuildLogBatch` arrives.
    ///
    /// **Re-stamping with a different `exec_id` clears the accumulated
    /// lines.** The lines belong to the previous execution; carrying them
    /// across the stamp would hand them to the flusher under the new
    /// execution's `drv_logs` row and S3 key. The dispatch call site never
    /// hits this (it discards first), but recovery's restamp runs against
    /// an ex-leader's *retained* `LogBuffers` (`clear_persisted_state`
    /// keeps it), and an interim leader may have re-dispatched the drv
    /// under a new `exec_id` in between. A same-`exec_id` re-stamp keeps
    /// the lines: that is a single lease flap with no interim re-dispatch,
    /// and the still-streaming worker's in-flight execution must keep
    /// accumulating across it. A cross-exec restamp also clears the
    /// seal tombstone. The seal was set by the prior execution's
    /// terminal to bridge its completion→drain window; once the entry
    /// is restamped that drain can never happen — the queued or
    /// retained `FlushRequest` pins the OLD exec_id, so `drain_if_exec`
    /// refuses the restamped entry and the request resolves as stale
    /// without touching it. With final-pending retention (the final's
    /// request is marked at enqueue, retained for retry on a guard
    /// failure, and `handle_cleanup_terminal_build` skips the discard
    /// for marked entries), a sealed entry meeting a cross-exec restamp
    /// is a designed outcome of one PG outage at the prior execution's
    /// terminal plus an interim re-dispatch — no longer the
    /// dropped-request-plus-missed-cleanup double failure it once
    /// required — and a surviving seal would make `push_for` (which
    /// checks the seal before the binding gate) silently drop every
    /// batch of the NEW execution, plus the gateway forward gated on
    /// acceptance. Late batches from the prior execution's worker are
    /// rejected by the `(executor, drv)` binding (re-stamped below); if
    /// the interim leader re-dispatched to the SAME worker, a stray
    /// prior-exec batch can still land (the restamp empties the ring,
    /// so the monotone gate has nothing to compare it against) — the
    /// same pre-existing exposure as a cross-exec restamp of an
    /// unsealed, still-Running entry, accepted in exchange for not
    /// muting the new execution outright. The same-exec arm clears the
    /// seal (and the final-pending mark) too: only recovery restamps
    /// the same exec_id, i.e. the lease generation has just bumped, so
    /// the retained final the seal was waiting on is now always
    /// tenure-dropped by the flusher without unsealing — the entry must
    /// stay writable for the still-streaming execution, and the new
    /// tenure's own terminal re-seals and re-marks it.
    /// Re-dispatch un-seals via [`Self::discard`].
    ///
    /// `executor` is stored for the `(executor, drv)` binding check in
    /// [`Self::push_for`].
    ///
    /// A same-exec re-stamp does, however, clear the prefix bookkeeping
    /// (`recovered_prefix`/`prefix_checked`): the flusher re-reconciles
    /// against the stored `drv_logs` row once per tenure, because an
    /// interim leader may have extended it past what this ring holds.
    pub fn set_exec(&self, drv_path: &str, exec_id: Uuid, executor: &str) {
        let key = drv_log_hash(drv_path);
        let mut entry = self.buffers.entry(key.clone()).or_default();
        if entry.exec_id.is_some() && entry.exec_id != Some(exec_id) {
            // Cross-exec restamp: the assignment was re-issued under a
            // different exec_id while this entry sat retained. Bounded,
            // accepted data loss: everything the prior execution flushed
            // is already stored under its own exec_id; only the ≤30s
            // unflushed tail of an abandoned execution is dropped.
            // info!, not debug!: fires at most once per drv per leader
            // re-acquisition (recovery-driven, not worker-driven, so it
            // cannot be log-spammed) and is the only trace an operator
            // gets for "exec E1's stored log is missing its tail".
            //
            // The seal tombstone belongs to the prior execution too: with
            // final-pending retention a sealed entry legitimately survives
            // to this restamp, and a stale seal would mute the NEW
            // execution (push_for checks it before the binding gate).
            // Cleared exactly like discard() does; see the method docs.
            let unsealed = self.sealed.remove(&key).is_some();
            tracing::info!(
                drv = %drv_path,
                old_exec = ?entry.exec_id,
                new_exec = %exec_id,
                dropped_lines = entry.lines.len(),
                dropped_bytes = entry.bytes,
                unsealed,
                "cross-exec log buffer restamp; dropping lines retained from the prior execution"
            );
            entry.lines.clear();
            entry.bytes = 0;
            // The recovered prefix (and the "already looked" flag) belong
            // to the previous execution — a different exec_id keys a
            // different drv_logs row and S3 blob, so carrying them across
            // would prepend the wrong execution's content.
            entry.recovered_prefix = None;
            entry.prefix_checked = false;
            // A pending-final mark belongs to the prior execution too: the
            // flusher's queued/retained request names the old exec_id and
            // will resolve as stale/no-entry once it sees the restamp.
            // Carrying the mark across would make terminal cleanup skip the
            // discard that bounds a dropped-FlushRequest leak for the NEW
            // execution's buffer.
            entry.final_pending = false;
        } else if entry.exec_id == Some(exec_id) {
            // Same-exec re-stamp. Only recovery does this (dispatch always
            // discards first), i.e. this replica just (re-)acquired the
            // lease. The retained lines are kept — they belong to this
            // execution — but the prefix bookkeeping encodes a conclusion
            // from a previous tenure: an interim leader may have extended
            // the stored drv_logs row past what this ring holds (or past
            // the prefix cached back then), so "already checked" /
            // "already cached" cannot be trusted across the flap. Clearing
            // both makes the flusher re-consult the row once on the next
            // non-empty flush (`reconcile_stored_prefix`); for an
            // unflapped interim that costs one point-SELECT per
            // re-acquisition. r[impl obs.log.stored-coverage-preserved]
            entry.recovered_prefix = None;
            entry.prefix_checked = false;
            // A seal (and final-pending mark) left by the prior tenure's
            // terminal can no longer be resolved: the re-acquisition bumped
            // the lease generation, so the retained final that was going to
            // drain this entry is now tenure-dropped by the flusher without
            // unsealing, and no other path unseals a sealed entry. Clear
            // both so the still-streaming worker's batches keep landing
            // (`push_for` checks the seal before the binding gate) and so
            // terminal cleanup regains its bound on this entry; the new
            // tenure's own terminal re-seals and re-marks when it processes
            // this execution's terminal.
            self.sealed.remove(&key);
            entry.final_pending = false;
        }
        entry.exec_id = Some(exec_id);
        entry.assigned_executor = Some(executor.to_owned());
    }

    /// Read the exec_id for a drv. `None` if no entry or `set_exec` was
    /// never called (a legacy `push()` test, or a recovery gap — the
    /// flusher MUST skip those rather than write a garbage S3 key).
    pub fn exec_id(&self, drv_path: &str) -> Option<Uuid> {
        self.buffers.get(&drv_log_hash(drv_path))?.exec_id
    }

    /// Recovered-prefix state for `drv_path`, guarded on `exec_id` so a
    /// re-dispatched entry can never leak the prior execution's prefix.
    /// Missing entry / exec mismatch ⇒ [`PrefixState::Checked`] (the safe
    /// "don't fetch").
    pub(crate) fn prefix_state(&self, drv_path: &str, exec_id: Uuid) -> PrefixState {
        match self.buffers.get(&drv_log_hash(drv_path)) {
            Some(e) if e.exec_id == Some(exec_id) => {
                match (&e.recovered_prefix, e.prefix_checked) {
                    (Some(p), _) => PrefixState::Cached(std::sync::Arc::clone(p)),
                    (None, true) => PrefixState::Checked,
                    (None, false) => PrefixState::Unchecked,
                }
            }
            _ => PrefixState::Checked,
        }
    }

    /// Mark the stored-prefix lookup as done with no prefix to carry.
    /// No-op when the entry is gone or stamped with a different exec.
    pub(crate) fn mark_prefix_checked(&self, drv_path: &str, exec_id: Uuid) {
        if let Some(mut e) = self.buffers.get_mut(&drv_log_hash(drv_path))
            && e.exec_id == Some(exec_id)
        {
            e.prefix_checked = true;
        }
    }

    /// Attach a recovered prefix (also marks checked). Returns `false`
    /// when the entry is gone or stamped with a different exec — the
    /// caller still holds the `Arc` and can use it for the in-flight
    /// flush, it just won't be reused on later ticks.
    pub(crate) fn set_recovered_prefix(
        &self,
        drv_path: &str,
        exec_id: Uuid,
        prefix: std::sync::Arc<RecoveredPrefix>,
    ) -> bool {
        match self.buffers.get_mut(&drv_log_hash(drv_path)) {
            Some(mut e) if e.exec_id == Some(exec_id) => {
                e.recovered_prefix = Some(prefix);
                e.prefix_checked = true;
                true
            }
            _ => false,
        }
    }

    /// Mark `drv_path`'s entry as having a final flush pending with the
    /// flusher, iff it is stamped with `exec_id`. Returns whether the mark was
    /// applied (false ⇒ entry missing or restamped — nothing to protect; the
    /// request will resolve as stale/no-entry). Called by
    /// `terminal_log_epilogue` on a successful enqueue and by `flush_final`'s
    /// deferral arm (re-assert + does-an-entry-stamped-with-this-exec-still-
    /// exist check).
    // r[impl obs.log.deferred-final-retry+4]
    pub(crate) fn mark_final_pending(&self, drv_path: &str, exec_id: Uuid) -> bool {
        match self.buffers.get_mut(&drv_log_hash(drv_path)) {
            Some(mut e) if e.exec_id == Some(exec_id) => {
                e.final_pending = true;
                true
            }
            _ => false,
        }
    }

    /// Whether `drv_path`'s entry has a final flush pending with the flusher
    /// (enqueued and not yet resolved, or deferred and retained for retry).
    /// Read by `handle_cleanup_terminal_build` to skip its discard.
    pub(crate) fn final_pending(&self, drv_path: &str) -> bool {
        self.buffers
            .get(&drv_log_hash(drv_path))
            .is_some_and(|e| e.final_pending)
    }

    /// Exec-guarded sibling of [`Self::discard_if_empty`]: remove the entry
    /// only if it holds zero lines AND is stamped with `expected`. The flusher
    /// uses this from the deferral arm, which runs on its own task — unlike
    /// the actor's epilogue call site, a re-dispatch can interleave here, and
    /// an unguarded empty-reap would delete the *new* execution's
    /// freshly-stamped entry before its worker streams a byte.
    pub(crate) fn discard_if_empty_for_exec(&self, drv_path: &str, expected: Uuid) -> bool {
        let key = drv_log_hash(drv_path);
        let removed = self
            .buffers
            .remove_if(&key, |_, e| {
                e.lines.is_empty() && e.exec_id == Some(expected)
            })
            .is_some();
        if removed {
            self.sealed.remove(&key);
        }
        removed
    }

    /// Remove the entry only if it is currently SEALED, stamped with
    /// `expected`, and (when `require_empty`) holds zero lines — all three
    /// evaluated inside the `remove_if` predicate, i.e. under the same
    /// shard write-lock as the removal. On removal the seal tombstone is
    /// cleared too (mirrors `discard`). Returns whether an entry was
    /// removed.
    ///
    /// Used by the flusher for entries it concludes are reapable orphan
    /// residue. `require_empty = true` callers — `flush_final`'s
    /// out-of-tenure drop arm, its `finalize_guard_error` / `pre_drain`
    /// post-await re-checks, and the periodic empty-snapshot reap in
    /// `upload_and_record` — hold no evidence about the stored row, so
    /// they only ever remove the empty no-reaper shape.
    /// `require_empty = false` callers — the `already_finalized_refusal`
    /// post-await re-check and the periodic refused-UPSERT reap in
    /// `upload_and_record` — have already proven the execution's `drv_logs`
    /// row is finalized (the in-hand guard row / the frozen-row UPSERT
    /// refusal), which is what makes removing retained lines safe: the
    /// durable record supersedes them. The competing writer is
    /// the actor task's `set_exec` same-exec restamp, which adopts the
    /// entry as the live execution's carrier and clears the seal while
    /// holding the same entry's lock — so the predicate sees either the
    /// pre-restamp state (still sealed → removal proceeds, and the restamp
    /// then recreates a fresh stamped, unsealed entry via
    /// `entry().or_default()`) or the post-restamp state (seal cleared →
    /// no removal, live carrier preserved). A third interleaving exists:
    /// the restamp can land between the removal and this method's trailing
    /// `sealed.remove`, recreating the entry while the stale seal is still
    /// present for a few flusher instructions (late `push_for` calls drop
    /// batches in that window) — the trailing remove then clears it, same
    /// shape as [`Self::discard_if_empty_for_exec`]. What the predicate
    /// guarantees is only that the removed entry was sealed (+ empty when
    /// required) and stamped with `expected` *at removal time*. The removed
    /// entry is either a true prior-tenure orphan (terminal persisted under
    /// the old tenure, no reaper left) or an entry whose own in-tenure
    /// final is still pending (the current tenure's epilogue re-sealed it
    /// after a same-exec restamp). Reaping the latter is benign: an empty
    /// one only loses the empty drain's status stamp, and a non-empty one
    /// can only be removed by a `require_empty = false` caller — i.e. with
    /// the execution's row already finalized — so its lines were
    /// unpersistable and the pending final's own already-finalized arm
    /// would have drained and discarded them anyway (it then resolves via
    /// the no-entry arm; the call sites carry the full safety argument). It
    /// must NOT be read as "sealed ⇒ orphan".
    ///
    /// Lock order matches every other caller (buffers entry/shard lock,
    /// then `sealed`); no path holds a `sealed` guard while acquiring
    /// `buffers`, so reading the seal inside the predicate cannot deadlock.
    pub(crate) fn discard_if_sealed_for_exec(
        &self,
        drv_path: &str,
        expected: Uuid,
        require_empty: bool,
    ) -> bool {
        let key = drv_log_hash(drv_path);
        let removed = self
            .buffers
            .remove_if(&key, |_, e| {
                self.sealed.contains(&key)
                    && e.exec_id == Some(expected)
                    && (!require_empty || e.lines.is_empty())
            })
            .is_some();
        if removed {
            self.sealed.remove(&key);
        }
        removed
    }

    /// Push a batch with `(executor, drv)` binding enforcement.
    /// r[impl sched.log.batch-binding]
    ///
    /// Returns `true` if the batch was accepted into the ring buffer.
    /// Returns `false` when:
    /// - the entry is sealed (terminal completion already fired) — silent
    ///   drop, mirrors [`Self::push`]'s seal check. Not counted: the seal
    ///   check is drv-keyed and runs *before* the binding gate, so the
    ///   typical hit is a benign late batch from the assigned executor;
    ///   a wrong-executor batch for a sealed key is also dropped
    ///   uncounted (the buffer is gone or about to be — there is nothing
    ///   to pollute, and counting would noise the binding-violation
    ///   signal with normal completion timing), or
    /// - the binding gate rejects it: no entry exists for `drv_path`
    ///   (`reason="no_assignment"` — unsolicited drv; does NOT create an
    ///   entry, unlike legacy [`Self::push`]'s `or_default()`, which is
    ///   itself the threat: a fabricated `derivation_path` should not
    ///   allocate a fresh buffer), the entry was never `set_exec`'d
    ///   (`reason="unstamped"`), or the entry's `assigned_executor` does
    ///   not match `executor` (`reason="executor_mismatch"`). Each
    ///   binding-gate reject increments
    ///   `rio_scheduler_log_batches_rejected_total` and emits a `debug!`, or
    /// - the line-numbering gate rejects it: the batch starts at or below
    ///   the ring's highest stored line number (`reason="non_monotonic"`),
    ///   or `first_line_number + lines.len()` would overflow u64
    ///   (`reason="line_number_overflow"`). Same counter, same `debug!`
    ///   discipline. This is what makes the ring's documented
    ///   monotone-numbering invariant (`truncate_below`, `read_since`)
    ///   enforced rather than assumed; upward gaps of any size remain
    ///   accepted.
    ///
    /// **The caller MUST gate any sibling consumer on the return value.**
    /// `r[sched.log.batch-binding]` requires the *ingestion path* to drop
    /// rejected batches — that includes the recv task's gateway forward,
    /// not just the ring buffer write. A `false` return means the batch
    /// is unverified worker input and must not be fanned out.
    ///
    /// The completion path's analogous check
    /// (`sched.completion.output-membership`, `completion.rs`) runs
    /// inside the actor. This runs in the recv task, which deliberately
    /// bypasses the actor (see module header comment) — so the check is
    /// colocated with the data the recv task has: the ring buffer entry,
    /// stamped by [`Self::set_exec`] at dispatch.
    ///
    /// Rejection is per-batch, not per-stream — a single bad batch must
    /// not tear down a stream carrying other valid drvs.
    ///
    /// The check and the write happen under a single `get_mut()` write
    /// lock: there is no window between "verify the executor" and
    /// "append the lines" for a concurrent `discard()` + `set_exec()`
    /// re-dispatch to swap the entry under us. (A check-then-`drop`-
    /// then-`push()` shape would re-`or_default()` a freshly re-stamped
    /// entry and land the old executor's lines in the new exec's buffer
    /// — exactly the cross-executor pollution this gate exists to stop.)
    #[must_use = "the caller must gate sibling consumers (gateway forward) on acceptance"]
    pub fn push_for(&self, drv_path: &str, batch: &BuildLogBatch, executor: &str) -> bool {
        let key = drv_log_hash(drv_path);
        if self.sealed.contains(&key) {
            // Late batch after terminal; matches push()'s seal check.
            return false;
        }
        // debug!, not warn!, on all reject arms below — same shape as
        // handle_forward_phase (event.rs) and ProcessCompletion's stale-report
        // guard. Rejected paths bypass the seen_drvs cap (executor_service.rs),
        // so a 100%-rejected stream fires this unbounded; the metric covers
        // attack detection without log noise.
        let Some(mut entry) = self.buffers.get_mut(&key) else {
            tracing::debug!(drv = %drv_path, executor, "rejected log batch: no active assignment");
            metrics::counter!(
                "rio_scheduler_log_batches_rejected_total",
                "reason" => "no_assignment"
            )
            .increment(1);
            return false;
        };
        if entry.assigned_executor.is_none() {
            // Entry created by legacy `push()` (test-only path) — never
            // stamped with an executor. Reject under its own label so
            // dashboards can distinguish "test fixture wired wrong" from
            // a real cross-executor probe.
            tracing::debug!(drv = %drv_path, executor, "rejected log batch: entry unstamped (no set_exec)");
            metrics::counter!(
                "rio_scheduler_log_batches_rejected_total",
                "reason" => "unstamped"
            )
            .increment(1);
            return false;
        }
        if entry.assigned_executor.as_deref() != Some(executor) {
            tracing::debug!(
                drv = %drv_path, executor, assigned = ?entry.assigned_executor,
                "rejected log batch: executor mismatch"
            );
            metrics::counter!(
                "rio_scheduler_log_batches_rejected_total",
                "reason" => "executor_mismatch"
            )
            .increment(1);
            return false;
        }
        // Worker-supplied line numbering. The LogBatcher contract is strictly
        // monotone numbering per execution (header banner once at line 0,
        // daemon-transient retries reseed at the prior attempt's
        // final_line_count, reconnects resume after the last delivered line —
        // see rio-builder log_stream.rs / executor mod.rs, bug_013), so a
        // batch that would wrap u64 numbering or that starts at/below the
        // ring's highest stored line number is malformed worker input. Reject
        // the batch — do NOT renumber it: the field's only purpose is
        // ordering, and every downstream consumer (read_since's monotone
        // scan, truncate_below's front-truncation, the flusher's span and
        // stored-coverage subsumption arithmetic, drv_logs.line_count) relies
        // on ring numbering being monotone. Per-batch, like the binding arms
        // above: an in-order batch after a rejected one is accepted again.
        // Scope: the comparison is against the CURRENT ring contents only
        // (it resets when the ring empties — drain, failover truncate_below,
        // discard), and absolute magnitude is deliberately unbounded —
        // upward gaps of any size stay accepted (interior holes are
        // legitimate, obs.log.gap-span). Whatever passes here is made
        // harmless at the recording site: the flusher's span computation is
        // total and the drv_logs numeric binds clamp at i64::MAX.
        // r[impl sched.executor.input-bounds+2]
        if !batch.lines.is_empty() {
            if batch
                .first_line_number
                .checked_add(batch.lines.len() as u64)
                .is_none()
            {
                tracing::debug!(
                    drv = %drv_path, executor,
                    first_line_number = batch.first_line_number,
                    lines = batch.lines.len(),
                    "rejected log batch: line numbering would overflow u64"
                );
                metrics::counter!(
                    "rio_scheduler_log_batches_rejected_total",
                    "reason" => "line_number_overflow"
                )
                .increment(1);
                return false;
            }
            if let Some((last_line, _)) = entry.lines.back()
                && batch.first_line_number <= *last_line
            {
                tracing::debug!(
                    drv = %drv_path, executor,
                    first_line_number = batch.first_line_number,
                    ring_last_line = last_line,
                    "rejected log batch: non-monotone first_line_number \
                     (ring already holds an equal or higher line number)"
                );
                metrics::counter!(
                    "rio_scheduler_log_batches_rejected_total",
                    "reason" => "non_monotonic"
                )
                .increment(1);
                return false;
            }
        }
        // Same write-lock guard for check AND write — no TOCTOU window.
        Self::push_into(&mut entry, batch);
        true
    }

    /// Discard a buffer without returning its contents. Also un-seals.
    ///
    /// Called via the actor wrapper `DagActor::discard_log_buffer`
    /// (dispatch and rollback — see its rustdoc for
    /// the caller list and per-path rationale), and directly by
    /// `handle_cleanup_terminal_build` for each reaped DAG node (bounds
    /// a dropped-FlushRequest leak to `TERMINAL_CLEANUP_DELAY`, ~60s;
    /// entries whose deferred final flush is retained by the flusher are
    /// skipped by that caller — see `handle_cleanup_terminal_build`;
    /// zero-line entries whose terminal FlushRequest could not be
    /// enqueued are reaped earlier, at the epilogue, via
    /// [`Self::discard_if_empty`]) and
    /// `tick_process_expired_poisons` for never-re-dispatched poisoned
    /// drvs (defense-in-depth against a slow leak), and by
    /// `discard_unsealed_not_in` (acquisition-time sweep — a `pub(crate)`
    /// sibling, so plain backticks rather than an intra-doc link).
    /// Idempotent; no-op on a missing entry.
    pub fn discard(&self, drv_path: &str) {
        let key = drv_log_hash(drv_path);
        self.buffers.remove(&key);
        self.sealed.remove(&key);
    }

    /// Remove the entry for `drv_path` **only if** it currently holds zero
    /// lines. Returns whether an entry was removed; on removal also clears
    /// the seal tombstone (mirrors [`Self::discard`]) so `sealed` stays
    /// bounded. If no entry exists, nothing is removed and an existing
    /// seal tombstone is deliberately left in place (it still guards
    /// against a late batch; `CleanupTerminalBuild` bounds it, exactly as
    /// today) — this method only reverses the entry+tombstone pair it
    /// actually reaps.
    ///
    /// Called by the actor's `terminal_log_epilogue` when the completion
    /// `FlushRequest` could not be enqueued (flush channel full, flusher
    /// task dead, or no flusher configured): the flusher's `drain_if_exec`
    /// will never remove the entry, the periodic snapshot skips zero-line
    /// entries, and the drv is terminal so no re-dispatch `discard` is
    /// coming — left in place the dead carrier would just sit in memory
    /// until `CleanupTerminalBuild`. Reads do not depend on the reap:
    /// `GetDerivationLogs` probes the stored side (e.g. the ex-leader's
    /// `.partial` from the fresh-standby recovery-restamp shape of
    /// `adopt_orphan_completion`) whenever the entry it finds holds zero
    /// lines, so removing the entry is bookkeeping hygiene. A non-empty
    /// entry is deliberately left alone: its lines are still wanted (the
    /// periodic flush keeps snapshotting them while a flusher exists;
    /// `CleanupTerminalBuild` discards at build cleanup).
    ///
    /// Atomicity: the emptiness check and the removal happen together
    /// under `remove_if`'s per-shard lock. A `push_for` that passed the
    /// seal check before the caller's seal landed either wins the race
    /// (entry now non-empty → kept, lines preserved) or loses it (entry
    /// gone → the batch is rejected as unassigned) — the seal alone does
    /// not provide this guarantee; `remove_if` does.
    pub fn discard_if_empty(&self, drv_path: &str) -> bool {
        let key = drv_log_hash(drv_path);
        let removed = self
            .buffers
            .remove_if(&key, |_, e| e.lines.is_empty())
            .is_some();
        if removed {
            self.sealed.remove(&key);
        }
        removed
    }

    /// Drop every buffered line whose worker-assigned line number is below
    /// `threshold`, only if the entry is stamped with `exec_id`. Returns
    /// `(dropped_lines, dropped_bytes)`.
    ///
    /// Called by the flusher when a stored `drv_logs` row for this
    /// execution ends at `threshold`: a re-acquired ex-leader's retained
    /// ring can overlap the stored coverage, have an interior hole inside
    /// it, or even hold a head that precedes the stored range entirely. The
    /// stored blob cannot be sliced and the merge supports exactly one
    /// prefix ahead of the ring, so the ring yields EVERYTHING below the
    /// stored end — overlapping lines are superseded by the durable copy
    /// (no loss), and a non-overlapping head below the stored range is
    /// dropped (bounded by the ≤30s unflushed-tail budget the spec already
    /// grants per failover; see the flusher's `reconcile_stored_prefix`).
    ///
    /// Live `read_since` serving of the dropped head ends here — the same
    /// post-failover degradation a fresh leader's suffix-only ring already
    /// has; the content (when it was ever flushed) stays readable from the
    /// stored `.partial` once the entry is drained.
    ///
    /// Line numbers in the ring are monotone (batches arrive in order and
    /// the worker never re-streams below what this leader already holds),
    /// so a one-time front-truncation stays valid for the execution.
    pub(crate) fn truncate_below(
        &self,
        drv_path: &str,
        exec_id: Uuid,
        threshold: u64,
    ) -> (u64, u64) {
        let Some(mut buf) = self.buffers.get_mut(&drv_log_hash(drv_path)) else {
            return (0, 0);
        };
        if buf.exec_id != Some(exec_id) {
            return (0, 0);
        }
        let (mut dropped_lines, mut dropped_bytes) = (0u64, 0u64);
        while buf.lines.front().is_some_and(|(n, _)| *n < threshold) {
            let (_, l) = buf.lines.pop_front().expect("front checked above");
            dropped_lines += 1;
            dropped_bytes += l.len() as u64;
            buf.bytes -= l.len();
        }
        (dropped_lines, dropped_bytes)
    }

    /// Discard the buffer for `drv_path` **only if** it is currently
    /// stamped to `executor` and not sealed. Returns whether anything
    /// was removed.
    ///
    /// Called by the actor's `handle_executor_disconnected` cleanup for
    /// paths the disconnecting executor's stream touched but the DAG no
    /// longer recognizes (`dag.hash_for_path()` is `None`). The ownership
    /// check is the load-bearing part: `hash_for_path` is an exact-string
    /// lookup on the full canonical path, while [`drv_log_hash`] (which
    /// keys this map) accepts a fabricated suffix or a bare hash. A
    /// compromised worker can therefore choose a `derivation_path` that
    /// fails the DAG gate but normalizes to a *victim's* buffer key. This
    /// method refuses to remove an entry stamped to anyone else
    /// (cross-tenant DoS) or one that is sealed (the flusher's `FlushRequest`
    /// is in flight and `flush_final` would otherwise drop the request as
    /// stale, silently losing the build log).
    ///
    /// Idempotent; no-op on a missing or unstamped entry.
    ///
    /// ## Concurrency
    ///
    /// The seal check and the `remove_if` are two operations, not one.
    /// This is TOCTOU-safe because `seal()` is only called from actor
    /// handlers (`seal_log_buffer` in `actor/event.rs`) — the same
    /// single-threaded event loop that calls this method from
    /// `handle_executor_disconnected`. There is no concurrent `seal()` to
    /// race. The flusher's `unseal()` calls (`flush.rs`) are concurrent
    /// but each follows a `drain()` (entry already gone) or a stale-exec
    /// early-return (entry gone or restamped) — both outcomes the
    /// `remove_if` ownership check handles independently.
    ///
    /// `remove_if` (rather than `get` + check + `remove`) is used because
    /// it's shorter and harder to misuse if a future caller appears
    /// off-actor — it is *not* load-bearing for atomicity with the
    /// current actor-only caller.
    pub fn discard_if_owned_by(&self, drv_path: &str, executor: &str) -> bool {
        let key = drv_log_hash(drv_path);
        if self.sealed.contains(&key) {
            return false;
        }
        self.buffers
            .remove_if(&key, |_, e| {
                e.assigned_executor.as_deref() == Some(executor)
            })
            .is_some()
    }

    /// Discard every **unsealed** entry whose key is not in `live_keys`
    /// (keys in [`drv_log_hash`] form). Returns how many were discarded.
    ///
    /// Recovery-only sweep, called via the actor's
    /// `sweep_stale_log_buffers` right after the DAG is rebuilt from PG on
    /// lease acquisition: an ex-leader's `LogBuffers` is retained across
    /// the flap (`clear_persisted_state`), and the restamp loop only
    /// covers PG-`Assigned|Running` drvs — an entry whose drv went
    /// terminal under an interim leader (absent from the rebuilt DAG, or
    /// present only as a Poisoned TTL-tracking node and therefore excluded
    /// from `live_keys`) would otherwise shadow that execution's stored
    /// log in `GetDerivationLogs` (the ring buffer is probed before S3)
    /// and be re-uploaded as a stale `.partial` by every periodic flush
    /// for the process lifetime.
    ///
    /// Sealed entries are skipped: a seal marks a terminal observed by
    /// THIS process whose final `FlushRequest` may still be queued — the
    /// flusher's `drain_if_exec` owns that entry's removal. If that
    /// request was instead dropped at enqueue (channel full) and the
    /// lease flapped before the build's cleanup ran, the sealed entry
    /// lingers until process restart — accepted residual: at acquisition
    /// we cannot tell a queued request from a dropped one, and discarding
    /// sealed entries would lose a queued final flush.
    ///
    /// Keys are snapshotted first and discarded after — `DashMap` iterators
    /// hold shard locks, so removing while iterating the same map would
    /// deadlock.
    pub(crate) fn discard_unsealed_not_in(
        &self,
        live_keys: &std::collections::HashSet<String>,
    ) -> usize {
        let stale: Vec<(String, Option<Uuid>, usize, usize)> = self
            .buffers
            .iter()
            .filter(|e| !live_keys.contains(e.key()) && !self.sealed.contains(e.key()))
            .map(|e| {
                (
                    e.key().clone(),
                    e.value().exec_id,
                    e.value().lines.len(),
                    e.value().bytes,
                )
            })
            .collect();
        for (key, exec_id, lines, bytes) in &stale {
            // info!, not debug!: at most once per drv per re-acquisition,
            // and it is the only trace an operator gets that this
            // execution's unflushed tail was dropped (mirrors set_exec's
            // cross-exec restamp log).
            tracing::info!(
                drv = %key,
                exec_id = ?exec_id,
                dropped_lines = lines,
                dropped_bytes = bytes,
                "discarding retained log buffer for a derivation not live after recovery"
            );
            self.discard(key);
        }
        stale.len()
    }

    /// Re-arm the once-per-tenure stored-coverage reconciliation on every
    /// retained entry: clear `recovered_prefix` and the `prefix_checked`
    /// latch so the flusher re-consults the stored `drv_logs` row on each
    /// execution's next non-empty flush (`reconcile_stored_prefix`).
    ///
    /// Recovery-only, called from `recover_from_pg` at lease acquisition,
    /// before the PG loads. Clears EVERY retained entry — including the
    /// PG-`Assigned|Running` ones recovery subsequently restamps, for
    /// which the same-exec arm of [`Self::set_exec`] performs a redundant
    /// second clear (that arm remains the general contract for any
    /// restamp, recovery-driven or not). The entries that NEED this call
    /// are the ones recovery does not restamp: entries the acquisition
    /// sweep spares because their drv is non-terminal in some other state
    /// (Ready/Queued/Substituting after an interim leader's reset), and
    /// sealed entries whose final `FlushRequest` is still queued. Their
    /// latches encode conclusions reached under a previous tenure; an
    /// interim leader may have extended the stored row past what this
    /// ring holds, and trusting the stale latch would let the next flush
    /// overwrite that durable coverage — or freeze a truncated final and
    /// delete the `.partial` that is its only copy.
    ///
    /// Lines, exec stamps, executor bindings, and seal tombstones are
    /// untouched — only the per-tenure reconciliation state is cleared.
    /// Cost: at most one point-SELECT (plus one S3 GET when stored
    /// coverage is not subsumed) per retained entry on its next non-empty
    /// flush — the same once-per-tenure cost the restamp arm already
    /// imposes on Assigned/Running entries. Idempotent.
    ///
    /// Benign race: a final flush already in flight from the prior
    /// tenure's queue may have its just-set latch/cached prefix cleared
    /// between its reconcile and its pre-drain read; for sealed entries
    /// no interim leader can have extended the stored row, so that
    /// collapses to the accepted same-tenure `Checked` head-loss shape
    /// (or a conservatively preserved `.partial`) — never an overwrite
    /// of stored coverage.
    ///
    /// Returns how many entries had a latch or cached prefix to clear
    /// (for the acquisition log line).
    // r[impl obs.log.stored-coverage-preserved]
    pub(crate) fn rearm_prefix_reconciliation(&self) -> usize {
        let mut rearmed = 0usize;
        for mut entry in self.buffers.iter_mut() {
            if entry.prefix_checked || entry.recovered_prefix.is_some() {
                entry.prefix_checked = false;
                entry.recovered_prefix = None;
                rearmed += 1;
            }
        }
        rearmed
    }

    /// Mark `drv_path` terminal: subsequent [`Self::push`] calls drop.
    ///
    /// Called by the actor's `terminal_log_epilogue` (via `seal_log_buffer`)
    /// BEFORE `trigger_log_flush`; see `terminal_log_epilogue`'s rustdoc for
    /// the terminal-path caller list. The flusher's [`Self::drain`] still
    /// owns buffer removal (except a zero-line entry whose terminal
    /// FlushRequest could not be enqueued — the epilogue reaps that via
    /// [`Self::discard_if_empty`]) — sealing only prevents post-drain
    /// recreation by a late batch. Any buffer present at seal time is left
    /// for the flusher; sealing then draining yields the same contents as
    /// draining alone.
    ///
    /// Idempotent. Retry / re-dispatch un-seals via [`Self::unseal`] (or
    /// [`Self::discard`], which also un-seals); any [`Self::set_exec`]
    /// restamp clears it too — cross-exec because the seal belongs to the
    /// execution being replaced, same-exec because at lease re-acquisition
    /// the prior tenure's pending final can no longer drain the entry.
    pub fn seal(&self, drv_path: &str) {
        self.sealed.insert(drv_log_hash(drv_path));
    }

    /// Reverse [`Self::seal`]: re-open `drv_path` for pushes. Called by
    /// `LogFlusher::flush_final`'s resolution arms (post-drain,
    /// no-entry, already-finalized residue reap) and by the
    /// deferred-final retention-cap overflow drop to bound `sealed`.
    /// [`Self::discard`]-family reaps and [`Self::set_exec`] restamps
    /// (cross- and same-exec) clear the seal directly rather than through
    /// this method. Idempotent; no-op if not sealed.
    pub fn unseal(&self, drv_path: &str) {
        self.sealed.remove(&drv_log_hash(drv_path));
    }

    /// Whether `drv_path` is currently sealed (a completion landed and
    /// the flusher owns drain). Retained for the flusher contract +
    /// tests; the stream-exit cleanup no longer branches on this — it
    /// was moved into the actor's epoch-gated `ExecutorDisconnected`
    /// handler (the unsynchronized `is_sealed` branch here raced the
    /// actor's `seal()` under load).
    pub fn is_sealed(&self, drv_path: &str) -> bool {
        self.sealed.contains(&drv_log_hash(drv_path))
    }

    /// Number of active buffers. For metrics + flusher periodic-scan skip.
    pub fn active_count(&self) -> usize {
        self.buffers.len()
    }

    /// Number of sealed (tombstoned) drv_paths. Should hover near
    /// `active_count()` in steady state; unbounded growth = leak.
    pub fn sealed_count(&self) -> usize {
        self.sealed.len()
    }

    /// Snapshot all currently-buffered drv_paths (keys only, no lines).
    ///
    /// For the periodic flush. Snapshotting keys first (under DashMap's
    /// per-shard read locks) then draining each (under per-key write lock)
    /// avoids holding a shard lock across the slow S3 PUT. If a new
    /// drv_path starts buffering between the snapshot and the drain, it
    /// gets picked up on the NEXT periodic tick — no correctness issue.
    pub(crate) fn active_keys(&self) -> Vec<String> {
        self.buffers.iter().map(|e| e.key().clone()).collect()
    }

    /// Non-consuming clone of a buffer's contents (for periodic snapshot flush).
    ///
    /// Unlike `drain`, this does NOT remove the buffer — the derivation is
    /// still running, live serving via the ring buffer must continue, and
    /// the on-completion flush will drain+upload the final state. This
    /// means periodic snapshots upload an ever-growing prefix of the same
    /// log (wasteful in S3 PUTs, but bounded: at most one per 30s per active
    /// derivation, and the spec explicitly accepts that tradeoff at
    /// `observability.typ`).
    ///
    /// Returns `(first_line, last_line, line_count, total_bytes, lines)`;
    /// `last_line` is the highest worker-assigned line number present — it
    /// exceeds `first_line + line_count − 1` exactly when the payload
    /// carries an interior hole (`obs.log.gap-span`).
    #[allow(
        clippy::type_complexity,
        reason = "same shape as drain_if_exec — see the allow there"
    )]
    pub(crate) fn snapshot(&self, drv_path: &str) -> Option<(u64, u64, u64, u64, Vec<Vec<u8>>)> {
        let buf = self.buffers.get(&drv_log_hash(drv_path))?;
        let first_line = buf.lines.front().map(|(n, _)| *n).unwrap_or(0);
        let last_line = buf.lines.back().map(|(n, _)| *n).unwrap_or(0);
        let line_count = buf.lines.len() as u64;
        let total_bytes = buf.bytes as u64;
        let lines: Vec<Vec<u8>> = buf.lines.iter().map(|(_n, bytes)| bytes.clone()).collect();
        Some((first_line, last_line, line_count, total_bytes, lines))
    }

    /// Line-number span of the entry for `drv_path` IF it is stamped with
    /// `exec_id`: `(first_line, last_line, line_count)`. `None` when the
    /// entry is missing, unstamped, or stamped with a different execution.
    /// `line_count == 0` ⇒ first/last are meaningless zeros.
    ///
    /// Used by the flusher's stored-coverage reconciliation to decide
    /// whether the ring contiguously subsumes what a prior tenure already
    /// flushed for this execution — `snapshot()` deliberately drops
    /// per-line numbers, and an interior hole (lines delivered only to an
    /// interim leader) is invisible without the span. Also used by the
    /// admin read path's empty-entry fallthrough (`line_count == 0` ⇒ the
    /// ring cannot answer; probe the stored side for the stamped exec).
    pub(crate) fn span(&self, drv_path: &str, exec_id: Uuid) -> Option<(u64, u64, u64)> {
        let buf = self.buffers.get(&drv_log_hash(drv_path))?;
        if buf.exec_id != Some(exec_id) {
            return None;
        }
        let first = buf.lines.front().map(|(n, _)| *n).unwrap_or(0);
        let last = buf.lines.back().map(|(n, _)| *n).unwrap_or(0);
        Some((first, last, buf.lines.len() as u64))
    }
}

impl Default for LogBuffers {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mk_batch(drv_path: &str, first_line: u64, lines: &[&[u8]]) -> BuildLogBatch {
        BuildLogBatch {
            derivation_path: drv_path.to_string(),
            lines: lines.iter().map(|l| l.to_vec()).collect(),
            first_line_number: first_line,
            executor_id: "test-worker".into(),
        }
    }

    #[test]
    fn log_s3_key_layout() {
        let exec = Uuid::nil();
        let drv = "/nix/store/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-hello.drv";
        assert_eq!(
            log_s3_key(drv, &exec, false),
            format!("logs/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2/{exec}.log.zst"),
        );
        assert_eq!(
            log_s3_key(drv, &exec, true),
            format!("logs/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2/{exec}.partial.log.zst"),
        );
        // Idempotent on a basename and a bare hash too — same normalizer
        // as the rest of LogBuffers.
        assert_eq!(
            log_s3_key("amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2-hello.drv", &exec, false),
            format!("logs/amnhr5p1w6gmjb7bynh7vxdfjs8x3kr2/{exec}.log.zst"),
        );
    }

    #[test]
    fn push_then_read_since_returns_all() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"line0", b"line1", b"line2"]));

        let lines = bufs.read_since("drv-a", 0).unwrap();
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0], (0, b"line0".to_vec()));
        assert_eq!(lines[1], (1, b"line1".to_vec()));
        assert_eq!(lines[2], (2, b"line2".to_vec()));
    }

    #[test]
    fn push_twice_appends() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0"]));
        bufs.push(&mk_batch("drv-a", 1, &[b"l1"]));
        assert_eq!(bufs.read_since("drv-a", 0).unwrap().len(), 2);
    }

    #[test]
    fn read_since_filters_by_line_number() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0", b"l1", b"l2", b"l3", b"l4"]));
        let lines = bufs.read_since("drv-a", 3).unwrap();
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].0, 3);
        assert_eq!(lines[1].0, 4);
    }

    #[test]
    fn read_since_nonexistent_buffer_is_none() {
        let bufs = LogBuffers::new();
        assert!(
            bufs.read_since("not-there", 0).is_none(),
            "absent buffer → None (not Some(empty)) so callers can \
             distinguish 'try S3' from 'caught up, re-poll'"
        );
    }

    #[test]
    fn read_since_caught_up_is_some_empty() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0", b"l1", b"l2"]));
        let lines = bufs.read_since("drv-a", 3).unwrap();
        assert!(
            lines.is_empty(),
            "buffer present, since ≥ newest → Some(empty) (caller re-polls)"
        );
    }

    #[test]
    fn ring_eviction_drops_oldest() {
        let bufs = LogBuffers::new();
        // Fill to capacity.
        let lines: Vec<Vec<u8>> = (0..RING_CAPACITY).map(|i| format!("l{i}").into()).collect();
        let line_refs: Vec<&[u8]> = lines.iter().map(|v| v.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", 0, &line_refs));
        assert_eq!(bufs.read_since("drv-a", 0).unwrap().len(), RING_CAPACITY);

        // Push 100 more → oldest 100 evicted.
        let extra: Vec<Vec<u8>> = (0..100).map(|i| format!("x{i}").into()).collect();
        let extra_refs: Vec<&[u8]> = extra.iter().map(|v| v.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", RING_CAPACITY as u64, &extra_refs));

        let all = bufs.read_since("drv-a", 0).unwrap();
        assert_eq!(all.len(), RING_CAPACITY, "still at capacity after eviction");
        // The FIRST line number present should be 100 (lines 0-99 evicted).
        assert_eq!(all[0].0, 100, "oldest 100 should be evicted");
        // The LAST line should be the last extra line.
        assert_eq!(all.last().unwrap().0, RING_CAPACITY as u64 + 99);
    }

    #[test]
    fn single_batch_larger_than_capacity_keeps_tail() {
        // Edge case: one giant batch > RING_CAPACITY. We want the TAIL kept
        // (most recent lines), not an empty buffer.
        let bufs = LogBuffers::new();
        let big = RING_CAPACITY + 50;
        let lines: Vec<Vec<u8>> = (0..big).map(|i| vec![i as u8]).collect();
        let line_refs: Vec<&[u8]> = lines.iter().map(|v| v.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", 0, &line_refs));

        let all = bufs.read_since("drv-a", 0).unwrap();
        assert_eq!(all.len(), RING_CAPACITY);
        assert_eq!(all[0].0, 50, "first 50 evicted, kept tail");
        assert_eq!(all.last().unwrap().0, big as u64 - 1);
    }

    #[test]
    fn drain_removes_entry_and_returns_all() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"hello", b"world"]));
        assert_eq!(bufs.active_count(), 1);

        let (first, count, bytes, lines) = bufs.drain("drv-a").expect("buffer exists");
        assert_eq!(first, 0, "no eviction → first_line=0");
        assert_eq!(count, 2);
        assert_eq!(bytes, 5 + 5);
        assert_eq!(lines, vec![b"hello".to_vec(), b"world".to_vec()]);
        assert_eq!(bufs.active_count(), 0, "drain removed the entry");
        assert!(bufs.drain("drv-a").is_none(), "second drain returns None");
    }

    /// Regression for bug_084: after ring eviction, `drain` must return
    /// the FIRST surviving line's true number — NOT zero, NOT line_count.
    /// This is the offset persisted in `drv_logs.first_line` so the
    /// S3 read path stays in true-line-number space.
    #[test]
    fn drain_after_eviction_returns_first_surviving_line_number() {
        let bufs = LogBuffers::new();
        let lines: Vec<Vec<u8>> = (0..RING_CAPACITY).map(|i| vec![i as u8]).collect();
        let line_refs: Vec<&[u8]> = lines.iter().map(|v| v.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", 0, &line_refs));
        // 50 more → first 50 evicted; survivors are true lines [50..100050).
        let extra: Vec<Vec<u8>> = (0..50).map(|i| vec![i as u8]).collect();
        let extra_refs: Vec<&[u8]> = extra.iter().map(|v| v.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", RING_CAPACITY as u64, &extra_refs));

        let (first, count, _bytes, _lines) = bufs.drain("drv-a").unwrap();
        assert_eq!(first, 50, "first surviving true line number");
        assert_eq!(count, RING_CAPACITY as u64, "survivor count (capped)");
    }

    #[test]
    fn drain_nonexistent_returns_none() {
        let bufs = LogBuffers::new();
        assert!(bufs.drain("not-there").is_none());
    }

    // ── drain_if_exec: atomic stale-request guard (bug_004, round 8) ──

    #[test]
    fn drain_if_exec_matching_exec_drains() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        bufs.set_exec("drv-a", exec, "executor-1");
        assert!(bufs.push_for(
            "drv-a",
            &mk_batch("drv-a", 0, &[b"hello", b"world"]),
            "executor-1"
        ));

        let (first, last, count, bytes, lines) = bufs
            .drain_if_exec("drv-a", exec)
            .expect("matching exec must drain");
        assert_eq!(first, 0);
        assert_eq!(last, 1);
        assert_eq!(count, 2);
        assert_eq!(bytes, 5 + 5);
        assert_eq!(lines, vec![b"hello".to_vec(), b"world".to_vec()]);
        assert_eq!(bufs.active_count(), 0, "drain removed the entry");
    }

    /// The load-bearing half of the TOCTOU fix: a drain pinned to a stale
    /// exec_id must refuse AND leave the live entry (lines and stamp)
    /// untouched. The old read-compare-`drain()` shape removed the live
    /// entry here and returned its (empty) contents.
    #[test]
    fn drain_if_exec_mismatched_exec_leaves_entry_intact() {
        let bufs = LogBuffers::new();
        let stale = Uuid::now_v7();
        let live = Uuid::now_v7();
        bufs.set_exec("drv-a", live, "executor-2");
        assert!(bufs.push_for(
            "drv-a",
            &mk_batch("drv-a", 0, &[b"live-line"]),
            "executor-2"
        ));

        assert!(
            bufs.drain_if_exec("drv-a", stale).is_none(),
            "mismatched exec must refuse to drain"
        );
        assert_eq!(
            bufs.exec_id("drv-a"),
            Some(live),
            "live entry's stamp must survive the refused drain"
        );
        assert_eq!(
            bufs.read_since("drv-a", 0).map(|v| v.len()),
            Some(1),
            "live entry's lines must survive the refused drain"
        );
        // And the live exec can still drain its own buffer afterwards.
        let (_, _, count, _, _) = bufs.drain_if_exec("drv-a", live).expect("live exec drains");
        assert_eq!(count, 1);
    }

    #[test]
    fn drain_if_exec_missing_entry_returns_none() {
        let bufs = LogBuffers::new();
        assert!(bufs.drain_if_exec("not-there", Uuid::now_v7()).is_none());
    }

    /// An entry created by the legacy `push()` (no `set_exec`) has
    /// `exec_id == None` and can never match a request's pinned exec.
    #[test]
    fn drain_if_exec_unstamped_entry_returns_none() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"line"]));
        assert!(bufs.drain_if_exec("drv-a", Uuid::now_v7()).is_none());
        assert_eq!(bufs.active_count(), 1, "unstamped entry left in place");
    }

    /// A worker that reconnects after an interim-leader tenure re-streams only
    /// undelivered batches, so a retained ring can carry an interior hole.
    /// snapshot/drain must report the highest line actually present so the
    /// flusher can record the execution's true span, not just the physical count.
    #[test]
    fn snapshot_and_drain_if_exec_report_last_line_across_interior_hole() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        bufs.set_exec("drv-a", exec, "test-worker");
        bufs.push(&mk_batch("drv-a", 0, &[b"l0", b"l1", b"l2"]));
        bufs.push(&mk_batch("drv-a", 6, &[b"l6", b"l7"]));

        let (first, last, count, _bytes, lines) = bufs.snapshot("drv-a").unwrap();
        assert_eq!((first, last, count), (0, 7, 5), "snapshot spans the hole");
        assert_eq!(lines.len(), 5);

        let (first, last, count, _bytes, lines) = bufs
            .drain_if_exec("drv-a", exec)
            .expect("stamped entry drains");
        assert_eq!((first, last, count), (0, 7, 5), "drain spans the hole");
        assert_eq!(lines.len(), 5);
    }

    #[test]
    fn discard_removes_without_returning() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"line"]));
        bufs.discard("drv-a");
        assert_eq!(bufs.active_count(), 0);
        assert!(bufs.read_since("drv-a", 0).is_none());
    }

    #[test]
    fn separate_drv_paths_are_independent() {
        // Keys with no `-` so `drv_log_hash` leaves them distinct
        // (`drv-a`/`drv-b` both normalize to `drv`).
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("aaa", 0, &[b"a0"]));
        bufs.push(&mk_batch("bbb", 0, &[b"b0", b"b1"]));

        assert_eq!(bufs.read_since("aaa", 0).unwrap().len(), 1);
        assert_eq!(bufs.read_since("bbb", 0).unwrap().len(), 2);

        bufs.drain("aaa");
        assert_eq!(bufs.read_since("bbb", 0).unwrap().len(), 2, "b untouched");
    }

    /// DashMap's sharded locking should handle concurrent push from
    /// multiple tasks without panicking or losing lines. Different
    /// keys → truly concurrent; same key → serialized by shard lock.
    #[tokio::test]
    async fn concurrent_push_different_keys() {
        let bufs = std::sync::Arc::new(LogBuffers::new());
        let mut handles = Vec::new();
        for i in 0..16 {
            let bufs = bufs.clone();
            handles.push(tokio::spawn(async move {
                // No `-` so `drv_log_hash` leaves keys distinct.
                let drv = format!("drv{i}");
                let five_lines: [&[u8]; 5] = [b"l", b"l", b"l", b"l", b"l"];
                for batch_n in 0..10 {
                    bufs.push(&mk_batch(&drv, batch_n * 5, &five_lines));
                }
            }));
        }
        for h in handles {
            h.await.unwrap();
        }
        assert_eq!(bufs.active_count(), 16);
        for i in 0..16 {
            assert_eq!(
                bufs.read_since(&format!("drv{i}"), 0).unwrap().len(),
                50,
                "drv{i} should have all 50 lines"
            );
        }
    }

    /// Regression: late LogBatch after completion must not recreate a
    /// drained entry. seal() tombstones the path so push() drops; the
    /// flusher's drain() still returns the pre-seal contents.
    #[test]
    fn seal_blocks_late_push_and_preserves_drain() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"line0", b"line1"]));

        // Actor seals on completion (BEFORE flusher drains).
        bufs.seal("drv-a");
        assert_eq!(bufs.sealed_count(), 1);

        // Late batch from the same BuildExecution stream — dropped.
        bufs.push(&mk_batch("drv-a", 2, &[b"late"]));

        // Flusher drains: gets the 2 pre-seal lines (seal did NOT
        // remove the buffer, only tombstoned it).
        let (_first, count, _bytes, lines) = bufs.drain("drv-a").expect("buffer should exist");
        assert_eq!(count, 2);
        assert_eq!(lines, vec![b"line0".to_vec(), b"line1".to_vec()]);

        // Another late batch after drain — still sealed, still dropped.
        // This is the entry-count leak the seal closes: without it,
        // this push would recreate an orphan entry.
        bufs.push(&mk_batch("drv-a", 3, &[b"later"]));
        assert_eq!(
            bufs.active_count(),
            0,
            "sealed path must not recreate entry"
        );
        assert!(bufs.drain("drv-a").is_none());

        // Re-dispatch (poison-clear / retry) un-seals; new worker's
        // pushes land again.
        bufs.unseal("drv-a");
        assert_eq!(bufs.sealed_count(), 0);
        bufs.push(&mk_batch("drv-a", 0, &[b"retry"]));
        assert_eq!(bufs.active_count(), 1);
    }

    /// Regression: `sealed` must be cleared on terminal cleanup.
    /// Before the fix, `seal()` had no production remover — every
    /// completion leaked one String into `sealed` forever.
    #[test]
    fn seal_then_drain_then_unseal_clears_tombstone() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0"]));
        bufs.seal("drv-a");
        let _ = bufs.drain("drv-a");
        // drain() does NOT touch `sealed` — that's the leak shape.
        assert_eq!(bufs.sealed_count(), 1, "drain leaves seal in place");
        // Recv-task / flusher unseal is the bound.
        bufs.unseal("drv-a");
        assert_eq!(bufs.sealed_count(), 0);
        // Unseal re-opened: a fresh push lands.
        bufs.push(&mk_batch("drv-a", 0, &[b"fresh"]));
        assert_eq!(bufs.active_count(), 1);
    }

    /// Regression for merged_bug_128 secondary: transient-failure
    /// reassignment must not concatenate the old worker's partial lines
    /// with the new worker's. `assign_to_worker` now calls `discard()`
    /// before the new worker's first push.
    #[test]
    fn transient_retry_discard_clears_stale_partial() {
        let bufs = LogBuffers::new();
        // Worker W1 pushes 5 lines, then disconnects (transient failure).
        bufs.push(&mk_batch(
            "drv-a",
            0,
            &[b"w1-0", b"w1-1", b"w1-2", b"w1-3", b"w1-4"],
        ));
        // Re-dispatch: actor's assign_to_worker discards.
        bufs.discard("drv-a");
        // Worker W2 pushes from line 0 (fresh attempt).
        bufs.push(&mk_batch("drv-a", 0, &[b"w2-0", b"w2-1", b"w2-2"]));
        // Final flush drains: must be exactly W2's 3 lines, not 8.
        let (_first, count, _bytes, lines) = bufs.drain("drv-a").expect("buffer exists");
        assert_eq!(count, 3, "stale W1 partial must be gone");
        assert_eq!(
            lines,
            vec![b"w2-0".to_vec(), b"w2-1".to_vec(), b"w2-2".to_vec()]
        );
    }

    /// Regression for bug_241: a dropped FlushRequest (channel-full burst)
    /// leaves the buffer in place; before the fix nothing ever removed it
    /// (perpetual 30s S3 PUTs + ~10MiB held). `CleanupTerminalBuild` now
    /// discards reaped nodes' buffers.
    #[test]
    fn dropped_flush_request_buffer_reaped_by_cleanup_discard() {
        let bufs = LogBuffers::new();
        bufs.push(&mk_batch("drv-a", 0, &[b"l0", b"l1"]));
        // Actor seals on completion, then try_send fails (channel full) —
        // flush_final never runs. Buffer is still present + sealed.
        bufs.seal("drv-a");
        assert_eq!(bufs.active_count(), 1, "dropped request leaves buffer");
        assert_eq!(bufs.sealed_count(), 1);
        // after TERMINAL_CLEANUP_DELAY (~60s): CleanupTerminalBuild discards.
        bufs.discard("drv-a");
        assert_eq!(bufs.active_count(), 0, "cleanup discard frees buffer");
        assert_eq!(bufs.sealed_count(), 0, "cleanup discard also unseals");
    }

    /// bug_008 (round 11): when the terminal FlushRequest cannot be
    /// enqueued, the actor reaps a ZERO-line (recovery-restamped) entry —
    /// nothing will ever persist or serve it, so the reap is bookkeeping
    /// (reads probe the ex-leader's S3 `.partial` for a zero-line entry
    /// either way) — but must keep an entry that holds lines (the
    /// ex-leader retained tail) for the periodic snapshot +
    /// CleanupTerminalBuild path.
    #[test]
    fn discard_if_empty_removes_only_zero_line_entries() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();

        // Fresh-standby shape: stamped by recovery, sealed at terminal,
        // zero lines (worker never reconnected).
        bufs.set_exec("empty-drv", exec, "w1");
        bufs.seal("empty-drv");

        // Ex-leader shape: same stamp but the retained unflushed tail.
        bufs.set_exec("tail-drv", exec, "w1");
        assert!(bufs.push_for("tail-drv", &mk_batch("tail-drv", 0, &[b"l0"]), "w1"));
        bufs.seal("tail-drv");

        assert!(
            bufs.discard_if_empty("empty-drv"),
            "zero-line entry must be removed"
        );
        assert!(
            bufs.read_since("empty-drv", 0).is_none(),
            "reaped entry leaves no ring state behind"
        );
        assert!(!bufs.is_sealed("empty-drv"), "removal also unseals");

        assert!(
            !bufs.discard_if_empty("tail-drv"),
            "an entry holding lines must be kept for the periodic flush"
        );
        assert!(
            bufs.read_since("tail-drv", 0).is_some_and(|l| l.len() == 1),
            "retained tail still readable"
        );
        assert!(bufs.is_sealed("tail-drv"), "kept entry keeps its seal");

        assert!(
            !bufs.discard_if_empty("missing-drv"),
            "missing entry is a no-op"
        );
    }

    #[test]
    fn discard_also_unseals() {
        let bufs = LogBuffers::new();
        bufs.seal("drv-a");
        bufs.discard("drv-a");
        bufs.push(&mk_batch("drv-a", 0, &[b"fresh"]));
        assert_eq!(bufs.active_count(), 1, "discard must clear seal");
    }

    /// Pending-final mark plumbing: exec-guarded set/read, and the flag dies
    /// with the entry.
    #[test]
    fn final_pending_mark_is_exec_guarded() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        let other = Uuid::now_v7();

        assert!(
            !bufs.mark_final_pending("r14m3f-missing", exec),
            "missing entry: nothing to mark"
        );

        bufs.set_exec("r14m3f-drv", exec, "w1");
        assert!(
            !bufs.mark_final_pending("r14m3f-drv", other),
            "exec mismatch: not marked"
        );
        assert!(!bufs.final_pending("r14m3f-drv"));

        assert!(bufs.mark_final_pending("r14m3f-drv", exec));
        assert!(bufs.final_pending("r14m3f-drv"));

        // Re-asserting is fine, then the flag dies with the entry.
        assert!(bufs.mark_final_pending("r14m3f-drv", exec));
        bufs.discard("r14m3f-drv");
        assert!(!bufs.final_pending("r14m3f-drv"));
    }

    /// Restamp lifecycle of the pending-final mark: any restamp clears it.
    /// A same-exec restamp (recovery at lease re-acquisition) clears it
    /// because the marked request is from the prior tenure and will be
    /// tenure-dropped — keeping the mark would exempt the entry from
    /// terminal cleanup with no remaining reaper (bug_001, round 16) — while
    /// keeping the lines; a cross-exec restamp (an interim leader
    /// re-dispatched the drv) clears it along with the rest of the prior
    /// execution's bookkeeping, so terminal cleanup goes back to bounding
    /// the NEW execution's buffer.
    #[test]
    fn final_pending_mark_restamp_lifecycle() {
        let bufs = LogBuffers::new();
        let e1 = Uuid::now_v7();
        let e2 = Uuid::now_v7();

        bufs.set_exec("r14m3g-drv", e1, "w1");
        assert!(bufs.push_for("r14m3g-drv", &mk_batch("r14m3g-drv", 0, &[b"l0"]), "w1"));
        assert!(bufs.mark_final_pending("r14m3g-drv", e1));
        assert!(bufs.final_pending("r14m3g-drv"));

        // Same-exec restamp clears the mark but keeps the lines.
        bufs.set_exec("r14m3g-drv", e1, "w1");
        assert!(
            !bufs.final_pending("r14m3g-drv"),
            "same-exec restamp must clear the prior tenure's dead mark"
        );
        assert!(
            bufs.read_since("r14m3g-drv", 0)
                .is_some_and(|l| l.len() == 1),
            "same-exec restamp keeps the lines"
        );

        // Cross-exec restamp clears it (and the lines).
        bufs.set_exec("r14m3g-drv", e2, "w1");
        assert!(
            !bufs.final_pending("r14m3g-drv"),
            "cross-exec restamp clears the prior execution's mark"
        );
        assert!(
            bufs.read_since("r14m3g-drv", 0)
                .is_some_and(|l| l.is_empty()),
            "cross-exec restamp clears the prior execution's lines"
        );
    }

    /// Seal lifecycle across restamps: any restamp clears the seal. A
    /// same-exec restamp (recovery at lease re-acquisition, no interim
    /// re-dispatch) clears it because the prior tenure's pending final is
    /// tenure-dropped and can never drain/unseal the entry — a surviving
    /// seal would mute the still-streaming worker's post-flap batches
    /// (bug_001, round 16). A cross-exec restamp (an interim leader
    /// re-dispatched the drv) clears it along with the rest of the prior
    /// execution's bookkeeping: the prior exec's final can no longer drain
    /// this entry, and a surviving seal would make push_for silently drop
    /// every batch of the NEW execution (bug_009, round 15).
    #[test]
    fn seal_restamp_lifecycle() {
        let bufs = LogBuffers::new();
        let e1 = Uuid::now_v7();
        let e2 = Uuid::now_v7();

        // exec₁ streams, reaches terminal (seal), final is pending/deferred
        // (mark).
        bufs.set_exec("r15b9a-drv", e1, "w1");
        assert!(bufs.push_for("r15b9a-drv", &mk_batch("r15b9a-drv", 0, &[b"l0"]), "w1"));
        bufs.seal("r15b9a-drv");
        assert!(bufs.mark_final_pending("r15b9a-drv", e1));

        // Same-exec restamp (recovery, no interim re-dispatch): seal cleared,
        // the still-streaming execution's batches keep landing.
        bufs.set_exec("r15b9a-drv", e1, "w1");
        assert!(
            !bufs.is_sealed("r15b9a-drv"),
            "same-exec restamp must clear the seal (the pending final is tenure-dropped, not drained)"
        );
        assert!(
            bufs.push_for(
                "r15b9a-drv",
                &mk_batch("r15b9a-drv", 1, &[b"post-flap"]),
                "w1"
            ),
            "the execution's batches must keep landing across a same-exec restamp"
        );

        // Cross-exec restamp (interim leader re-dispatched under e2 on w2):
        // seal cleared, new execution's batches land, prior worker stays out.
        bufs.set_exec("r15b9a-drv", e2, "w2");
        assert!(
            !bufs.is_sealed("r15b9a-drv"),
            "cross-exec restamp must clear the prior execution's seal"
        );
        assert!(
            bufs.push_for("r15b9a-drv", &mk_batch("r15b9a-drv", 0, &[b"e2-l0"]), "w2"),
            "the re-dispatched execution's batches must be accepted after the restamp"
        );
        assert!(
            bufs.read_since("r15b9a-drv", 0)
                .is_some_and(|l| l.len() == 1),
            "exec₂'s line is in the ring"
        );
        assert!(
            !bufs.push_for("r15b9a-drv", &mk_batch("r15b9a-drv", 5, &[b"stray"]), "w1"),
            "the prior executor's late batches are still rejected by the binding gate"
        );
        assert_eq!(bufs.exec_id("r15b9a-drv"), Some(e2));
    }

    /// bug_001 (r16): the full same-exec restamp shape the terminal epilogue
    /// leaves behind. A same-exec restamp happens only at lease
    /// re-acquisition (recovery; dispatch always discards first), and the
    /// re-acquisition bumped the lease generation — so the prior tenure's
    /// retained final, the request the seal was waiting on, is now always
    /// tenure-dropped by the flusher without unsealing. The restamp must
    /// therefore clear the seal and the final-pending mark itself (the new
    /// tenure's own terminal re-seals and re-marks), while keeping the
    /// lines: otherwise the reconnected worker's post-flap batches are
    /// silently rejected and the next final uploads a truncated log as
    /// complete.
    #[test]
    fn same_exec_restamp_clears_seal_and_final_pending() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();

        bufs.set_exec("r16b1-drv", exec, "w1");
        assert!(bufs.push_for("r16b1-drv", &mk_batch("r16b1-drv", 0, &[b"pre-flap"]), "w1"));
        // Terminal epilogue shape: seal, then mark after the successful
        // enqueue (the same APIs terminal_log_epilogue calls).
        bufs.seal("r16b1-drv");
        assert!(bufs.mark_final_pending("r16b1-drv", exec));

        // Lease re-acquisition: recovery restamps the SAME exec_id.
        bufs.set_exec("r16b1-drv", exec, "w1");

        assert!(
            !bufs.is_sealed("r16b1-drv"),
            "same-exec restamp must clear the seal — the prior tenure's final \
             is tenure-dropped and can never drain/unseal this entry"
        );
        assert!(
            !bufs.final_pending("r16b1-drv"),
            "same-exec restamp must clear the final-pending mark — the marked \
             request is dead, and the mark would otherwise exempt the entry \
             from terminal cleanup forever"
        );
        assert!(
            bufs.read_since("r16b1-drv", 0)
                .is_some_and(|l| l.len() == 1),
            "lines are retained — the still-streaming execution keeps accumulating"
        );
        assert!(
            bufs.push_for(
                "r16b1-drv",
                &mk_batch("r16b1-drv", 1, &[b"post-flap"]),
                "w1"
            ),
            "the reconnected worker's post-flap batches must be accepted"
        );
    }

    /// Exec-guarded empty-reap used by the flusher's deferral arm: removes
    /// only a zero-line entry stamped with the expected exec (clearing the
    /// seal tombstone with it); refuses non-empty entries and exec
    /// mismatches.
    #[test]
    fn discard_if_empty_for_exec_guards_on_exec_and_emptiness() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        let other = Uuid::now_v7();

        // Empty + matching exec → reaped (and unsealed).
        bufs.set_exec("r14m3h-empty", exec, "w1");
        bufs.seal("r14m3h-empty");
        assert!(bufs.discard_if_empty_for_exec("r14m3h-empty", exec));
        assert!(bufs.read_since("r14m3h-empty", 0).is_none());
        assert!(!bufs.is_sealed("r14m3h-empty"));

        // Empty but exec mismatch → kept.
        bufs.set_exec("r14m3i-mismatch", exec, "w1");
        assert!(!bufs.discard_if_empty_for_exec("r14m3i-mismatch", other));
        assert_eq!(bufs.exec_id("r14m3i-mismatch"), Some(exec));

        // Non-empty + matching exec → kept (its lines are still wanted).
        bufs.set_exec("r14m3j-tail", exec, "w1");
        assert!(bufs.push_for("r14m3j-tail", &mk_batch("r14m3j-tail", 0, &[b"l0"]), "w1"));
        assert!(!bufs.discard_if_empty_for_exec("r14m3j-tail", exec));
        assert!(
            bufs.read_since("r14m3j-tail", 0)
                .is_some_and(|l| l.len() == 1),
            "retained tail still readable"
        );

        // Missing entry → no-op.
        assert!(!bufs.discard_if_empty_for_exec("r14m3k-missing", exec));
    }

    /// r17 bug_003: the tenure-drop reap evaluates seal + exec + (optional)
    /// emptiness inside one `remove_if` predicate. Each gate refuses on its
    /// own; `require_empty=false` widens the reap to non-empty sealed
    /// entries (the finalized-elsewhere case) without touching unsealed or
    /// mis-stamped ones.
    // r[verify obs.log.deferred-final-retry+4]
    #[test]
    fn discard_if_sealed_for_exec_checks_seal_exec_and_emptiness() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        let other = Uuid::now_v7();

        // Sealed + empty + matching exec → removed and unsealed (both
        // require_empty values would accept; use the strict one).
        bufs.set_exec("r17p1-sealed-empty", exec, "w1");
        bufs.seal("r17p1-sealed-empty");
        assert!(bufs.discard_if_sealed_for_exec("r17p1-sealed-empty", exec, true));
        assert!(bufs.read_since("r17p1-sealed-empty", 0).is_none());
        assert!(!bufs.is_sealed("r17p1-sealed-empty"));

        // Unsealed (live carrier shape) → untouched regardless of
        // emptiness requirement.
        bufs.set_exec("r17p2-unsealed", exec, "w1");
        assert!(!bufs.discard_if_sealed_for_exec("r17p2-unsealed", exec, true));
        assert!(!bufs.discard_if_sealed_for_exec("r17p2-unsealed", exec, false));
        assert_eq!(bufs.exec_id("r17p2-unsealed"), Some(exec));

        // Sealed + non-empty: refused with require_empty=true, removed
        // (and unsealed) with require_empty=false.
        bufs.set_exec("r17p3-tail", exec, "w1");
        assert!(bufs.push_for("r17p3-tail", &mk_batch("r17p3-tail", 0, &[b"l0"]), "w1"));
        bufs.seal("r17p3-tail");
        assert!(!bufs.discard_if_sealed_for_exec("r17p3-tail", exec, true));
        assert!(
            bufs.read_since("r17p3-tail", 0)
                .is_some_and(|l| l.len() == 1),
            "refused reap leaves the lines"
        );
        assert!(bufs.is_sealed("r17p3-tail"), "refused reap leaves the seal");
        assert!(bufs.discard_if_sealed_for_exec("r17p3-tail", exec, false));
        assert!(bufs.read_since("r17p3-tail", 0).is_none());
        assert!(!bufs.is_sealed("r17p3-tail"));

        // Exec mismatch → untouched even when sealed and empty.
        bufs.set_exec("r17p4-mismatch", exec, "w1");
        bufs.seal("r17p4-mismatch");
        assert!(!bufs.discard_if_sealed_for_exec("r17p4-mismatch", other, true));
        assert!(!bufs.discard_if_sealed_for_exec("r17p4-mismatch", other, false));
        assert_eq!(bufs.exec_id("r17p4-mismatch"), Some(exec));
        assert!(bufs.is_sealed("r17p4-mismatch"));

        // Missing entry → no-op.
        assert!(!bufs.discard_if_sealed_for_exec("r17p5-missing", exec, true));
    }

    /// bug_080: `RING_CAPACITY` bounds line COUNT only; an untrusted
    /// worker sending few-but-large lines must not OOM the scheduler.
    // r[verify obs.log.ring-byte-cap]
    #[test]
    fn push_evicts_on_byte_cap() {
        let bufs = LogBuffers::new();
        // 300 × MAX_LINE_LEN = ~18.75 MiB total, 300 lines — well
        // under RING_CAPACITY=100k but > RING_BYTE_CAP=16 MiB.
        // (Lines must be ≤ MAX_LINE_LEN or push() truncates them
        // BEFORE byte-accounting — that's `push_truncates_oversized_line`.)
        let line = vec![b'x'; MAX_LINE_LEN];
        let batch: Vec<&[u8]> = (0..300).map(|_| line.as_slice()).collect();
        bufs.push(&mk_batch("drv-a", 0, &batch));
        let (_, count, bytes, _) = bufs.drain("drv-a").unwrap();
        assert!(
            bytes <= RING_BYTE_CAP as u64,
            "bug_080: total bytes must be ≤ RING_BYTE_CAP; got {bytes} \
             (pre-fix: line-count cap alone → all 300 lines / ~18.75 MiB retained)"
        );
        assert!(
            count <= (RING_BYTE_CAP / MAX_LINE_LEN) as u64,
            "byte-cap eviction: 300 × 64 KiB → ≤256 lines retained, got {count}"
        );
    }

    #[test]
    fn push_truncates_oversized_line() {
        let bufs = LogBuffers::new();
        let huge = vec![b'x'; 200 * 1024];
        bufs.push(&mk_batch("drv-a", 0, &[&huge]));
        let (_, _, bytes, lines) = bufs.drain("drv-a").unwrap();
        assert_eq!(
            lines[0].len(),
            MAX_LINE_LEN,
            "single line truncated to MAX_LINE_LEN"
        );
        assert_eq!(bytes, MAX_LINE_LEN as u64);
        // The retained Vec's CAPACITY must also be bounded: clone-then-
        // truncate would allocate the full 200 KiB and keep that capacity
        // while `bytes` counts only MAX_LINE_LEN — the byte cap would
        // bound accounted bytes, not allocated bytes. `into_drained`
        // moves the stored Vecs out, so the capacity is observable here.
        assert!(
            lines[0].capacity() <= MAX_LINE_LEN,
            "retained line capacity must be ≤ MAX_LINE_LEN (slice-then-to_vec), \
             got {} — clone-then-truncate keeps the oversized allocation",
            lines[0].capacity()
        );
    }

    // ── set_exec / push_for binding check ───────────────────────────────
    // r[verify sched.log.batch-binding]

    #[test]
    fn set_exec_creates_empty_entry_with_metadata() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        bufs.set_exec("drv-a", exec, "executor-1");
        // Entry exists and is empty.
        assert_eq!(bufs.active_count(), 1);
        assert_eq!(bufs.exec_id("drv-a"), Some(exec));
        // No lines yet.
        assert_eq!(bufs.read_since("drv-a", 0), Some(vec![]));
    }

    #[test]
    fn discard_then_set_exec_resets_entry() {
        let bufs = LogBuffers::new();
        let exec1 = Uuid::now_v7();
        bufs.set_exec("drv-a", exec1, "executor-1");
        bufs.push(&mk_batch("drv-a", 0, &[b"line1"]));
        bufs.discard("drv-a");
        let exec2 = Uuid::now_v7();
        bufs.set_exec("drv-a", exec2, "executor-2");
        // Old lines gone, new exec_id.
        assert_eq!(bufs.exec_id("drv-a"), Some(exec2));
        assert_eq!(bufs.read_since("drv-a", 0), Some(vec![]));
    }

    /// bug_004 (r9): a re-stamp with a DIFFERENT exec_id and no preceding
    /// `discard` — recovery restamping an ex-leader's retained entry after
    /// an interim leader re-dispatched the drv — must not carry the old
    /// execution's lines into the new execution's buffer. The periodic
    /// flusher reads `(exec_id, snapshot)` off this entry and would upload
    /// the old lines under the new exec's `drv_logs` row + S3 key.
    #[test]
    fn set_exec_with_new_exec_id_clears_stale_lines() {
        let bufs = LogBuffers::new();
        let exec1 = Uuid::now_v7();
        bufs.set_exec("drv-a", exec1, "executor-1");
        assert!(bufs.push_for(
            "drv-a",
            &mk_batch("drv-a", 0, &[b"old-exec-line"]),
            "executor-1"
        ));
        // Precondition: the line is buffered under exec1, and a recovered
        // prefix cached for exec1 is visible as such.
        assert_eq!(bufs.read_since("drv-a", 0).unwrap().len(), 1);
        assert!(bufs.set_recovered_prefix(
            "drv-a",
            exec1,
            std::sync::Arc::new(RecoveredPrefix {
                first_line: 0,
                line_count: 1,
                total_bytes: 5,
                compressed: vec![1, 2, 3],
            })
        ));
        assert!(matches!(
            bufs.prefix_state("drv-a", exec1),
            PrefixState::Cached(_)
        ));

        let exec2 = Uuid::now_v7();
        bufs.set_exec("drv-a", exec2, "executor-2");

        assert_eq!(bufs.exec_id("drv-a"), Some(exec2));
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![]),
            "exec1's lines must not survive a re-stamp to exec2"
        );
        // exec1's recovered prefix must not survive either: the new
        // execution starts Unchecked, and a query under the stale exec_id
        // resolves to the safe "don't fetch" default.
        assert!(matches!(
            bufs.prefix_state("drv-a", exec2),
            PrefixState::Unchecked
        ));
        assert!(matches!(
            bufs.prefix_state("drv-a", exec1),
            PrefixState::Checked
        ));
        // The new executor's batches are accepted into the now-clean buffer.
        assert!(bufs.push_for(
            "drv-a",
            &mk_batch("drv-a", 0, &[b"new-exec-line"]),
            "executor-2"
        ));
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![(0, b"new-exec-line".to_vec())])
        );
    }

    /// The complement: a same-exec_id re-stamp (single lease flap, no
    /// interim re-dispatch — recovery re-stamps the SAME exec_id from
    /// `assignments`) must RETAIN the lines. The worker is still streaming
    /// this execution; clearing here would lose the unflushed tail in the
    /// common flap case. The prefix bookkeeping is the exception: it
    /// encodes a conclusion from a previous tenure, so the restamp re-arms
    /// it and the flusher re-consults the stored row once this tenure
    /// (deeper coverage in `same_exec_restamp_resets_prefix_state`).
    #[test]
    fn set_exec_with_same_exec_id_retains_lines() {
        let bufs = LogBuffers::new();
        let exec1 = Uuid::now_v7();
        bufs.set_exec("drv-a", exec1, "executor-1");
        assert!(bufs.push_for("drv-a", &mk_batch("drv-a", 0, &[b"line-0"]), "executor-1"));
        assert!(bufs.set_recovered_prefix(
            "drv-a",
            exec1,
            std::sync::Arc::new(RecoveredPrefix {
                first_line: 0,
                line_count: 1,
                total_bytes: 5,
                compressed: vec![1, 2, 3],
            })
        ));

        bufs.set_exec("drv-a", exec1, "executor-1");

        assert_eq!(bufs.exec_id("drv-a"), Some(exec1));
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![(0, b"line-0".to_vec())]),
            "a same-exec re-stamp is not a new execution; lines must survive"
        );
        assert!(
            matches!(bufs.prefix_state("drv-a", exec1), PrefixState::Unchecked),
            "lines survive; prefix bookkeeping is re-armed so the new tenure re-consults the stored row"
        );
    }

    /// A same-exec re-stamp (recovery at lease re-acquisition) re-arms the
    /// prefix bookkeeping every time — both the `Checked` latch and a
    /// cached prefix — while leaving the buffered lines untouched. The
    /// flusher must re-consult the stored row once per tenure: an interim
    /// leader may have extended it while this replica was deposed.
    // r[verify obs.log.stored-coverage-preserved]
    #[test]
    fn same_exec_restamp_resets_prefix_state() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        bufs.set_exec("drv-a", exec, "executor-1");
        assert!(bufs.push_for("drv-a", &mk_batch("drv-a", 0, &[b"line-0"]), "executor-1"));

        // Tenure 1: the flusher looked and found nothing.
        bufs.mark_prefix_checked("drv-a", exec);
        assert!(matches!(
            bufs.prefix_state("drv-a", exec),
            PrefixState::Checked
        ));

        // Re-acquisition: the latch is cleared, lines are kept.
        bufs.set_exec("drv-a", exec, "executor-1");
        assert!(matches!(
            bufs.prefix_state("drv-a", exec),
            PrefixState::Unchecked
        ));
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![(0, b"line-0".to_vec())]),
            "the re-arm must not touch the buffered lines"
        );

        // Tenure 2: the flusher cached a recovered prefix.
        assert!(bufs.set_recovered_prefix(
            "drv-a",
            exec,
            std::sync::Arc::new(RecoveredPrefix {
                first_line: 0,
                line_count: 1,
                total_bytes: 5,
                compressed: vec![1, 2, 3],
            })
        ));
        assert!(matches!(
            bufs.prefix_state("drv-a", exec),
            PrefixState::Cached(_)
        ));

        // Another re-acquisition: the cache is cleared too, lines kept.
        bufs.set_exec("drv-a", exec, "executor-1");
        assert!(matches!(
            bufs.prefix_state("drv-a", exec),
            PrefixState::Unchecked
        ));
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![(0, b"line-0".to_vec())])
        );
    }

    /// bug_001 (r13): the acquisition-time re-arm — third leg of the
    /// per-tenure discipline next to set_exec's cross-exec and same-exec
    /// arms. It must clear the Checked latch and any cached prefix on
    /// EVERY entry (sealed ones included) while leaving lines, exec
    /// stamps, executor bindings, and seals untouched; entries with
    /// nothing latched are not counted.
    // r[verify obs.log.stored-coverage-preserved]
    #[test]
    fn rearm_prefix_reconciliation_clears_latches_keeps_lines() {
        let bufs = LogBuffers::new();
        let (exec_a, exec_b, exec_c) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
        // Distinct hash parts -> distinct LogBuffers keys (drv_log_hash
        // keys on the 32-char hash; reusing one hash would collide).
        let drv_a = format!("/nix/store/{}-rearm-a.drv", "a".repeat(32));
        let drv_b = format!("/nix/store/{}-rearm-b.drv", "b".repeat(32));
        let drv_c = format!("/nix/store/{}-rearm-c.drv", "c".repeat(32));

        // (a) Checked latch from a prior tenure.
        bufs.set_exec(&drv_a, exec_a, "w-a");
        assert!(bufs.push_for(&drv_a, &mk_batch(&drv_a, 0, &[b"a0"]), "w-a"));
        bufs.mark_prefix_checked(&drv_a, exec_a);
        // (b) Cached prefix + sealed (queued-final shape).
        bufs.set_exec(&drv_b, exec_b, "w-b");
        assert!(bufs.push_for(&drv_b, &mk_batch(&drv_b, 5, &[b"b5"]), "w-b"));
        assert!(bufs.set_recovered_prefix(
            &drv_b,
            exec_b,
            std::sync::Arc::new(RecoveredPrefix {
                first_line: 0,
                line_count: 5,
                total_bytes: 10,
                compressed: vec![1, 2, 3],
            })
        ));
        bufs.seal(&drv_b);
        // (c) Already Unchecked — must not be counted.
        bufs.set_exec(&drv_c, exec_c, "w-c");
        assert!(bufs.push_for(&drv_c, &mk_batch(&drv_c, 0, &[b"c0"]), "w-c"));

        assert_eq!(
            bufs.rearm_prefix_reconciliation(),
            2,
            "only latched entries count"
        );

        for (drv, exec) in [(&drv_a, exec_a), (&drv_b, exec_b), (&drv_c, exec_c)] {
            assert!(
                matches!(bufs.prefix_state(drv, exec), PrefixState::Unchecked),
                "{drv}: must be Unchecked after the re-arm"
            );
            assert_eq!(bufs.exec_id(drv), Some(exec), "{drv}: exec stamp untouched");
            assert_eq!(
                bufs.read_since(drv, 0).map(|l| l.len()),
                Some(1),
                "{drv}: lines untouched"
            );
        }
        assert!(
            bufs.is_sealed(&drv_b),
            "seal tombstones are not the re-arm's to clear"
        );
        assert_eq!(bufs.rearm_prefix_reconciliation(), 0, "idempotent");
    }

    /// `truncate_below` drops exactly the ring lines below the threshold
    /// (the range a stored row already covers), keeps the rest, keeps the
    /// byte accounting consistent, and refuses to touch an entry stamped
    /// with a different execution.
    // r[verify obs.log.stored-coverage-preserved]
    #[test]
    fn truncate_below_drops_only_superseded_head() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        bufs.set_exec("drv-a", exec, "executor-1");
        // Lines 0-4 (2 bytes each), then a hole, then lines 8-9.
        assert!(bufs.push_for(
            "drv-a",
            &mk_batch("drv-a", 0, &[b"aa", b"bb", b"cc", b"dd", b"ee"]),
            "executor-1"
        ));
        assert!(bufs.push_for(
            "drv-a",
            &mk_batch("drv-a", 8, &[b"ff", b"gg"]),
            "executor-1"
        ));

        // A wrong-exec call is a no-op.
        assert_eq!(bufs.truncate_below("drv-a", Uuid::now_v7(), 100), (0, 0));
        assert_eq!(bufs.read_since("drv-a", 0).unwrap().len(), 7);

        // Stored coverage ends at 6: lines 0-4 yield (5 lines, 10 bytes);
        // the post-hole tail (8, 9) survives.
        assert_eq!(bufs.truncate_below("drv-a", exec, 6), (5, 10));
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![(8, b"ff".to_vec()), (9, b"gg".to_vec())])
        );

        // Threshold at or below the new first line is a no-op.
        assert_eq!(bufs.truncate_below("drv-a", exec, 8), (0, 0));
        assert_eq!(bufs.read_since("drv-a", 0).unwrap().len(), 2);

        // Byte accounting stayed consistent: a follow-up push neither
        // panics (debug-mode underflow) nor mis-evicts.
        assert!(bufs.push_for("drv-a", &mk_batch("drv-a", 10, &[b"hh"]), "executor-1"));
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![
                (8, b"ff".to_vec()),
                (9, b"gg".to_vec()),
                (10, b"hh".to_vec())
            ])
        );
    }

    #[test]
    fn exec_id_none_for_legacy_push() {
        let bufs = LogBuffers::new();
        // push() still creates entries (or_default) — but with no exec_id.
        // The flusher MUST treat this as "skip" rather than mint a key.
        bufs.push(&mk_batch("drv-a", 0, &[b"line1"]));
        assert_eq!(bufs.exec_id("drv-a"), None);
    }

    #[test]
    fn push_for_rejects_wrong_executor() {
        let bufs = LogBuffers::new();
        bufs.set_exec("drv-a", Uuid::now_v7(), "executor-1");
        let accepted = bufs.push_for("drv-a", &mk_batch("drv-a", 0, &[b"line1"]), "executor-2");
        // Rejected — no lines stored, and the caller is told so it can
        // gate the gateway forward (the second consumer of the batch).
        assert!(!accepted);
        assert_eq!(bufs.read_since("drv-a", 0), Some(vec![]));
    }

    #[test]
    fn push_for_rejects_unknown_drv() {
        let bufs = LogBuffers::new();
        // No set_exec → no entry → rejected, does NOT create an entry.
        let accepted = bufs.push_for("drv-x", &mk_batch("drv-x", 0, &[b"line1"]), "executor-1");
        assert!(!accepted);
        assert_eq!(bufs.active_count(), 0);
    }

    #[test]
    fn push_for_accepts_matching_executor() {
        let bufs = LogBuffers::new();
        bufs.set_exec("drv-a", Uuid::now_v7(), "executor-1");
        let accepted = bufs.push_for("drv-a", &mk_batch("drv-a", 0, &[b"line1"]), "executor-1");
        assert!(accepted);
        let lines = bufs.read_since("drv-a", 0).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], (0, b"line1".to_vec()));
    }

    #[test]
    fn push_for_respects_seal() {
        let bufs = LogBuffers::new();
        bufs.set_exec("drv-a", Uuid::now_v7(), "executor-1");
        bufs.seal("drv-a");
        let accepted = bufs.push_for("drv-a", &mk_batch("drv-a", 0, &[b"late"]), "executor-1");
        // Sealed → rejected even from the right executor, and the caller
        // is told so the gateway forward is suppressed too.
        assert!(!accepted);
        assert_eq!(bufs.read_since("drv-a", 0), Some(vec![]));
    }

    #[test]
    fn push_for_rejects_unstamped_entry() {
        let bufs = LogBuffers::new();
        // Legacy push() creates an entry with no assigned_executor.
        // push_for must reject — and report rejection — even though the
        // entry exists.
        bufs.push(&mk_batch("drv-a", 0, &[b"legacy"]));
        let accepted = bufs.push_for("drv-a", &mk_batch("drv-a", 1, &[b"new"]), "executor-1");
        assert!(!accepted);
        // Only the legacy line is present.
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![(0, b"legacy".to_vec())])
        );
    }

    /// Worker numbering must be strictly monotone per execution: a batch
    /// whose first_line_number is at or below the ring's highest stored
    /// line number is rejected (per-batch), and the caller is told so the
    /// gateway forward is suppressed too.
    // r[verify sched.executor.input-bounds+2]
    #[test]
    fn push_for_rejects_non_monotonic_batch() {
        let bufs = LogBuffers::new();
        bufs.set_exec("drv-a", Uuid::now_v7(), "executor-1");
        assert!(bufs.push_for("drv-a", &mk_batch("drv-a", 1000, &[b"l1000"]), "executor-1"));

        // Below the ring's end → rejected, nothing stored.
        assert!(!bufs.push_for("drv-a", &mk_batch("drv-a", 0, &[b"l0"]), "executor-1"));
        // Equal to the ring's end → rejected (would double-number the line).
        assert!(!bufs.push_for("drv-a", &mk_batch("drv-a", 1000, &[b"dup"]), "executor-1"));
        assert_eq!(
            bufs.read_since("drv-a", 0),
            Some(vec![(1000, b"l1000".to_vec())]),
            "rejected batches must not reach the ring"
        );
        // Rejection is per-batch: the next in-order batch is accepted.
        assert!(bufs.push_for("drv-a", &mk_batch("drv-a", 1001, &[b"l1001"]), "executor-1"));
        assert_eq!(bufs.read_since("drv-a", 0).unwrap().len(), 2);
    }

    /// Upward gaps stay accepted — that is the legitimate interior-hole /
    /// resumed-suffix shape (obs.log.gap-span), not an ordering violation.
    #[test]
    fn push_for_accepts_forward_gap() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        bufs.set_exec("drv-a", exec, "executor-1");
        assert!(bufs.push_for(
            "drv-a",
            &mk_batch("drv-a", 0, &[b"l0", b"l1"]),
            "executor-1"
        ));
        assert!(bufs.push_for("drv-a", &mk_batch("drv-a", 8, &[b"l8"]), "executor-1"));
        let (first, last, count) = bufs.span("drv-a", exec).unwrap();
        assert_eq!((first, last, count), (0, 8, 3));
    }

    /// first_line_number near u64::MAX would make `base + i` wrap (panic in
    /// debug, wrap in release): rejected before it reaches push_into.
    // r[verify sched.executor.input-bounds+2]
    #[test]
    fn push_for_rejects_line_number_overflow() {
        let bufs = LogBuffers::new();
        bufs.set_exec("drv-a", Uuid::now_v7(), "executor-1");
        let accepted = bufs.push_for(
            "drv-a",
            &mk_batch("drv-a", u64::MAX, &[b"a", b"b"]),
            "executor-1",
        );
        assert!(!accepted);
        assert_eq!(bufs.read_since("drv-a", 0), Some(vec![]));
    }

    #[test]
    fn discard_if_owned_by_removes_own_unsealed_buffer() {
        let bufs = LogBuffers::new();
        bufs.set_exec("drv-a", Uuid::now_v7(), "executor-1");
        assert!(bufs.push_for("drv-a", &mk_batch("drv-a", 0, &[b"l0"]), "executor-1"));
        assert!(bufs.discard_if_owned_by("drv-a", "executor-1"));
        assert_eq!(bufs.exec_id("drv-a"), None);
        assert_eq!(bufs.read_since("drv-a", 0), None);
    }

    /// bug_004: the security invariant. A worker-supplied path that
    /// normalizes (via `drv_log_hash`) to another executor's buffer key MUST
    /// NOT remove that executor's entry on disconnect.
    // r[verify sched.log.batch-binding]
    #[test]
    fn discard_if_owned_by_preserves_other_executor_buffer() {
        let bufs = LogBuffers::new();
        bufs.set_exec("drv-a", Uuid::now_v7(), "victim");
        assert!(!bufs.discard_if_owned_by("drv-a", "attacker"));
        assert!(bufs.exec_id("drv-a").is_some());
    }

    /// Sealed = the flusher owns drain. The disconnect cleanup MUST NOT
    /// reap a sealed buffer or `flush_final`'s staleness check
    /// (`buffers.exec_id(...) != Some(req.exec_id)`) would drop the request
    /// and silently lose a completed build's log.
    #[test]
    fn discard_if_owned_by_preserves_sealed_buffer() {
        let bufs = LogBuffers::new();
        bufs.set_exec("drv-a", Uuid::now_v7(), "executor-1");
        bufs.seal("drv-a");
        assert!(!bufs.discard_if_owned_by("drv-a", "executor-1"));
        assert!(bufs.is_sealed("drv-a"));
        assert!(bufs.exec_id("drv-a").is_some());
    }

    #[test]
    fn discard_if_owned_by_no_op_on_missing() {
        let bufs = LogBuffers::new();
        assert!(!bufs.discard_if_owned_by("drv-a", "executor-1"));
    }

    #[test]
    fn discard_if_owned_by_preserves_unstamped_entry() {
        let bufs = LogBuffers::new();
        // Legacy `push()` creates an unstamped entry (test-only; production
        // entries always come from `set_exec`). The ownership check refuses
        // to remove it — there is no executor to attribute the discard to.
        bufs.push(&mk_batch("drv-a", 0, &[b"l0"]));
        assert!(!bufs.discard_if_owned_by("drv-a", "executor-1"));
        assert!(bufs.read_since("drv-a", 0).is_some());
    }

    /// Recovery-sweep primitive: only unsealed entries outside the live-key
    /// set are discarded; live entries and sealed entries (final
    /// FlushRequest may still be queued) are untouched.
    #[test]
    fn discard_unsealed_not_in_spares_live_and_sealed() {
        let bufs = LogBuffers::new();
        let exec = Uuid::now_v7();
        for key in ["keepdrv", "stalecold", "stalesealed"] {
            bufs.set_exec(key, exec, "w1");
            // mk_batch hardcodes executor_id "test-worker"; push_for checks
            // its `executor` argument against the assigned executor, not the
            // batch field, so the "w1" binding is what's exercised here.
            assert!(bufs.push_for(key, &mk_batch(key, 0, &[b"line"]), "w1"));
        }
        bufs.seal("stalesealed");

        let live: std::collections::HashSet<String> = ["keepdrv".to_string()].into();
        assert_eq!(bufs.discard_unsealed_not_in(&live), 1);

        // Live entry untouched.
        assert_eq!(bufs.read_since("keepdrv", 0).map(|l| l.len()), Some(1));
        // Stale unsealed entry gone — the read path falls through to S3.
        assert!(bufs.read_since("stalecold", 0).is_none());
        // Stale sealed entry kept for the queued final flush; seal intact.
        assert_eq!(bufs.read_since("stalesealed", 0).map(|l| l.len()), Some(1));
        assert!(bufs.is_sealed("stalesealed"));
    }
}
