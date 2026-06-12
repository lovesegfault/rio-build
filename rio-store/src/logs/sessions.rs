//! The live-ingest session registry: a PG-backed lease over
//! `log_ingest_sessions` granting one `AppendLog` stream per execution.
//!
//! Two problems, one row per execution:
//!
//! 1. **Admission**: each `AppendLog` stream pins an in-memory ingest
//!    buffer on its replica. Without a lease, one assignment token could
//!    open many concurrent streams for the same execution and multiply
//!    that memory. [`acquire`] makes the second open lose with
//!    [`Acquire::Busy`] (→ gRPC `ALREADY_EXISTS`).
//! 2. **Routing**: a `TailLog` reader lands on an arbitrary replica via
//!    the Service; the live in-memory buffer lives on whichever replica
//!    the builder's stream landed on. [`lookup_live`] tells the reader
//!    which pod to proxy to.
//!
//! The lease is liveness-bounded, not transactional: the owning replica
//! refreshes `heartbeat_at` every [`HEARTBEAT_INTERVAL`]; a row whose
//! heartbeat is older than [`SESSION_STALE_AFTER`] is dead (the replica
//! crashed or was partitioned from PG without releasing) and the next
//! [`acquire`] steals it. A HEALTHY session survives one missed
//! heartbeat by construction: the staleness bound covers the worst
//! committed-stamp age of a one-miss session plus a strictly positive
//! slack (the compile-asserted margin law below, merged_bug_014 —
//! consumers evaluate the COMMITTED stamp's age, so the margin is
//! derived at their clock, not the producer's cadence). All
//! operations are single statements —
//! no connection is held across anything but its own query, so N
//! concurrent streams cost 0 pinned pool connections at steady state.
//!
//! The lease is an admission and routing mechanism, NOT a
//! mutual-exclusion guarantee. In the steal window (a deposed owner
//! that has not yet observed [`HeartbeatOutcome::Lost`] from its next
//! heartbeat) two sessions can ingest and cut chunks for the same
//! execution concurrently. The system is correct under that overlap
//! because chunk keys are session-scoped (no S3 collision) and the read
//! path dedups by line number (overlapping manifest rows union).
//! `Lost` tells the deposed owner to stop spending memory and to stop
//! acking — it does not mean its already-committed chunks are invalid,
//! and they must not be deleted.

use std::time::Duration;

use sqlx::PgPool;
use tokio::sync::watch;
use tracing::warn;
use uuid::Uuid;

/// How often the owning replica refreshes `heartbeat_at` while its
/// `AppendLog` stream is open.
///
/// A lease survives one missed heartbeat (a slow PG round-trip, a GC
/// pause): [`SESSION_STALE_AFTER`] covers the worst COMMITTED-stamp
/// age of a one-miss session — `2 × HEARTBEAT_INTERVAL +
/// HEARTBEAT_RPC_BOUND` — plus [`SESSION_MARGIN_SLACK`]; the compile
/// assert below certifies that inequality. The constant is bound into
/// the SQL as a `make_interval` parameter rather than written as an
/// `interval '...'` literal so the queries cannot drift from it.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

/// A session whose heartbeat is older than this is dead: its lease is
/// stealable by [`acquire`] and it is invisible to [`lookup_live`].
///
/// Derived from THE shared definition
/// ([`rio_migrations::sql::SESSION_STALE_AFTER_SECS`]) — the
/// scheduler's `gc_exec_rows` conjunct 5 and the store's stale-session
/// reap consume the same constant, so "live" cannot mean different
/// ages to different consumers (bug_234).
pub const SESSION_STALE_AFTER: Duration =
    Duration::from_secs(rio_migrations::sql::SESSION_STALE_AFTER_SECS as u64);

/// The consumer-clock slack (merged_bug_014, R29): the strictly
/// positive term separating [`SESSION_STALE_AFTER`] from the worst
/// committed-stamp age of a HEALTHY one-miss session. The slack is a
/// CERTIFIED term of the inequality below, not prose riding outside
/// it — a bare `≥` would admit `SESSION_STALE_AFTER ==
/// worst_one_miss_committed_age()` (the zero-margin boundary, the
/// exact collapse R29's boundary clause refuses), and const drift to
/// that boundary is a compile red. Re-derived at merged_bug_017's
/// close: the fast-retry budget joins the worst case
/// (2I + F + R = 42 s at the shipped consts), so the slack is the
/// remaining 3 s — the retry buys realistic-case resilience (a fast
/// PG blip no longer burns a whole interval) at a priced 2 s of
/// worst-case margin, recorded here rather than ridden silently.
pub const SESSION_MARGIN_SLACK: Duration = Duration::from_secs(3);

/// Attempts one heartbeat tick may make (merged_bug_017): the first
/// bounded attempt, plus at most one bounded retry when the first
/// returned a FAST error (see [`FAST_RETRY_BUDGET`]). A TIMED-OUT
/// attempt is terminal for its tick — a hang already spent the whole
/// RPC bound, and retrying after it is what let one tick occupy
/// 2×RPC_BOUND > INTERVAL, displacing the recovery tick the margin
/// certificate prices.
pub const HEARTBEAT_ATTEMPTS: u32 = 2;

/// The fast-retry window (merged_bug_017): a failed first attempt is
/// retried ONLY if it returned within this budget, so the tick body
/// is bounded by [`TICK_BODY_BOUND`] = FAST_RETRY_BUDGET +
/// HEARTBEAT_RPC_BOUND by construction — never by attempt counting
/// alone (an error arriving just under the RPC bound costs as much
/// as a hang; pricing attempts without pricing their LATENCY is how
/// the wave-11 retry broke the certificate).
pub const FAST_RETRY_BUDGET: Duration = Duration::from_secs(2);

/// THE tick-body bound (merged_bug_019, R33: one quantity, one
/// producer): the longest a heartbeat tick's body can occupy the
/// task — the fast-retry window plus one full bounded attempt.
/// Consumed by the margin certificate below, by
/// [`HeartbeatHandle::stop`]'s join bound, and by the narration —
/// consumers import, never re-derive.
pub const TICK_BODY_BOUND: Duration =
    Duration::from_secs(FAST_RETRY_BUDGET.as_secs() + HEARTBEAT_RPC_BOUND.as_secs());

