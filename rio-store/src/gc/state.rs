//! Durable, cluster-scoped chunk-collect state and the cycle lease
//! (bug_174 + merged_bug_211, bughunt wave D1; migration 090).
//!
//! Pre-wave, the collector's cadence/cursor/backlog were PROCESS
//! facts: every replica armed its own daily `interval_at(boot + 24h)`
//! timer (mutual exclusion — the advisory try-lock — but no rate
//! limit: N replicas ⇒ up to N heavy cycles/day at KEDA scale), the
//! keyset cursor was a process static (a capped pass restarted from
//! scratch on whichever replica won next), and the backlog estimate
//! was anchored on whichever pod served a dry run and drained only by
//! that same process's cycles — every OTHER replica's gauge sat
//! frozen at its pre-registered 0 (or a stale anchor) forever.
//!
//! Post-090 these are rows of the `gc_collect_state` singleton:
//! cycles are atomic stamped events (`cycle_epoch`,
//! `last_live_cycle_at`), the cursor and backlog estimate live in the
//! row, the backstop fires ONLY when `now - last_live_cycle_at`
//! crosses the interval (cluster-wide, not per-replica), and every
//! replica publishes its gauges from a 60s row read — replicas
//! converge on the durable value; a frozen foreign anchor is
//! unrepresentable. Aggregation semantics: the gauges are a
//! REPLICATED CLUSTER FACT — aggregate with max(), never sum()
//! (owner decision Q6, 2026-06-03; docs/ops/gc-enablement.typ D4).

use sqlx::PgPool;

use super::lock::PgSessionLock;

/// One row of `gc_collect_state` (migration 090).
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct GcCollectState {
    pub(crate) cycle_epoch: i64,
    pub(crate) cursor: Option<Vec<u8>>,
    pub(crate) backlog_estimate: Option<i64>,
    pub(crate) last_mark_set_size: Option<i64>,
    pub(crate) last_would_collect: Option<i64>,
}

const SELECT_STATE: &str = "SELECT cycle_epoch, cursor, \
                                   backlog_estimate, last_mark_set_size, last_would_collect \
                              FROM gc_collect_state WHERE singleton";

/// `$1` = interval seconds (float). TRUE when (a) no live cycle has
/// ever run or the last one is at least the interval old, AND (b) no
/// live cycle has been ATTEMPTED inside the interval (bug_284:
/// migration 100). (a) is the success cadence — the stalled alert
/// keys on it; (b) is the attempt throttle — a cycle that aborts
/// without committing (fail-closed ParseFailure, mid-cycle DB error)
/// cannot be re-attempted faster than the documented heavy-cycle
/// cadence, because the attempt stamp is written BEFORE the cycle
/// runs and no outcome arm can un-write it. Evaluated on the DB clock
/// (no cross-replica clock enters the cadence decision).
const BACKSTOP_DUE_SQL: &str = "SELECT (last_live_cycle_at IS NULL \
        OR (now() - last_live_cycle_at) >= make_interval(secs => $1)) \
       AND (last_attempt_at IS NULL \
        OR (now() - last_attempt_at) >= make_interval(secs => $1)) \
   FROM gc_collect_state WHERE singleton";

/// The backstop's cheap pre-check, WITHOUT the lock (a stale read can
/// only cause a harmless lease-acquire that re-checks under the lock).
pub(crate) async fn backstop_due_unlocked(
    pool: &PgPool,
    interval: std::time::Duration,
) -> Result<bool, sqlx::Error> {
    sqlx::query_scalar(BACKSTOP_DUE_SQL)
        .bind(interval.as_secs_f64())
        .fetch_one(pool)
        .await
}

/// Read the collect state WITHOUT the lock (the backstop's cheap
/// pre-check and the per-replica gauge publisher).
pub(crate) async fn read_state_unlocked(pool: &PgPool) -> Result<GcCollectState, sqlx::Error> {
    sqlx::query_as(SELECT_STATE).fetch_one(pool).await
}

