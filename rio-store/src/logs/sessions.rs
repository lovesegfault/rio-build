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
pub const SESSION_STALE_AFTER: Duration = Duration::from_secs(30);

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
    let won: Option<(Uuid,)> = sqlx::query_as(
        r"
        INSERT INTO log_ingest_sessions (exec_id, session_id, replica_pod)
        VALUES ($1, $2, $3)
        ON CONFLICT (exec_id) DO UPDATE SET
            session_id   = EXCLUDED.session_id,
            replica_pod  = EXCLUDED.replica_pod,
            started_at   = now(),
            heartbeat_at = now()
        WHERE log_ingest_sessions.heartbeat_at < now() - make_interval(secs => $4)
           OR log_ingest_sessions.replica_pod = EXCLUDED.replica_pod
        RETURNING session_id
        ",
    )
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

/// Where is `exec_id`'s live ingest stream, if anywhere?
///
/// `None` means no live session: either the execution is not currently
/// being ingested, or the row exists but its heartbeat is past
/// [`SESSION_STALE_AFTER`] (the owner is dead and the in-memory buffer
/// it held is gone). `TailLog` readers fall back to a history-only read
/// in both cases; the distinction does not matter to them.
pub async fn lookup_live(pool: &PgPool, exec_id: Uuid) -> Result<Option<LiveSession>, sqlx::Error> {
    let row: Option<(Uuid, String)> = sqlx::query_as(
        "SELECT session_id, replica_pod FROM log_ingest_sessions \
         WHERE exec_id = $1 AND heartbeat_at > now() - make_interval(secs => $2)",
    )
    .bind(exec_id)
    .bind(SESSION_STALE_AFTER.as_secs_f64())
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(session_id, replica_pod)| LiveSession {
        session_id,
        replica_pod,
    }))
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
        let live = lookup_live(&db.pool, exec).await.unwrap();
        assert_eq!(
            live.map(|l| (l.session_id, l.replica_pod)),
            Some((session, "this-pod".to_string())),
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
        assert!(lookup_live(&db.pool, exec).await.unwrap().is_some());
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
        let live = lookup_live(&db.pool, fresh_exec)
            .await
            .unwrap()
            .expect("fresh row is live");
        assert_eq!(live.session_id, fresh_session);
        assert_eq!(live.replica_pod, "pod-a");

        let stale_exec = Uuid::now_v7();
        seed_session(&db.pool, stale_exec, Uuid::now_v7(), "pod-b", 31.0).await;
        assert!(
            lookup_live(&db.pool, stale_exec).await.unwrap().is_none(),
            "a row past the staleness threshold is dead: readers must fall back to history-only"
        );

        // No row at all.
        assert!(
            lookup_live(&db.pool, Uuid::now_v7())
                .await
                .unwrap()
                .is_none()
        );
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