/// The worst committed-stamp age of a HEALTHY one-miss session,
/// DERIVED from the executable schedule's own constants
/// (merged_bug_017, R29′): the t₀ beat commits; the t₀+I tick fails
/// entirely (its body ≤ [`TICK_BODY_BOUND`] ≤ I, so the recovery
/// tick is NOT displaced — the never-displace law is compile-asserted
/// below); the recovery tick fires at t₀+2I and its first commit
/// lands within one fast-error window plus one full bounded attempt.
/// Worst age: 2I + F + R. Adding an attempt, widening a bound, or
/// re-shaping the loop reddens the certificate mechanically — the
/// formula's inputs ARE the consts the loop executes.
pub const fn worst_one_miss_committed_age() -> Duration {
    Duration::from_secs(
        2 * HEARTBEAT_INTERVAL.as_secs()
            + FAST_RETRY_BUDGET.as_secs()
            + HEARTBEAT_RPC_BOUND.as_secs(),
    )
}

// THE staleness margin law (merged_bug_014, R29 — the conversion
// witness, both clocks named; re-derived from the executable schedule
// at merged_bug_017's close): "one missed heartbeat survives" is a
// claim about the CONSUMER-VISIBLE quantity — the age of the last
// COMMITTED heartbeat_at stamp when a consumer (the steal arm,
// lookup_live, the scheduler's gc conjunct) evaluates it — not about
// the producer's tick cadence. The worst committed age is the
// SCHEDULE-DERIVED `worst_one_miss_committed_age()` (2I + F + R),
// valid only while a tick body can never displace its successor —
// the second assert pins that structural premise. (The wave-11
// two-attempt retry broke the predecessor certificate exactly there:
// a fully-hung tick occupied 2×RPC_BOUND > INTERVAL, displaced the
// recovery tick, and a one-miss session's committed age reached
// I+3R = 45 s = SESSION_STALE_AFTER with zero margin — while the
// frozen 2I+R assert and its 40 s fixture passed green, the gap
// riding as a self-reported comment residual. The formula now takes
// the loop's own consts as inputs, so the next schedule change
// reddens the seal instead of the fleet.)
const _: () = assert!(
    SESSION_STALE_AFTER.as_secs()
        >= worst_one_miss_committed_age().as_secs() + SESSION_MARGIN_SLACK.as_secs(),
    "SESSION_STALE_AFTER must cover the schedule-derived worst \
     committed-stamp age of a one-miss session \
     (worst_one_miss_committed_age = 2I + FAST_RETRY_BUDGET + \
     HEARTBEAT_RPC_BOUND) plus SESSION_MARGIN_SLACK"
);
// The never-displace law: the recovery tick the worst-age formula
// schedules at t₀+2I exists only if no tick body can overrun the
// interval. This is the structural premise the wave-11 retry
// violated (2×RPC_BOUND = 20 s > 15 s).
const _: () = assert!(
    TICK_BODY_BOUND.as_secs() <= HEARTBEAT_INTERVAL.as_secs(),
    "a heartbeat tick's body must never displace its successor: \
     TICK_BODY_BOUND (FAST_RETRY_BUDGET + HEARTBEAT_RPC_BOUND) must fit \
     inside HEARTBEAT_INTERVAL"
);
const _: () = assert!(
    SESSION_MARGIN_SLACK.as_secs() > 0,
    "the consumer-clock slack must be strictly positive: STALE == \
     worst_one_miss_committed_age() is the zero-margin boundary collapse"
);

/// Bound on one heartbeat UPDATE round-trip inside the dedicated
/// heartbeat task ([`spawn_heartbeat`]). A call past the bound is
/// abandoned (warned) and the next tick retries — a hung PG connection
/// costs the cadence at most one in-bound delay, never the cadence
/// itself.
pub const HEARTBEAT_RPC_BOUND: Duration = Duration::from_secs(10);

// The liveness cadence's construction-grade bound (bug_148; priced
// from the ONE tick-body quantity per merged_bug_019): the dedicated
// heartbeat task is the ONLY task on the cadence path, and its tick
// body is bounded by TICK_BODY_BOUND — a fully-failed tick (hung, or
// fast-error-then-bounded-retry) delays the next beat attempt by at
// most that envelope, so the inter-beat gap stays within
// HEARTBEAT_INTERVAL + TICK_BODY_BOUND <= SESSION_STALE_AFTER: one
// bad tick costs at most the one-missed-beat budget the lease math
// above already prices. The pre-fix shape kept the heartbeat as one
// select arm of the AppendLog driver, whose sibling arms awaited chunk
// cuts inline for up to one cut interval (60 s) plus an ack send (60 s
// more) — a healthy 31-60 s S3 PUT made the row stale mid-stream, a
// steal was admitted, and the owner's next heartbeat aborted a healthy
// stream. The companion census (`drive_loop_liveness_census`,
// service.rs) pins that no `sessions::heartbeat` call re-enters the
// driver loop.
const _: () = assert!(
    TICK_BODY_BOUND.as_secs() <= SESSION_STALE_AFTER.as_secs() - HEARTBEAT_INTERVAL.as_secs(),
    "the heartbeat tick-body envelope must fit the staleness margin: \
     TICK_BODY_BOUND <= SESSION_STALE_AFTER - HEARTBEAT_INTERVAL"
);

/// A live ingest session, as seen by a `TailLog` reader deciding where
/// the in-memory tail of an execution's log lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveSession {
    pub session_id: Uuid,
    pub replica_pod: String,
}

/// Outcome of [`acquire`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Acquire {
    /// The lease is ours: the row was created, stolen from a stale
    /// owner, or re-taken from our own previous session.
    Acquired,
    /// Another replica holds a live (fresh-heartbeat) session for this
    /// execution. Maps to gRPC `ALREADY_EXISTS`. `current_owner` is the
    /// holding pod, for the error message and for operators chasing a
    /// stuck execution.
    Busy { current_owner: String },
}

/// Outcome of [`heartbeat`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HeartbeatOutcome {
    /// The lease is still ours; `heartbeat_at` was refreshed.
    Renewed,
    /// The lease was stolen (this replica's heartbeat went stale and
    /// another replica acquired the execution), or the row was deleted.
    /// The ingest task should abort its stream: stop heartbeating, stop
    /// acking, drop its in-memory buffer. Chunks it already committed
    /// remain valid — the new session's chunks will overlap them and
    /// the read path dedups by line number.
    Lost,
}