/// What a finished cycle commits to the durable row.
pub(crate) enum CycleCommit {
    /// A live (deleting) cycle: stamps `last_live_cycle_at`, persists
    /// the stop cursor, decrements the backlog estimate (floor 0) --
    /// or, when NO anchor exists yet, establishes one from the
    /// observation's unmarked-rows seed minus this cycle's victims
    /// (bug_306: live-only operation must not leave the drain gauge on
    /// its boot zero for the whole capped drain). The cursor/backlog
    /// decision is taken from the typed
    /// [`super::collect::PassDisposition`] (bug_174): only a
    /// FULL-KEYSPACE completion re-anchors the estimate at 0; a
    /// cursor-resumed completion resets the cursor but keeps the
    /// decremented estimate (chunks below the resume point that became
    /// eligible between cycles were never scanned under this mark).
    Live {
        disposition: super::collect::PassDisposition,
        victims_collected: u64,
        /// Real-basis observation — only [`super::collect`]'s
        /// real-basis arm can mint one (bug_226).
        observation: super::collect::DurableObservation,
    },
    /// A shadow (dry-run) cycle: anchors the backlog estimate at the
    /// would-collect count and records the observation sizes — but
    /// does NOT stamp `last_live_cycle_at` (a dry run is not a live
    /// cycle; the backstop's cadence question must not be answered by
    /// an observation) and does not touch the cursor.
    Shadow {
        /// Real-basis observation (bug_226): committing a
        /// counterfactual (simulated-sweep-excluded) backlog anchor or
        /// mark size is a type error — the dry-run PREVIEW numbers
        /// cannot reach this constructor.
        observation: super::collect::DurableObservation,
    },
}

/// Proof that a cycle's durable commit LANDED (merged_bug_218). The
/// only mint sites are [`GcCycleLease::commit_cycle`]'s success paths
/// — and the witness is constructed AT THE DURABILITY POINT, in the
/// expression observing the commit statement's success, BEFORE any
/// further fallible await (merged_bug_022: post-commit cleanup is
/// structurally unable to alter attribution) — so the `outcome="ok"`
/// tick — [`CycleCommitted::record_ok_outcome`], its sole producer —
/// structurally cannot run for a cycle whose stamp/cursor/backlog
/// update was lost: metric attribution and the commit result cannot
/// diverge. `#[must_use]`: dropping the witness without recording is
/// a compile-time warning at every caller.
#[must_use = "record_ok_outcome() — the ok tick rides the commit witness"]
pub(crate) struct CycleCommitted(());

impl CycleCommitted {
    /// The ONLY producer of `rio_store_gc_collect_cycles_total{outcome="ok"}`.
    pub(crate) fn record_ok_outcome(self) {
        metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "ok").increment(1);
    }
}

/// Closed result of one cycle commit (merged_bug_022, the Q1 closure
/// set): both production callers consume it by EXHAUSTIVE match — a
/// new arm cannot ship without each caller taking an attribution
/// position (`ok` / `commit_failed` / `commit_indeterminate` tick and
/// the operator render).
#[must_use = "every arm carries an attribution decision the caller must take"]
pub(crate) enum CycleCommitResult {
    /// The durable commit LANDED — the witness was minted at the
    /// durability point (or the cycle's own landed commit was
    /// recognized by payload echo on the epoch-guarded retry).
    Committed(CycleCommitted),
    /// PROVEN not landed: a foreign winner sits at `expected_epoch+1`
    /// with a payload that is not ours (had ours landed first, the
    /// foreign commit would sit at `expected_epoch+2`). Carries the
    /// primary error for the caller's disclosure.
    NotCommitted(sqlx::Error),
    /// Unprovable either way: the retry was refused/errored, the
    /// diagnostic read failed, or the epoch advanced past `+1` —
    /// the commit may or may not have landed. Carries the primary
    /// error for the caller's disclosure.
    Ambiguous(sqlx::Error),
}

#[cfg(test)]
impl CycleCommitResult {
    /// Test choreographies that REQUIRE a landed commit.
    pub(crate) fn expect_committed(self, msg: &str) -> CycleCommitted {
        match self {
            Self::Committed(w) => w,
            Self::NotCommitted(e) => panic!("{msg}: proven not committed: {e}"),
            Self::Ambiguous(e) => panic!("{msg}: indeterminate: {e}"),
        }
    }
}

/// What the epoch-guarded retry proved (internal to the retry path;
/// the caller-facing closure set is [`CycleCommitResult`];
/// `pub(crate)` so the alphabet table in collect.rs can pin the
/// classification rows).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RetryVerdict {
    /// The guarded UPDATE applied (1 row): the retry landed the commit.
    Applied,
    /// 0 rows because the row already carries OUR write: the primary
    /// applied and only its response was lost — pure payload echo at
    /// or after the held own-attempt anchor (merged_bug_021).
    OwnCommitLanded,
    /// 0 rows and the row at `expected+1` carries a POSITIVELY
    /// mismatched payload (pure contradiction of our SET list):
    /// PROVEN lost. A stale or absent temporal anchor is NEVER this
    /// verdict (merged_bug_021 — a sibling can lawfully perturb any
    /// shared temporal ordering; only payload proves).
    ForeignWinner,
    /// 0 rows and the row evidence proves nothing (epoch past `+1`,
    /// the impossible `== expected` after a 0-row guarded UPDATE, or
    /// a matching payload whose own-attempt anchor is stale/absent —
    /// incl. the warn-tolerated stamp-failure path holding no
    /// anchor).
    Unprovable,
}

