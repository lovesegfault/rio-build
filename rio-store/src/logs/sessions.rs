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
//! [`acquire`] steals it. All four operations are single statements —
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
/// Half of [`SESSION_STALE_AFTER`]: a lease survives one missed
/// heartbeat (a slow PG round-trip, a GC pause) but not two. The
/// constant is bound into the SQL as a `make_interval` parameter rather
/// than written as an `interval '...'` literal so the queries cannot
/// drift from it.
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

// One missed heartbeat survives, two do not — the lease math every
// doc in this module quotes. Compile-time so the shared const cannot
// drift away from the refresh cadence.
const _: () = assert!(
    HEARTBEAT_INTERVAL.as_secs() * 2 == SESSION_STALE_AFTER.as_secs(),
    "HEARTBEAT_INTERVAL must be half of SESSION_STALE_AFTER"
);

/// Bound on one heartbeat UPDATE round-trip inside the dedicated
/// heartbeat task ([`spawn_heartbeat`]). A call past the bound is
/// abandoned (warned) and the next tick retries — a hung PG connection
/// costs the cadence at most one in-bound delay, never the cadence
/// itself.
pub const HEARTBEAT_RPC_BOUND: Duration = Duration::from_secs(10);

// The liveness cadence's construction-grade bound (bug_148): the
// dedicated heartbeat task is the ONLY task on the cadence path, and
// its single await is bounded strictly inside the staleness margin —
// one hung-then-abandoned call delays the next beat attempt by at most
// HEARTBEAT_RPC_BOUND, so the inter-beat gap stays within
// HEARTBEAT_INTERVAL + HEARTBEAT_RPC_BOUND <= SESSION_STALE_AFTER: a
// single hang costs at most the one-missed-beat budget the lease math
// above already prices. The pre-fix shape kept the heartbeat as one
// select arm of the AppendLog driver, whose sibling arms awaited chunk
// cuts inline for up to one cut interval (60 s) plus an ack send (60 s
// more) — a healthy 31-60 s S3 PUT made the row stale mid-stream, a
// steal was admitted, and the owner's next heartbeat aborted a healthy
// stream. The companion census (`drive_loop_liveness_census`,
// service.rs) pins that no `sessions::heartbeat` call re-enters the
// driver loop.
const _: () = assert!(
    HEARTBEAT_RPC_BOUND.as_secs() <= SESSION_STALE_AFTER.as_secs() - HEARTBEAT_INTERVAL.as_secs(),
    "the heartbeat task's RPC bound must fit the staleness margin: \
     HEARTBEAT_RPC_BOUND <= SESSION_STALE_AFTER - HEARTBEAT_INTERVAL"
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
/// the same replica must not be locked out for 30 s by its own previous
/// session). When the `WHERE` rejects the steal, PostgreSQL reports the
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

    /// Stop the beat task and join it, bounded by one RPC bound (its
    /// longest possible in-flight await); abort as the backstop. The
    /// JOIN-OBLIGATION rule: the handle's owner MUST call this on
    /// every teardown path — a discarded handle is a beat task renewing
    /// a released lease until its next tick observes Lost.
    pub async fn stop(self) {
        self.stop.cancel();
        let mut task = self.task;
        if tokio::time::timeout(HEARTBEAT_RPC_BOUND, &mut task)
            .await
            .is_err()
        {
            // The task is parked past its own bound (only possible via
            // a pathological scheduler stall — every await it holds is
            // itself bounded). Abort it; the lease release that
            // follows is session-id-predicated, so a straggler beat
            // against a released row is a harmless zero-row UPDATE.
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
                    // The cadence path's ONLY await, bounded by the
                    // compile-asserted margin: a hang is abandoned and
                    // the next tick retries.
                    match tokio::time::timeout(
                        HEARTBEAT_RPC_BOUND,
                        heartbeat(&pool, exec_id, session_id),
                    )
                    .await
                    {
                        Ok(Ok(HeartbeatOutcome::Renewed)) => {}
                        Ok(Ok(HeartbeatOutcome::Lost)) => {
                            // Latch and exit: the lease belongs to a
                            // newer session; renewing further would
                            // fight the new owner's row.
                            let _ = lost_tx.send(true);
                            return;
                        }
                        // A PG blip. Keep going: the lease survives one
                        // missed heartbeat by construction (the beat vs
                        // 2x-beat staleness window), and if PG is down
                        // the chunk cuts are failing too — the driver's
                        // gray-failure abort fires there.
                        Ok(Err(e)) => {
                            warn!(%exec_id, error = %e, "ingest lease heartbeat failed");
                        }
                        Err(_elapsed) => {
                            warn!(
                                %exec_id,
                                bound_secs = HEARTBEAT_RPC_BOUND.as_secs_f64(),
                                "ingest lease heartbeat abandoned at its RPC bound"
                            );
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
        // 31s > the 30s staleness threshold: the owning replica crashed
        // without releasing and has missed two 15s heartbeats.
        seed_session(&db.pool, exec, dead, "other-pod", 31.0).await;

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
        // has gone stale. We must not lock ourselves out for 30s.
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
        seed_session(&db.pool, stale_exec, Uuid::now_v7(), "pod-b", 31.0).await;
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