// r[impl store.log.session-keyed]
/// Take (or steal) the ingest lease for `exec_id`.
///
/// One `INSERT … ON CONFLICT DO UPDATE … WHERE <stealable> RETURNING`
/// round-trip. The conflict-update's `WHERE` admits the steal only when
/// the incumbent is stale (heartbeat older than [`SESSION_STALE_AFTER`])
/// or already ours (`replica_pod` matches — a builder reconnecting to
/// the same replica must not be locked out for the staleness window by
/// its own previous session). When the `WHERE` rejects the steal, PostgreSQL reports the
/// statement as having affected zero rows and `RETURNING` yields
/// nothing — that is the [`Acquire::Busy`] case, and a follow-up
/// point-`SELECT` fetches the incumbent's pod for the error payload.
/// (Two round-trips on the contended path, one on the happy path; a CTE
/// could merge them but the contended path is the rare one and the
/// two-step version keeps the statement readable.)
pub async fn acquire(
    pool: &PgPool,
    exec_id: Uuid,
    session_id: Uuid,
    replica_pod: &str,
) -> Result<Acquire, sqlx::Error> {
    // Staleness is NOT(live) of the ONE shared predicate — never a
    // second hand-written comparison (bug_234: complementary by
    // construction, including the exact-equality boundary).
    let steal_sql = format!(
        "INSERT INTO log_ingest_sessions (exec_id, session_id, replica_pod) \
         VALUES ($1, $2, $3) \
         ON CONFLICT (exec_id) DO UPDATE SET \
             session_id   = EXCLUDED.session_id, \
             replica_pod  = EXCLUDED.replica_pod, \
             started_at   = now(), \
             heartbeat_at = now() \
         WHERE NOT ({live}) \
            OR log_ingest_sessions.replica_pod = EXCLUDED.replica_pod \
         RETURNING session_id",
        live =
            rio_migrations::sql::live_ingest_session_sql("log_ingest_sessions.heartbeat_at", "$4")
    );
    // AssertSqlSafe: composed exclusively from const fragments
    // (rio-migrations) and literal bind placeholders — no runtime data
    // enters the text.
    let won: Option<(Uuid,)> = sqlx::query_as(sqlx::AssertSqlSafe(steal_sql))
        .bind(exec_id)
        .bind(session_id)
        .bind(replica_pod)
        .bind(SESSION_STALE_AFTER.as_secs_f64())
        .fetch_optional(pool)
        .await?;

    if won.is_some() {
        return Ok(Acquire::Acquired);
    }

    // The conflict-WHERE rejected the steal: a live session on another
    // replica holds the lease. Fetch the owner for the Busy payload.
    // Racing a concurrent release/expiry here is benign: the caller
    // turns Busy into ALREADY_EXISTS and the builder's reconnect retry
    // will win the next acquire.
    let owner: Option<(String,)> =
        sqlx::query_as("SELECT replica_pod FROM log_ingest_sessions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_optional(pool)
            .await?;
    Ok(Acquire::Busy {
        current_owner: owner.map(|(pod,)| pod).unwrap_or_default(),
    })
}

/// Refresh the lease. Call every [`HEARTBEAT_INTERVAL`] while the
/// `AppendLog` stream is open.
///
/// `UPDATE … WHERE exec_id AND session_id`: zero rows affected means
/// the row no longer carries our session (stolen or deleted) and the
/// caller gets [`HeartbeatOutcome::Lost`].
pub async fn heartbeat(
    pool: &PgPool,
    exec_id: Uuid,
    session_id: Uuid,
) -> Result<HeartbeatOutcome, sqlx::Error> {
    let result = sqlx::query(
        "UPDATE log_ingest_sessions SET heartbeat_at = now() \
         WHERE exec_id = $1 AND session_id = $2",
    )
    .bind(exec_id)
    .bind(session_id)
    .execute(pool)
    .await?;
    Ok(if result.rows_affected() == 1 {
        HeartbeatOutcome::Renewed
    } else {
        HeartbeatOutcome::Lost
    })
}

/// The lease row's current `session_id`, if a row exists — the
/// OWNERSHIP WITNESS for the two-store handoff (bug_010). Two
/// same-pod opens can both pass [`acquire`] (the same-pod arm admits
/// a steal from our own previous session), and the awaited PG acquire
/// can invert order against the synchronous registry insert that
/// follows it — so the registry's cancel decision must verify the
/// inserting session still owns the row, not assume it from insertion
/// order. "Displaced ⇒ displacer owns the row" is a checked
/// predicate, never a comment.
pub async fn current_session(pool: &PgPool, exec_id: Uuid) -> Result<Option<Uuid>, sqlx::Error> {
    sqlx::query_scalar("SELECT session_id FROM log_ingest_sessions WHERE exec_id = $1")
        .bind(exec_id)
        .fetch_optional(pool)
        .await
}

/// Drop the lease on clean stream close. Idempotent, and a no-op when
/// the row has already been stolen (the `session_id` predicate keeps a
/// stale teardown from deleting the new owner's row).
pub async fn release(pool: &PgPool, exec_id: Uuid, session_id: Uuid) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM log_ingest_sessions WHERE exec_id = $1 AND session_id = $2")
        .bind(exec_id)
        .bind(session_id)
        .execute(pool)
        .await?;
    Ok(())
}

/// The typed answer to "where is `exec_id`'s live ingest stream?" —
/// the registry view's THREE states, kept distinct so a reader's
/// history-only downgrade can disclose WHY (bug_148: the pre-fix
/// `Option` folded `Stale` into `Absent`, so a row gone stale behind a
/// healthy-but-parked owner silently downgraded every follower with no
/// operator-visible signal).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveLookup {
    /// A fresh-heartbeat session exists: route the reader to it.
    Live(LiveSession),
    /// A row exists but its heartbeat is past [`SESSION_STALE_AFTER`]:
    /// the owner died uncleanly OR (the bug_148 face) is alive but not
    /// renewing. History-only is the correct serve either way; the
    /// caller discloses the downgrade.
    Stale { replica_pod: String },
    /// No row at all: nothing is ingesting this execution. The normal
    /// case for any finished build — history-only, no disclosure owed.
    Absent,
}