/// The held attempt stamp (merged_bug_021): the `last_attempt_at`
/// value OUR OWN `stamp_attempt` wrote, returned by the statement and
/// held as an OPAQUE TOKEN — `last_attempt_at::text` out,
/// `$1::timestamptz` back in (PG's own out/in round-trip,
/// microsecond-exact). The comparison stays DB-side, so the module's
/// "no timestamp crosses into process frame" law holds LITERALLY: the
/// process never parses, orders, or arithmetics the value. This is
/// the recognition anchor the shared `last_attempt_at` COLUMN cannot
/// be: any holder lawfully stamps the column with no dueness gate
/// (run_gc phase 3), so a sibling's stamp could interpose between our
/// applied-but-response-lost commit and the probe and forge a
/// "proven foreign" verdict out of our own landed write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OwnAttemptStamp(String);

impl OwnAttemptStamp {
    /// The `$n::timestamptz` bind value.
    fn as_str(&self) -> &str {
        &self.0
    }
}

/// One diagnostic read of the singleton row for 0-row retry
/// classification (merged_bug_022). Timestamp comparisons are computed
/// DB-side (booleans out) — no timestamp crosses into process frame,
/// so no cross-clock comparison exists (the merged_bug_017 class); the
/// own-attempt anchor crosses only as the opaque [`OwnAttemptStamp`]
/// token (merged_bug_021).
#[derive(Debug, Clone, PartialEq, Eq, sqlx::FromRow)]
pub(crate) struct CommitProbe {
    pub(crate) cycle_epoch: i64,
    pub(crate) cursor: Option<Vec<u8>>,
    pub(crate) backlog_estimate: Option<i64>,
    pub(crate) last_mark_set_size: Option<i64>,
    pub(crate) last_would_collect: Option<i64>,
    /// `last_live_cycle_at IS NOT NULL`.
    pub(crate) live_stamped: bool,
    /// `$1 IS NOT NULL AND last_live_cycle_at IS NOT NULL AND
    /// last_live_cycle_at >= $1` where `$1` is OUR OWN held attempt
    /// stamp (merged_bug_021) — false when we hold no stamp (the
    /// warn-tolerated stamp failure left None) or the live stamp
    /// predates OUR attempt. A sibling's `last_attempt_at` write is
    /// invisible here: the probe no longer reads the shared column,
    /// so the interposition is untypeable.
    pub(crate) live_since_own_attempt: bool,
}

const COMMIT_PROBE_SQL: &str = "SELECT cycle_epoch, cursor, backlog_estimate, \
       last_mark_set_size, last_would_collect, \
       last_live_cycle_at IS NOT NULL AS live_stamped, \
       ($1::timestamptz IS NOT NULL AND last_live_cycle_at IS NOT NULL \
          AND last_live_cycle_at >= $1::timestamptz) AS live_since_own_attempt \
  FROM gc_collect_state WHERE singleton";

/// The 0-row retry classification table (merged_bug_022; recognition
/// anchor merged_bug_021) — pure and exhaustive; the generated product
/// census in collect.rs pins every cell.
///
/// Echo fields are ONLY those whose committed value is a pure function
/// of the [`CycleCommit`] (the SET lists in `execute_commit`) — the
/// module's pure-echo law, now holding for the TEMPORAL leg too:
///
/// - Live, pure payload: `cursor == disposition.cursor_at_stop()` AND
///   `last_mark_set_size == observation.mark_set_size()` AND
///   `last_live_cycle_at IS NOT NULL` (`IS NOT NULL` is a pure
///   predicate of our SET list — a shadow never writes it). A
///   POSITIVE payload contradiction here is the ONLY thing that
///   proves the +1 commit was not ours (`ForeignWinner`).
/// - Live, recognition anchor: `live_since_own_attempt` — the live
///   stamp at-or-after OUR OWN held attempt stamp
///   ([`OwnAttemptStamp`], returned by `stamp_attempt`'s RETURNING;
///   compared DB-side). Its only writers are live commits (in the
///   quantified event set) and the comparand is a constant WE minted
///   — a sibling's `stamp_attempt` writes a column the probe no
///   longer reads. Excludes foreign-SHADOW misrecognition exactly as
///   the old shared-column conjunct intended: both live paths stamp
///   the attempt BEFORE the cycle, so a pre-existing stale live stamp
///   predates our stamp and fails the anchor.
/// - A failed anchor with a MATCHING payload downgrades to
///   `Unprovable`, never `ForeignWinner`: distinguishing "foreign
///   shadow with coincident payload" from "PG clock regression on our
///   own landed commit" would require exactly the cross-session
///   ordering assumption the pure-echo law forbids; the same lane
///   covers the held-stamp-absent case (the warn-tolerated stamp
///   failure) honestly.
/// - Shadow: `backlog_estimate == observation.would_collect()` AND
///   `last_would_collect == observation.would_collect()` AND
///   `last_mark_set_size == observation.mark_set_size()` (no temporal
///   leg ever existed; the payload-coincidence residual below).
///
/// DOCUMENTED BENIGN RESIDUAL: a byte-identical foreign LIVE commit
/// landing inside the milliseconds retry window is recognized as ours
/// — durable row state is identical by construction, so the only error
/// is WHICH replica's ok ticks. Accepted in lieu of a per-attempt
/// nonce column, which would require a new migration on the frozen
/// `gc_collect_state` schema (rio-migrations is outside this plane;
/// SIGNED Q2: zero DDL this wave). Modeled honestly as the
/// `payloadCoincidence` nondet in `docs/spec/models/gcCadence.qnt`.
pub(crate) fn classify_zero_row_retry(
    commit: &CycleCommit,
    expected_epoch: i64,
    probe: &CommitProbe,
) -> RetryVerdict {
    if probe.cycle_epoch != expected_epoch + 1 {
        // Past +1: ours MAY be one of several interleaved commits —
        // unprovable. == expected: impossible after a 0-row guarded
        // UPDATE (the guard would have matched); never invent proof
        // from an impossible state. Below expected: corrupted/reset
        // row — equally unprovable.
        return RetryVerdict::Unprovable;
    }
    match commit {
        CycleCommit::Live {
            disposition,
            observation,
            ..
        } => {
            let payload_matched = probe.cursor.as_deref() == disposition.cursor_at_stop()
                && probe.last_mark_set_size == Some(observation.mark_set_size())
                && probe.live_stamped;
            if !payload_matched {
                // Positive pure-payload contradiction: the +1 commit
                // provably was not ours.
                RetryVerdict::ForeignWinner
            } else if probe.live_since_own_attempt {
                RetryVerdict::OwnCommitLanded
            } else {
                // Payload matches but the recognition anchor is stale
                // or absent: never claim PROVEN foreign on the absence
                // of a temporal ordering (merged_bug_021).
                RetryVerdict::Unprovable
            }
        }
        CycleCommit::Shadow { observation } => {
            let echo_matched = probe.backlog_estimate == Some(observation.would_collect())
                && probe.last_would_collect == Some(observation.would_collect())
                && probe.last_mark_set_size == Some(observation.mark_set_size());
            if echo_matched {
                RetryVerdict::OwnCommitLanded
            } else {
                RetryVerdict::ForeignWinner
            }
        }
    }
}

/// Test-only production-statement router (Q1 witness provenance): the
/// foreign-winner red mints its foreign row through the SAME
/// `execute_commit` statement production uses — never hand-rolled SQL.
/// Routed via [`super::lock::SessionConn`] (the gc-wide acquire
/// discipline).
#[cfg(test)]
pub(crate) async fn commit_foreign_for_test(
    pool: &PgPool,
    commit: &CycleCommit,
) -> Result<u64, sqlx::Error> {
    let mut conn = super::lock::SessionConn::acquire(pool).await?;
    let rows = GcCycleLease::execute_commit(commit, None, &mut *conn.conn()).await?;
    conn.release_to_pool();
    Ok(rows)
}

/// Test-only production-statement router (Q1 witness provenance): the
/// sibling-stamp red drives a FOREIGN holder's `stamp_attempt` through
/// the SAME statement production uses — the lock-free dead-session
/// world where any replica lawfully stamps the shared column.
#[cfg(test)]
pub(crate) async fn stamp_attempt_foreign_for_test(pool: &PgPool) -> Result<(), sqlx::Error> {
    let mut conn = super::lock::SessionConn::acquire(pool).await?;
    let _foreign_stamp = GcCycleLease::execute_stamp_attempt(&mut *conn.conn()).await?;
    conn.release_to_pool();
    Ok(())
}

/// Test-only commit-fault injection carrier; the protocol is the
/// CLOSED [`CommitFaultMode`] alphabet (merged_bug_022 - the retired
/// magic-u8 protocol could not express applied-but-response-lost, so
/// the conflation it caused was untestable). Cleared on consume.
#[cfg(test)]
pub(crate) static COMMIT_FAIL_INJECT: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

/// The commit-fault axis, CLOSED (Q1 closure set): explicit
/// discriminants ride the existing [`COMMIT_FAIL_INJECT`] AtomicU8 and
/// every consult site decodes by EXHAUSTIVE match - adding a mode
/// cannot compile without each site taking a position.
#[cfg(test)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CommitFaultMode {
    /// No injection.
    Off = 0,
    /// The primary (lock-session) UPDATE is refused without executing.
    PrimaryRefused = 1,
    /// Primary refused AND the epoch-guarded retry refused.
    PrimaryAndRetryRefused = 2,
    /// The primary UPDATE EXECUTES (the row lands durably, through the
    /// production statement) and then the response is lost - exactly
    /// the applied-but-response-lost wire shape of a session killed
    /// between apply and acknowledge.
    AppliedResponseLost = 3,
    /// The post-commit release is refused: the release call is skipped
    /// and the lock dropped (drop detaches the connection, matching
    /// the failed-release semantics; lock.rs stays untouched).
    ReleaseRefused = 4,
}