/// Where is `exec_id`'s live ingest stream, if anywhere?
///
/// One round-trip: the row (if any) comes back WITH the shared
/// liveness predicate evaluated as a column, so `Stale` and `Absent`
/// stay distinguishable (the predicate itself is the single shared
/// definition — never a second hand-written comparison; bug_234).
pub async fn lookup_live(pool: &PgPool, exec_id: Uuid) -> Result<LiveLookup, sqlx::Error> {
    let live_sql = format!(
        "SELECT session_id, replica_pod, ({live}) AS live \
         FROM log_ingest_sessions WHERE exec_id = $1",
        live = rio_migrations::sql::live_ingest_session_sql("heartbeat_at", "$2")
    );
    // AssertSqlSafe: const-fragment composition, no runtime data.
    let row: Option<(Uuid, String, bool)> = sqlx::query_as(sqlx::AssertSqlSafe(live_sql))
        .bind(exec_id)
        .bind(SESSION_STALE_AFTER.as_secs_f64())
        .fetch_optional(pool)
        .await?;
    Ok(match row {
        Some((session_id, replica_pod, true)) => LiveLookup::Live(LiveSession {
            session_id,
            replica_pod,
        }),
        Some((_, replica_pod, false)) => LiveLookup::Stale { replica_pod },
        None => LiveLookup::Absent,
    })
}

/// The owner side of one dedicated session-heartbeat task
/// ([`spawn_heartbeat`]). The task renews the lease every
/// [`HEARTBEAT_INTERVAL`] until told to stop, the lease is lost, or
/// the row errors persistently; nothing else runs on it, so no sibling
/// await can starve the cadence (bug_148 — the R27 construction: the
/// liveness cadence lives where nothing can block it).
pub struct HeartbeatHandle {
    /// Latches `true` exactly once, when a heartbeat observes
    /// [`HeartbeatOutcome::Lost`]. The driver selects on `changed()`;
    /// a `RecvError` there means the task died WITHOUT observing Lost
    /// (panic) — the driver fails closed.
    lost: watch::Receiver<bool>,
    /// Tells the beat task to exit at teardown (after the final drain,
    /// before the lease release).
    stop: rio_common::signal::Token,
    task: tokio::task::JoinHandle<()>,
}

impl HeartbeatHandle {
    /// The Lost latch, for the driver's select arm.
    pub fn lost_watch(&self) -> watch::Receiver<bool> {
        self.lost.clone()
    }

    /// Stop the beat task and join it, bounded by [`TICK_BODY_BOUND`]
    /// (the ONE tick-body quantity, merged_bug_019: the longest the
    /// task can lawfully be in flight is its tick-body envelope —
    /// the join bound, the margin certificate, and the narration all
    /// import it, never re-derive it); abort as the backstop. The
    /// in-body stop select makes teardown prompt by construction
    /// (every attempt races the stop token, so a conformant task
    /// returns within a poll of the cancel, and the abort arm is
    /// genuinely unreachable for it — the bound is the certificate's
    /// envelope, not the expected wait). The JOIN-OBLIGATION rule:
    /// the handle's owner MUST call this on every teardown path — a
    /// discarded handle is a beat task renewing a released lease
    /// until its next tick observes Lost.
    pub async fn stop(self) {
        self.stop.cancel();
        let mut task = self.task;
        if tokio::time::timeout(TICK_BODY_BOUND, &mut task)
            .await
            .is_err()
        {
            // The task is parked past the tick-body envelope (only
            // possible via a pathological scheduler stall — every
            // await it holds is itself bounded AND races the stop
            // token). Abort it; the lease release that follows is
            // session-id-predicated, so a straggler beat against a
            // released row is a harmless zero-row UPDATE.
            task.abort();
            warn!("session heartbeat task did not stop within its bound; aborted");
        }
    }
}

/// Spawn the dedicated heartbeat task for one ingest session
/// (production cadence: [`HEARTBEAT_INTERVAL`] beats bounded by
/// [`HEARTBEAT_RPC_BOUND`]).
pub fn spawn_heartbeat(pool: PgPool, exec_id: Uuid, session_id: Uuid) -> HeartbeatHandle {
    spawn_heartbeat_with(pool, exec_id, session_id, HEARTBEAT_INTERVAL)
}