#[cfg(test)]
impl CommitFaultMode {
    /// Arm the injection for the next [`GcCycleLease::commit_cycle`].
    pub(crate) fn arm(self) {
        COMMIT_FAIL_INJECT.store(self as u8, std::sync::atomic::Ordering::SeqCst);
    }

    fn decode(v: u8) -> Self {
        match v {
            0 => Self::Off,
            1 => Self::PrimaryRefused,
            2 => Self::PrimaryAndRetryRefused,
            3 => Self::AppliedResponseLost,
            4 => Self::ReleaseRefused,
            other => unreachable!("unknown CommitFaultMode discriminant {other}"),
        }
    }

    /// Peek without clearing (the primary consult site; later sites
    /// consume).
    fn peek() -> Self {
        Self::decode(COMMIT_FAIL_INJECT.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// Consume: read and clear.
    fn take() -> Self {
        Self::decode(COMMIT_FAIL_INJECT.swap(0, std::sync::atomic::Ordering::SeqCst))
    }
}

/// The held collect-cycle lease: the GC advisory lock plus the
/// lock-snapshot of the durable state. While this value lives, this
/// replica is the cluster's collector.
pub(crate) struct GcCycleLease {
    lock: PgSessionLock,
    pool: PgPool,
    pub(crate) state: GcCollectState,
    /// The attempt stamp OUR `stamp_attempt` wrote (merged_bug_021):
    /// the 0-row retry's recognition anchor. `None` at acquire and
    /// after a warn-tolerated stamp failure — that triple-fault path
    /// classifies `Unprovable`, honestly.
    own_attempt_stamp: Option<OwnAttemptStamp>,
}

impl GcCycleLease {
    /// Acquire the GC lock (non-blocking) and read the state through
    /// the lock's session. `Ok(None)` = another holder.
    pub(crate) async fn try_acquire(pool: &PgPool) -> Result<Option<Self>, sqlx::Error> {
        let Some(mut lock) = PgSessionLock::try_acquire(pool, super::GC_LOCK_ID).await? else {
            return Ok(None);
        };
        let state: GcCollectState = sqlx::query_as(SELECT_STATE)
            .fetch_one(&mut **lock.conn())
            .await?;
        Ok(Some(Self {
            lock,
            pool: pool.clone(),
            state,
            own_attempt_stamp: None,
        }))
    }

    /// Is a backstop cycle due at `interval`? Evaluated through the
    /// lock session on the DB clock — the double-check after the
    /// unlocked pre-read ([`backstop_due_unlocked`]).
    pub(crate) async fn backstop_due(
        &mut self,
        interval: std::time::Duration,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(BACKSTOP_DUE_SQL)
            .bind(interval.as_secs_f64())
            .fetch_one(&mut **self.lock.conn())
            .await
    }

    /// Stamp the live-cycle ATTEMPT (bug_284), through the lock
    /// session, BEFORE the cycle runs: every outcome arm — Ok,
    /// ParseFailure, Err, even a panic — inherits the stamp, so the
    /// "no outcome arm can produce a faster-than-documented retry
    /// cadence" quantifier is witnessed by sequencing, not by per-arm
    /// bookkeeping. Shadow (dry-run) cycles MUST NOT call this: a dry
    /// run never defers the live collection cadence.
    pub(crate) async fn stamp_attempt(&mut self) -> Result<(), sqlx::Error> {
        let stamp = Self::execute_stamp_attempt(&mut *self.lock.conn()).await?;
        // Held as the 0-row retry's recognition anchor
        // (merged_bug_021): the probe compares the live stamp against
        // THIS value — a constant we minted — never against the
        // shared column a sibling lawfully overwrites.
        self.own_attempt_stamp = Some(stamp);
        Ok(())
    }

    /// The one attempt-stamp statement (merged_bug_021): RETURNING the
    /// written value as text — the opaque [`OwnAttemptStamp`] token.
    /// Extracted so the test router below reuses the PRODUCTION
    /// statement (the `commit_foreign_for_test` pattern).
    async fn execute_stamp_attempt(
        conn: &mut sqlx::PgConnection,
    ) -> Result<OwnAttemptStamp, sqlx::Error> {
        let (stamp,): (String,) = sqlx::query_as(
            "UPDATE gc_collect_state SET last_attempt_at = now(), updated_at = now() \
             WHERE singleton RETURNING last_attempt_at::text",
        )
        .fetch_one(conn)
        .await?;
        Ok(OwnAttemptStamp(stamp))
    }

    // r[impl store.gc.collect-cadence+4]
    /// Commit a finished cycle to the row (epoch+1, stamps), then
    /// release the lock — three-valued (merged_bug_022): the caller
    /// receives [`CycleCommitResult`] and matches it exhaustively.
    ///
    /// THE WITNESS IS MINTED AT THE DURABILITY POINT: [`CycleCommitted`]
    /// is constructed in the expression observing the commit
    /// statement's success, BEFORE the release — the release is
    /// best-effort bookkeeping (lock.rs: a failed release detaches the
    /// connection and PG frees the lock with the session; drop
    /// detaches too), structurally unable to alter attribution.
    ///
    /// The primary UPDATE rides the lock's session; if that session
    /// died while it sat idle through the multi-minute cycle
    /// (pgbouncer/NLB idle killers, a PG restart — the lock connection
    /// does NOTHING during the cycle, merged_bug_218), the commit is
    /// retried ONCE on a fresh pooled connection, guarded by
    /// `cycle_epoch = <the epoch this lease read at acquire>`: the
    /// advisory lock was already freed with the dead session, so
    /// another replica may have started — the guard makes a stale late
    /// commit a no-op instead of a clobber. A 0-row retry is then
    /// classified on ROW EVIDENCE ([`classify_zero_row_retry`] — one
    /// diagnostic read on the same fresh connection, no third
    /// connection): in the applied-but-response-lost shape the row
    /// sits at `expected+1` from OUR OWN UPDATE, so the guard
    /// necessarily matches 0 rows — the retry must recognize its own
    /// landed commit instead of unconditionally claiming a foreign
    /// winner. Outcomes: own payload echo at the held
    /// attempt anchor → `Committed`; POSITIVE payload contradiction at
    /// `expected+1` → `NotCommitted` (proven, merged_bug_021); anything
    /// else (retry/diagnostic error, epoch past `+1`, matching payload
    /// with a stale/absent anchor) → `Ambiguous`.
    pub(crate) async fn commit_cycle(mut self, commit: CycleCommit) -> CycleCommitResult {
        let expected_epoch = self.state.cycle_epoch;
        let primary = {
            #[cfg(test)]
            {
                match CommitFaultMode::peek() {
                    CommitFaultMode::Off | CommitFaultMode::ReleaseRefused => {
                        Self::execute_commit(&commit, None, &mut *self.lock.conn()).await
                    }
                    CommitFaultMode::PrimaryRefused | CommitFaultMode::PrimaryAndRetryRefused => {
                        Err(sqlx::Error::Protocol(
                            "gc-collect: injected primary commit failure (test only)".into(),
                        ))
                    }
                    // The applied-but-response-lost shape: the row
                    // lands through the PRODUCTION statement, then the
                    // acknowledgement is lost.
                    CommitFaultMode::AppliedResponseLost => {
                        match Self::execute_commit(&commit, None, &mut *self.lock.conn()).await {
                            Ok(_) => Err(sqlx::Error::Protocol(
                                "gc-collect: injected response loss after applied commit \
                                 (test only)"
                                    .into(),
                            )),
                            Err(e) => Err(e),
                        }
                    }
                }
            }
            #[cfg(not(test))]
            {
                Self::execute_commit(&commit, None, &mut *self.lock.conn()).await
            }
        };
        match primary {
            Ok(_) => {
                // THE DURABILITY POINT: the witness is minted by the
                // expression observing execute_commit's Ok, before any
                // further fallible await — the release below cannot
                // alter attribution (merged_bug_022).
                let witness = CycleCommitted(());
                #[cfg(test)]
                {
                    match CommitFaultMode::take() {
                        CommitFaultMode::ReleaseRefused => {
                            // Skip the release and drop the lock: drop
                            // detaches, matching the failed-release
                            // semantics (lock.rs stays untouched).
                            tracing::warn!(
                                "gc-collect: post-commit lock release failed \
                                 (injected; lock freed via session close)"
                            );
                            drop(self.lock);
                            return CycleCommitResult::Committed(witness);
                        }
                        CommitFaultMode::Off
                        | CommitFaultMode::PrimaryRefused
                        | CommitFaultMode::PrimaryAndRetryRefused
                        | CommitFaultMode::AppliedResponseLost => {}
                    }
                }
                // Best-effort: a failed release detaches the
                // connection and PG frees the lock with the session
                // (lock.rs release error path; drop detaches too) —
                // warn, never re-attribute a landed commit.
                if let Err(e) = self.lock.release().await {
                    tracing::warn!(
                        error = %e,
                        "gc-collect: post-commit lock release failed \
                         (lock freed via session close; attribution unchanged)"
                    );
                }
                CycleCommitResult::Committed(witness)
            }
            Err(primary_e) => {
                tracing::warn!(
                    error = %primary_e,
                    expected_epoch,
                    "gc-collect: commit failed on the lock session; \
                     retrying once, epoch-guarded, on a fresh connection"
                );
                // The lock session is suspect — detach it (the
                // advisory lock dies with it; it may already be gone).
                drop(self.lock);
                let retry = {
                    #[cfg(test)]
                    {
                        match CommitFaultMode::take() {
                            CommitFaultMode::PrimaryAndRetryRefused => Err(sqlx::Error::Protocol(
                                "gc-collect: injected retry commit failure (test only)".into(),
                            )),
                            CommitFaultMode::Off
                            | CommitFaultMode::PrimaryRefused
                            | CommitFaultMode::AppliedResponseLost
                            | CommitFaultMode::ReleaseRefused => {
                                Self::retry_commit_on_fresh_conn(
                                    &self.pool,
                                    &commit,
                                    expected_epoch,
                                    self.own_attempt_stamp.as_ref(),
                                )
                                .await
                            }
                        }
                    }
                    #[cfg(not(test))]
                    {
                        Self::retry_commit_on_fresh_conn(
                            &self.pool,
                            &commit,
                            expected_epoch,
                            self.own_attempt_stamp.as_ref(),
                        )
                        .await
                    }
                };
                match retry {
                    Ok(RetryVerdict::Applied) => CycleCommitResult::Committed(CycleCommitted(())),
                    Ok(RetryVerdict::OwnCommitLanded) => {
                        CycleCommitResult::Committed(CycleCommitted(()))
                    }
                    Ok(RetryVerdict::ForeignWinner) => CycleCommitResult::NotCommitted(primary_e),
                    Ok(RetryVerdict::Unprovable) => CycleCommitResult::Ambiguous(primary_e),
                    Err(retry_e) => {
                        // The retry itself errored: the PRIMARY error
                        // may have been post-apply (response lost), so
                        // this is indeterminate — pre-fix it was
                        // reported as definitively failed.
                        tracing::warn!(
                            error = %retry_e,
                            expected_epoch,
                            "gc-collect: commit retry failed; outcome indeterminate \
                             (the primary error may have been post-apply)"
                        );
                        CycleCommitResult::Ambiguous(primary_e)
                    }
                }
            }
        }
    }

    /// The epoch-guarded retry on a FRESH connection, routed through
    /// [`super::lock::SessionConn`] (the gc-wide acquire discipline:
    /// the guard test bans bare `pool.acquire` in gc code). On 0 rows
    /// the SAME connection runs ONE diagnostic read
    /// ([`COMMIT_PROBE_SQL`]) and the pure table
    /// ([`classify_zero_row_retry`]) decides what the evidence proves
    /// — no third connection. On success/classification the connection
    /// goes back to the pool; on any error the drop detaches it — a
    /// suspect connection never re-enters the pool.
    async fn retry_commit_on_fresh_conn(
        pool: &PgPool,
        commit: &CycleCommit,
        expected_epoch: i64,
        own_attempt: Option<&OwnAttemptStamp>,
    ) -> Result<RetryVerdict, sqlx::Error> {
        let mut conn = super::lock::SessionConn::acquire(pool).await?;
        let rows = Self::execute_commit(commit, Some(expected_epoch), &mut *conn.conn()).await?;
        if rows >= 1 {
            conn.release_to_pool();
            return Ok(RetryVerdict::Applied);
        }
        // 0 rows: classify on row evidence before deciding anything.
        // The held own-attempt token rides the bind (NULL when the
        // stamp failed or never ran — that lane reads Unprovable).
        let probe: CommitProbe = sqlx::query_as(COMMIT_PROBE_SQL)
            .bind(own_attempt.map(OwnAttemptStamp::as_str))
            .fetch_one(&mut **conn.conn())
            .await?;
        conn.release_to_pool();
        let verdict = classify_zero_row_retry(commit, expected_epoch, &probe);
        match verdict {
            RetryVerdict::OwnCommitLanded => {
                tracing::info!(
                    expected_epoch,
                    observed_epoch = probe.cycle_epoch,
                    echo_matched = true,
                    "gc-collect: epoch-guarded retry matched 0 rows because OUR \
                     commit already landed (response lost); recognized by payload echo"
                );
            }
            RetryVerdict::ForeignWinner => {
                // The pre-fix unconditional claim survives ONLY here,
                // now evidenced (epoch at expected+1, foreign payload).
                tracing::warn!(
                    expected_epoch,
                    observed_epoch = probe.cycle_epoch,
                    echo_matched = false,
                    "gc-collect: another holder committed first; cycle stamp lost"
                );
            }
            RetryVerdict::Unprovable => {
                tracing::warn!(
                    expected_epoch,
                    observed_epoch = probe.cycle_epoch,
                    echo_matched = false,
                    "gc-collect: 0-row retry unclassifiable (epoch not at expected+1); \
                     commit indeterminate"
                );
            }
            RetryVerdict::Applied => {
                unreachable!("classification never yields Applied")
            }
        }
        Ok(verdict)
    }

    /// The one commit statement, parameterized by an optional epoch
    /// guard (the retry path). Returns rows_affected.
    async fn execute_commit(
        commit: &CycleCommit,
        epoch_guard: Option<i64>,
        conn: &mut sqlx::PgConnection,
    ) -> Result<u64, sqlx::Error> {
        let res = match commit {
            CycleCommit::Live {
                disposition,
                victims_collected,
                observation,
            } => {
                let guard = match epoch_guard {
                    Some(_) => " AND cycle_epoch = $6",
                    None => "",
                };
                // r[impl store.gc.completion-witness+2]
                let q = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE gc_collect_state SET \
                       cycle_epoch = cycle_epoch + 1, \
                       last_live_cycle_at = now(), \
                       cursor = $1, \
                       backlog_estimate = CASE \
                         WHEN $2 THEN 0 \
                         WHEN backlog_estimate IS NULL THEN GREATEST($5 - $3, 0) \
                         ELSE GREATEST(backlog_estimate - $3, 0) END, \
                       last_mark_set_size = $4, \
                       updated_at = now() \
                     WHERE singleton{guard}"
                )))
                .bind(disposition.cursor_at_stop().map(<[u8]>::to_vec))
                .bind(disposition.anchors_backlog_zero())
                .bind(*victims_collected as i64)
                .bind(observation.mark_set_size())
                .bind(observation.unmarked_backlog_seed());
                match epoch_guard {
                    Some(e) => q.bind(e).execute(conn).await?,
                    None => q.execute(conn).await?,
                }
            }
            CycleCommit::Shadow { observation } => {
                let guard = match epoch_guard {
                    Some(_) => " AND cycle_epoch = $3",
                    None => "",
                };
                let q = sqlx::query(sqlx::AssertSqlSafe(format!(
                    "UPDATE gc_collect_state SET \
                       cycle_epoch = cycle_epoch + 1, \
                       backlog_estimate = $1, \
                       last_would_collect = $1, \
                       last_mark_set_size = $2, \
                       updated_at = now() \
                     WHERE singleton{guard}"
                )))
                .bind(observation.would_collect())
                .bind(observation.mark_set_size());
                match epoch_guard {
                    Some(e) => q.bind(e).execute(conn).await?,
                    None => q.execute(conn).await?,
                }
            }
        };
        Ok(res.rows_affected())
    }

    /// Release without committing (the skip path: lease taken, cycle
    /// not run — e.g. the backstop's double-check found it not due).
    pub(crate) async fn release(self) -> Result<(), sqlx::Error> {
        self.lock.release().await
    }
}

/// Spawn the per-replica gauge publisher: every 60s, read the durable
/// row (unlocked) and publish the three collect gauges from it. Every
/// replica converges on the cluster value within one period —
/// "whichever pod ran the cycle" stops being an observability fact.
/// NULL fields leave their gauge untouched (the pre-registered 0
/// stands until the cluster has an observation).
pub fn spawn_gc_gauge_publisher(
    pool: PgPool,
    shutdown: rio_common::signal::Token,
) -> tokio::task::JoinHandle<()> {
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(60));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    rio_common::task::spawn_periodic_with("gc-gauge-publisher", ticker, shutdown, move || {
        let pool = pool.clone();
        async move {
            match read_state_unlocked(&pool).await {
                Ok(state) => {
                    tracing::trace!(
                        cycle_epoch = state.cycle_epoch,
                        "gc gauges published from the durable row"
                    );
                    publish_gauges(&state);
                }
                Err(e) => {
                    tracing::debug!(error = %e, "gc gauge publisher: state read failed");
                }
            }
        }
    })
}

/// Publish the three collect gauges from a state row (split out for
/// the test battery).
pub(crate) fn publish_gauges(state: &GcCollectState) {
    if let Some(backlog) = state.backlog_estimate {
        metrics::gauge!("rio_store_gc_collect_backlog_chunks").set(backlog as f64);
    }
    if let Some(live) = state.last_mark_set_size {
        metrics::gauge!("rio_store_gc_chunks_live").set(live as f64);
    }
    if let Some(wc) = state.last_would_collect {
        metrics::gauge!("rio_store_gc_chunks_would_collect").set(wc as f64);
    }
}