/// [`spawn_heartbeat`] with an injected cadence — the production
/// constructor pins the const; tests inject a fast interval so the
/// cadence laws are observable without 15 s real-time waits (the
/// injected-runner lane, disclosed: the BEAT itself is always the
/// production `heartbeat` UPDATE against a real pool).
pub(crate) fn spawn_heartbeat_with(
    pool: PgPool,
    exec_id: Uuid,
    session_id: Uuid,
    interval: Duration,
) -> HeartbeatHandle {
    let (lost_tx, lost_rx) = watch::channel(false);
    let stop = rio_common::signal::Token::new();
    let stop_task = stop.clone();
    let task = tokio::spawn(async move {
        let mut ticks = tokio::time::interval(interval);
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        // The first tick of a tokio interval fires immediately;
        // `acquire` just stamped heartbeat_at, so consume it.
        ticks.tick().await;
        loop {
            tokio::select! {
                _ = stop_task.cancelled() => return,
                _ = ticks.tick() => {
                    // Bounded beats under the TICK-BODY envelope
                    // (merged_bug_017): a TIMED-OUT attempt is
                    // terminal for the tick (a hang already spent the
                    // whole RPC bound), and a failed attempt retries
                    // only when it returned inside FAST_RETRY_BUDGET —
                    // so the body is bounded by TICK_BODY_BOUND ≤
                    // INTERVAL by construction (the never-displace
                    // law the margin certificate asserts), and the
                    // retry still buys the realistic case: a fast PG
                    // blip no longer donates a whole interval of
                    // committed-stamp age. The retry decision is
                    // `may_retry` — the policy the certificate
                    // prices, kept as one pure fn so the test cells
                    // and the loop cannot drift.
                    let tick_started = tokio::time::Instant::now();
                    for attempt in 0..HEARTBEAT_ATTEMPTS {
                        // Every attempt RACES the stop token
                        // (merged_bug_019's second arm): teardown
                        // during a hung or in-flight beat returns
                        // promptly instead of spending the attempt's
                        // bound — dropping the in-flight UPDATE is
                        // safe (the release that follows is
                        // session-id-predicated; a landed straggler
                        // is a refreshed stamp on a row about to be
                        // released).
                        let outcome = tokio::select! {
                            _ = stop_task.cancelled() => return,
                            outcome = tokio::time::timeout(
                                HEARTBEAT_RPC_BOUND,
                                heartbeat(&pool, exec_id, session_id),
                            ) => outcome,
                        };
                        match outcome {
                            Ok(Ok(HeartbeatOutcome::Renewed)) => break,
                            Ok(Ok(HeartbeatOutcome::Lost)) => {
                                // Latch and exit: the lease belongs to a
                                // newer session; renewing further would
                                // fight the new owner's row.
                                let _ = lost_tx.send(true);
                                return;
                            }
                            // A PG blip. If it returned fast, one
                            // bounded retry fits the body envelope;
                            // otherwise wait for the next tick: the
                            // lease survives one missed heartbeat by
                            // construction (the certified committed-
                            // stamp margin), and if PG is down the
                            // chunk cuts are failing too — the
                            // driver's gray-failure abort fires there.
                            Ok(Err(e)) => {
                                warn!(
                                    %exec_id, error = %e, attempt,
                                    "ingest lease heartbeat failed"
                                );
                                if !may_retry(false, tick_started.elapsed(), attempt + 1) {
                                    break;
                                }
                            }
                            Err(_elapsed) => {
                                warn!(
                                    %exec_id,
                                    bound_secs = HEARTBEAT_RPC_BOUND.as_secs_f64(),
                                    attempt,
                                    "ingest lease heartbeat abandoned at its RPC bound \
                                     (terminal for this tick)"
                                );
                                // Terminal-on-timeout: never retried.
                                debug_assert!(!may_retry(true, tick_started.elapsed(), attempt + 1));
                                break;
                            }
                        }
                    }
                }
            }
        }
    });
    HeartbeatHandle {
        lost: lost_rx,
        stop,
        task,
    }
}

/// The retry policy the tick body executes and the margin certificate
/// prices (merged_bug_017): may another bounded attempt start, given
/// what the last one did and when the tick began? TIMED-OUT attempts
/// are terminal (the hang spent the whole RPC bound); errors retry
/// only inside [`FAST_RETRY_BUDGET`] (so the body stays inside
/// [`TICK_BODY_BOUND`]); the attempt count caps at
/// [`HEARTBEAT_ATTEMPTS`]. One pure fn so the loop, the certificate's
/// derivation, and the test cells consume the SAME policy.
fn may_retry(last_was_timeout: bool, tick_elapsed: Duration, attempts_used: u32) -> bool {
    !last_was_timeout && tick_elapsed <= FAST_RETRY_BUDGET && attempts_used < HEARTBEAT_ATTEMPTS
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;
    use sqlx::PgPool;
    use uuid::Uuid;

    /// Seed a registry row directly. There is no production API that
    /// creates a row with an arbitrary heartbeat age, so tests that need
    /// a pre-existing (possibly stale) session write the row themselves.
    /// `age_secs` is subtracted from `now()` for both timestamps.
    async fn seed_session(pool: &PgPool, exec: Uuid, session: Uuid, pod: &str, age_secs: f64) {
        sqlx::query(
            "INSERT INTO log_ingest_sessions (exec_id, session_id, replica_pod, started_at, heartbeat_at) \
             VALUES ($1, $2, $3, now() - make_interval(secs => $4), now() - make_interval(secs => $4))",
        )
        .bind(exec)
        .bind(session)
        .bind(pod)
        .bind(age_secs)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Fetch the raw row for assertions (None if absent).
    async fn fetch_row(pool: &PgPool, exec: Uuid) -> Option<(Uuid, String)> {
        sqlx::query_as::<_, (Uuid, String)>(
            "SELECT session_id, replica_pod FROM log_ingest_sessions WHERE exec_id = $1",
        )
        .bind(exec)
        .fetch_optional(pool)
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn acquire_fresh_exec_succeeds() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let session = Uuid::now_v7();

        let outcome = acquire(&db.pool, exec, session, "this-pod").await.unwrap();
        assert!(matches!(outcome, Acquire::Acquired));

        // The row exists with our session/pod and a fresh heartbeat.
        assert_eq!(
            fetch_row(&db.pool, exec).await,
            Some((session, "this-pod".to_string()))
        );
        assert_eq!(
            lookup_live(&db.pool, exec).await.unwrap(),
            LiveLookup::Live(LiveSession {
                session_id: session,
                replica_pod: "this-pod".to_string(),
            }),
            "a just-acquired session must be visible as live (fresh heartbeat)"
        );
    }

    #[tokio::test]
    async fn acquire_conflicts_with_live_session_on_other_replica() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let theirs = Uuid::now_v7();
        seed_session(&db.pool, exec, theirs, "other-pod", 0.0).await;

        let outcome = acquire(&db.pool, exec, Uuid::now_v7(), "this-pod")
            .await
            .unwrap();
        match outcome {
            Acquire::Busy { current_owner } => assert_eq!(current_owner, "other-pod"),
            other => panic!("expected Busy, got {other:?}"),
        }

        // The losing acquire must not have touched the winner's row.
        assert_eq!(
            fetch_row(&db.pool, exec).await,
            Some((theirs, "other-pod".to_string()))
        );
    }

    #[tokio::test]
    async fn acquire_steals_stale_session() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let dead = Uuid::now_v7();
        // One past the staleness threshold: the owning replica crashed
        // without releasing and stopped beating entirely.
        seed_session(
            &db.pool,
            exec,
            dead,
            "other-pod",
            SESSION_STALE_AFTER.as_secs_f64() + 1.0,
        )
        .await;

        let session = Uuid::now_v7();
        let outcome = acquire(&db.pool, exec, session, "this-pod").await.unwrap();
        assert!(
            matches!(outcome, Acquire::Acquired),
            "a stale lease must be stealable"
        );

        // Every ownership column is replaced.
        assert_eq!(
            fetch_row(&db.pool, exec).await,
            Some((session, "this-pod".to_string()))
        );
        // And the steal refreshed the heartbeat: the row is live again.
        assert!(matches!(
            lookup_live(&db.pool, exec).await.unwrap(),
            LiveLookup::Live(_)
        ));
    }

    #[tokio::test]
    async fn acquire_steals_own_replica_session() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let old_session = Uuid::now_v7();
        // FRESH row owned by us: a builder reconnecting to the same
        // replica after a stream blip, before the old session's heartbeat
        // has gone stale. We must not lock ourselves out for the
        // staleness window.
        seed_session(&db.pool, exec, old_session, "this-pod", 0.0).await;

        let new_session = Uuid::now_v7();
        let outcome = acquire(&db.pool, exec, new_session, "this-pod")
            .await
            .unwrap();
        assert!(matches!(outcome, Acquire::Acquired));
        assert_eq!(
            fetch_row(&db.pool, exec).await,
            Some((new_session, "this-pod".to_string())),
            "the same replica re-acquiring must replace its own session_id"
        );
    }

    #[tokio::test]
    async fn release_deletes_only_own_session() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let session_a = Uuid::now_v7();
        seed_session(&db.pool, exec, session_a, "this-pod", 0.0).await;

        // A release carrying a session_id that no longer owns the row
        // (e.g. a stale teardown racing a steal) must be a no-op.
        release(&db.pool, exec, Uuid::now_v7()).await.unwrap();
        assert!(
            fetch_row(&db.pool, exec).await.is_some(),
            "foreign release must not delete"
        );

        // The owner's release deletes the row.
        release(&db.pool, exec, session_a).await.unwrap();
        assert!(fetch_row(&db.pool, exec).await.is_none());

        // Idempotent: releasing again is not an error.
        release(&db.pool, exec, session_a).await.unwrap();
    }

    #[tokio::test]
    async fn lookup_returns_owner_only_when_fresh() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        let fresh_exec = Uuid::now_v7();
        let fresh_session = Uuid::now_v7();
        seed_session(&db.pool, fresh_exec, fresh_session, "pod-a", 0.0).await;
        match lookup_live(&db.pool, fresh_exec).await.unwrap() {
            LiveLookup::Live(live) => {
                assert_eq!(live.session_id, fresh_session);
                assert_eq!(live.replica_pod, "pod-a");
            }
            other => panic!("fresh row must be Live, got {other:?}"),
        }

        // A row past the staleness threshold is TYPED stale — readers
        // fall back to history-only AND the downgrade is disclosable
        // (bug_148: the pre-fix Option folded this into Absent, hiding
        // a healthy-but-not-renewing owner from every operator signal).
        let stale_exec = Uuid::now_v7();
        seed_session(
            &db.pool,
            stale_exec,
            Uuid::now_v7(),
            "pod-b",
            SESSION_STALE_AFTER.as_secs_f64() + 1.0,
        )
        .await;
        assert_eq!(
            lookup_live(&db.pool, stale_exec).await.unwrap(),
            LiveLookup::Stale {
                replica_pod: "pod-b".to_string()
            },
            "a stale row must be distinguishable from no row at all"
        );

        // No row at all.
        assert_eq!(
            lookup_live(&db.pool, Uuid::now_v7()).await.unwrap(),
            LiveLookup::Absent
        );
    }

    /// W11-L (merged_bug_014, R29) + the merged_bug_017 re-derivation.
    /// Proposition: **one missed beat never makes a healthy session
    /// stealable** — at the consumer's clock, seeded at the
    /// SCHEDULE-DERIVED worst (`worst_one_miss_committed_age()` =
    /// 2I + F + R, 42 s shipped: the t₀ beat committed, the t₀+I tick
    /// fully failed inside its body envelope, the recovery tick's
    /// fast-error window plus one full bounded attempt). The fixture
    /// consumes the FORMULA, not a frozen figure — the wave-11 gap
    /// (the 40 s fixture certifying a schedule the same commit
    /// replaced) cannot recur: a schedule change moves this seed
    /// automatically. A truly dead session (past the bound) stays
    /// stealable — the margin is slack, not paralysis.
    // r[verify store.log.session-margin+2]
    #[tokio::test]
    async fn one_missed_beat_never_makes_a_healthy_session_stealable() {
        let db = TestDb::new(&crate::MIGRATOR).await;

        // The healthy one-miss worst case, at the consumer's clock —
        // derived from the executable schedule's own constants.
        let worst_committed_age = worst_one_miss_committed_age().as_secs_f64();
        let exec = Uuid::now_v7();
        let owner = Uuid::now_v7();
        seed_session(&db.pool, exec, owner, "owner-pod", worst_committed_age).await;

        let outcome = acquire(&db.pool, exec, Uuid::now_v7(), "thief-pod")
            .await
            .unwrap();
        assert!(
            matches!(outcome, Acquire::Busy { .. }),
            "left (pre-fix): a healthy session at the one-miss worst case \
             is read dead — the steal deposes a healthy owner mid-stream / \
             right: the bound covers the schedule-derived consumer-visible \
             quantity with positive slack, got {outcome:?}"
        );
        assert!(
            matches!(
                lookup_live(&db.pool, exec).await.unwrap(),
                LiveLookup::Live(_)
            ),
            "the same margin keeps lookup_live honest at the one-miss worst case"
        );

        // The dead cell: past the bound the steal still works.
        let dead_exec = Uuid::now_v7();
        seed_session(
            &db.pool,
            dead_exec,
            Uuid::now_v7(),
            "owner-pod",
            SESSION_STALE_AFTER.as_secs_f64() + 1.0,
        )
        .await;
        assert!(
            matches!(
                acquire(&db.pool, dead_exec, Uuid::now_v7(), "thief-pod")
                    .await
                    .unwrap(),
                Acquire::Acquired
            ),
            "a truly dead session stays stealable — the margin is slack, \
             not paralysis"
        );
    }

    /// W11-M (merged_bug_014): the certified law can go red at both
    /// collapse shapes. The margin formula — `STALE − (2I + R)` — is
    /// re-derived here as a plain function and pinned at three cells:
    /// the shipped constants (slack ≥ the certified term, > 0), the
    /// pre-fix 2× ratio (30 s: NEGATIVE margin — consumers read a
    /// healthy one-miss session dead, the strawman the old
    /// `I × 2 == STALE` assert certified as law), and the exact
    /// boundary (40 s: ZERO margin — the collapse a bare `≥` would
    /// admit, refused by the strictly-positive slack conjunct). The
    /// compile asserts in this module enforce the same inequality at
    /// build time; this test is their negation-capable twin.
    // r[verify store.log.session-margin+2]
    #[test]
    fn margin_law_rejects_both_collapse_shapes() {
        fn committed_stamp_margin(stale: i64, interval: i64, fast: i64, rpc_bound: i64) -> i64 {
            stale - (2 * interval + fast + rpc_bound)
        }
        let (i, f, r) = (
            HEARTBEAT_INTERVAL.as_secs() as i64,
            FAST_RETRY_BUDGET.as_secs() as i64,
            HEARTBEAT_RPC_BOUND.as_secs() as i64,
        );
        // The plain-function mirror equals the const formula.
        assert_eq!(
            (2 * i + f + r) as u64,
            worst_one_miss_committed_age().as_secs(),
            "the test mirror and the certificate share one formula"
        );
        // The shipped constants satisfy the certified law.
        let margin = committed_stamp_margin(SESSION_STALE_AFTER.as_secs() as i64, i, f, r);
        assert!(
            margin >= SESSION_MARGIN_SLACK.as_secs() as i64,
            "the slack is a certified term of the inequality"
        );
        assert!(margin > 0, "strictly positive at the shipped constants");
        // The pre-fix strawman: the 2× ratio the old assert certified.
        assert!(
            committed_stamp_margin(2 * i, i, f, r) < 0,
            "the 2× ratio (30 s) has NEGATIVE consumer-clock margin: the \
             compile-certified law was false"
        );
        // The exact-boundary strawman: STALE set to the worst age.
        assert_eq!(
            committed_stamp_margin(2 * i + f + r, i, f, r),
            0,
            "the worst-age boundary is the zero-margin collapse the \
             strictly-positive slack conjunct refuses"
        );
        // The merged_bug_017 strawman (the recorded gap): under the
        // wave-11 unbudgeted two-attempt schedule a fully-hung tick
        // displaced the recovery (body 2R > I) and the worst age
        // reached I + 3R = STALE exactly — zero margin — or I + 4R
        // past it; the frozen 2I+R certificate read both as covered.
        assert!(
            (i + 3 * r) >= SESSION_STALE_AFTER.as_secs() as i64,
            "the unbudgeted retry's worst case reaches the staleness bound \
             (the wave-11 red this close kills)"
        );
        // The never-displace premise at the shipped consts.
        assert!(
            TICK_BODY_BOUND.as_secs() <= HEARTBEAT_INTERVAL.as_secs(),
            "the tick body fits the interval (the recovery tick exists)"
        );
    }

    // r[verify store.log.session-margin+2]
    /// W12-F (merged_bug_017): the schedule's policy cells and the
    /// consumer-tier exhibit of the wave-11 red. Policy (the pure fn
    /// the loop executes): a timed-out attempt is TERMINAL; an error
    /// retries only inside the fast window; the attempt cap holds.
    /// Consumer tier: a session at the OLD schedule's worst committed
    /// age (I + 3R = 45 s) is read DEAD — the red recorded against
    /// the wave-11 schedule, kept live as the proof that ages past
    /// the bound stay fatal and the new schedule simply cannot
    /// produce them (worst_one_miss_committed_age() + slack ≤ STALE,
    /// compile-certified; the W11-L cell pins the new worst reads
    /// LIVE).
    #[tokio::test]
    async fn retry_policy_and_the_old_schedule_red() {
        // Terminal-on-timeout: never retried, at any elapsed.
        assert!(!may_retry(true, Duration::from_millis(1), 1));
        assert!(!may_retry(true, FAST_RETRY_BUDGET, 1));
        // Fast error: retried within the budget, refused past it.
        assert!(may_retry(false, Duration::from_millis(500), 1));
        assert!(!may_retry(
            false,
            FAST_RETRY_BUDGET + Duration::from_millis(1),
            1
        ));
        // The attempt cap: no third attempt ever.
        assert!(!may_retry(
            false,
            Duration::from_millis(1),
            HEARTBEAT_ATTEMPTS
        ));
        // The body envelope the certificate prices: a lawful retry
        // start plus one full bounded attempt fits TICK_BODY_BOUND.
        assert_eq!(
            FAST_RETRY_BUDGET + HEARTBEAT_RPC_BOUND,
            TICK_BODY_BOUND,
            "the envelope is the budget plus one bounded attempt"
        );

        // The consumer-tier exhibit: the OLD schedule's reachable
        // worst (I + 3R) reads dead at every consumer — pre-fix that
        // was a HEALTHY one-miss session (the red); post-fix the
        // schedule cannot produce this age (compile-certified).
        let db = TestDb::new(&crate::MIGRATOR).await;
        let old_worst = (HEARTBEAT_INTERVAL + 3 * HEARTBEAT_RPC_BOUND).as_secs_f64();
        assert!(
            old_worst >= SESSION_STALE_AFTER.as_secs_f64(),
            "the old schedule's worst case crossed the staleness bound"
        );
        let exec = Uuid::now_v7();
        seed_session(&db.pool, exec, Uuid::now_v7(), "owner-pod", old_worst).await;
        assert!(
            matches!(
                acquire(&db.pool, exec, Uuid::now_v7(), "thief-pod")
                    .await
                    .unwrap(),
                Acquire::Acquired
            ),
            "left (the wave-11 red, exhibited): a session at the unbudgeted \
             schedule's worst committed age is stolen — every consumer read \
             a healthy one-miss session dead / right: the schedule keeps \
             every healthy one-miss age strictly under the bound, so this \
             age now implies a genuinely dead session"
        );
    }

    /// W12-G (merged_bug_019, red-first at the const tier — the
    /// behavioral fixture is the green half): pre-fix, `stop()` joined
    /// bounded by ONE `HEARTBEAT_RPC_BOUND` while the wave-11 retry
    /// let the in-flight stretch reach 2× that bound — teardown
    /// spuriously aborted a correctly-behaving task mid-UPDATE with
    /// the misleading "pathological scheduler stall" warn (the
    /// schedule-change fan-out: the join bound, its "longest possible
    /// in-flight await" prose, and the inter-beat narration all
    /// priced the one-attempt loop the same commit replaced). The
    /// recorded red, at the constants: the old join bound
    /// (HEARTBEAT_RPC_BOUND = 10 s) is STRICTLY BELOW the lawful
    /// in-flight stretch (TICK_BODY_BOUND = 12 s) — asserted here as
    /// the strawman so the relation can never silently invert again.
    /// Post-fix: the join imports THE one tick-body const, and the
    /// in-body stop select makes teardown prompt by construction —
    /// `stop()` during a PG-parked (hung) beat returns within a poll
    /// of the cancel, never waiting out the attempt bound and never
    /// reaching the abort arm.
    #[tokio::test]
    async fn stop_is_prompt_during_a_hung_attempt() {
        // The const-tier red (the strawman relation): the pre-fix
        // join bound cannot cover the lawful in-flight stretch.
        assert!(
            HEARTBEAT_RPC_BOUND < TICK_BODY_BOUND,
            "strawman: the old per-attempt join bound is below the tick-body \
             envelope — the pre-fix stop() aborted lawful in-flight work"
        );

        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let session = Uuid::now_v7();
        seed_session(&db.pool, exec, session, "this-pod", 0.0).await;

        // Park the beat: an ACCESS EXCLUSIVE lock makes the heartbeat
        // UPDATE hang server-side (the hung-attempt face).
        let lock_pool = db.reopen().await;
        let mut tx = lock_pool.begin().await.expect("begin lock txn");
        sqlx::query("LOCK TABLE log_ingest_sessions IN ACCESS EXCLUSIVE MODE")
            .execute(&mut *tx)
            .await
            .expect("lock");

        // Fast cadence so the first beat fires (and parks) quickly.
        let handle =
            spawn_heartbeat_with(db.pool.clone(), exec, session, Duration::from_millis(50));
        tokio::time::sleep(Duration::from_millis(300)).await; // the beat is parked

        let t0 = std::time::Instant::now();
        handle.stop().await;
        let elapsed = t0.elapsed();
        drop(tx);
        // Slack budget, documented (the wall-clock-gate discipline):
        // the prompt path resolves in milliseconds; 5 s of scheduler
        // slack still discriminates 2.4× against the 12 s envelope
        // the abort arm would have to wait out.
        assert!(
            elapsed < Duration::from_secs(5),
            "stop() during a hung beat must return promptly via the stop \
             select (took {elapsed:?}; the abort arm would take at least \
             TICK_BODY_BOUND = {TICK_BODY_BOUND:?})"
        );
    }

    /// The dedicated heartbeat task (bug_148): beats land on their own
    /// cadence with NOTHING else on the task — a parked sibling
    /// elsewhere in the process cannot starve them by construction.
    /// Injected fast cadence (the disclosed `_with` runner lane); the
    /// beat itself is the production UPDATE against real PG.
    #[tokio::test]
    async fn heartbeat_task_beats_on_its_own_cadence() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let session = Uuid::now_v7();
        // Born 5 s ago so every beat measurably ADVANCES heartbeat_at.
        seed_session(&db.pool, exec, session, "this-pod", 5.0).await;
        let epoch = |pool: PgPool| async move {
            sqlx::query_scalar::<_, f64>(
                "SELECT EXTRACT(EPOCH FROM heartbeat_at)::float8 \
                 FROM log_ingest_sessions WHERE exec_id = $1",
            )
            .bind(exec)
            .fetch_one(&pool)
            .await
            .unwrap()
        };
        let t0 = epoch(db.pool.clone()).await;

        let handle =
            spawn_heartbeat_with(db.pool.clone(), exec, session, Duration::from_millis(100));
        // A beat lands within the cadence (poll, no wall-clock gate).
        let mut advanced = false;
        for _ in 0..100 {
            if epoch(db.pool.clone()).await > t0 {
                advanced = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(advanced, "the dedicated task must renew the row");
        let lost = handle.lost_watch();
        assert!(!*lost.borrow(), "no Lost while the row is ours");
        handle.stop().await;
    }

    /// Lost latches once and the task exits: a stolen lease stops the
    /// beats (the deposed owner must not fight the new owner's row),
    /// and the latch is the driver's abort signal.
    #[tokio::test]
    async fn heartbeat_task_latches_lost_on_steal() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let mine = Uuid::now_v7();
        seed_session(&db.pool, exec, mine, "this-pod", 0.0).await;

        let handle = spawn_heartbeat_with(db.pool.clone(), exec, mine, Duration::from_millis(100));
        let mut lost = handle.lost_watch();

        // Steal: another session takes the row.
        sqlx::query("UPDATE log_ingest_sessions SET session_id = $2 WHERE exec_id = $1")
            .bind(exec)
            .bind(Uuid::now_v7())
            .execute(&db.pool)
            .await
            .unwrap();

        tokio::time::timeout(Duration::from_secs(5), lost.changed())
            .await
            .expect("the Lost latch must fire within the cadence")
            .expect("the latch is sent before the task exits");
        assert!(*lost.borrow(), "Lost latches true");
        handle.stop().await;
    }

    #[tokio::test]
    async fn heartbeat_detects_lost_lease() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let exec = Uuid::now_v7();
        let mine = Uuid::now_v7();
        seed_session(&db.pool, exec, mine, "this-pod", 0.0).await;

        assert!(matches!(
            heartbeat(&db.pool, exec, mine).await.unwrap(),
            HeartbeatOutcome::Renewed
        ));

        // Simulate a steal: another session now owns the row.
        let theirs = Uuid::now_v7();
        sqlx::query("UPDATE log_ingest_sessions SET session_id = $2 WHERE exec_id = $1")
            .bind(exec)
            .bind(theirs)
            .execute(&db.pool)
            .await
            .unwrap();

        assert!(
            matches!(
                heartbeat(&db.pool, exec, mine).await.unwrap(),
                HeartbeatOutcome::Lost
            ),
            "a heartbeat against a stolen lease must report Lost so the ingest task aborts"
        );
    }
}
