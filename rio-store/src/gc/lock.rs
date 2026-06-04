//! Session-affine PG connections and the GC advisory-lock capability
//! (bug_213, bughunt wave D1).
//!
//! PG session state — an advisory lock, a session temp table — lives
//! on ONE connection and dies with it. The pre-wave choreography used
//! `scopeguard` with an explicit DEFUSE before the cleanup await
//! (`ScopeGuard::into_inner` … `pg_advisory_unlock(...).await`): a
//! failed OR cancelled cleanup after the defuse returned the
//! connection to the shared pool WITH the session state still
//! attached — the next `run_gc` then read "already running" until the
//! pool happened to recycle that connection. The types here make that
//! shape inexpressible: there is no public defuse, and the only way a
//! connection re-enters the pool is through a consuming method whose
//! contract is "session state is gone".

use sqlx::PgPool;
use sqlx::pool::PoolConnection;
use sqlx::postgres::Postgres;

/// A pooled connection whose session carries state that must NOT leak
/// back into the shared pool.
///
/// Drop ⇒ DETACH: the connection is removed from the pool and closed,
/// so PG frees every piece of session state (locks, temp tables) with
/// the session. The ONLY pool re-entry is [`SessionConn::release_to_pool`],
/// which the caller may invoke only once the session state is gone.
/// "Defuse before the cleanup await" cannot be written.
pub(crate) struct SessionConn {
    conn: Option<PoolConnection<Postgres>>,
}

impl SessionConn {
    pub(crate) fn new(conn: PoolConnection<Postgres>) -> Self {
        Self { conn: Some(conn) }
    }

    /// Acquire a pooled connection ALREADY wrapped: the RAII guard
    /// exists before the caller's first cancellation point, so a
    /// future dropped mid-probe/mid-setup detaches the connection
    /// instead of returning it to the pool with session state
    /// attached (merged_bug_223 — the pre-wrap window is
    /// unrepresentable; the raw pool checkout itself creates no
    /// session state, so cancellation inside it is safe).
    pub(crate) async fn acquire(pool: &PgPool) -> Result<Self, sqlx::Error> {
        Ok(Self::new(pool.acquire().await?))
    }

    /// The live connection. Panics only if called after consumption,
    /// which the consuming signatures make unreachable.
    pub(crate) fn conn(&mut self) -> &mut PoolConnection<Postgres> {
        self.conn
            .as_mut()
            .expect("SessionConn is live until consumed")
    }

    /// Hand the connection back to the pool. Contract: the session
    /// carries no state any more (lock released / temp table dropped
    /// in-session).
    pub(crate) fn release_to_pool(mut self) {
        // A plain drop of a PoolConnection returns it to the pool;
        // our Drop impl then sees None and does not detach.
        drop(self.conn.take());
    }
}

impl Drop for SessionConn {
    fn drop(&mut self) {
        if let Some(c) = self.conn.take() {
            // Detach closes the connection: PG frees the session
            // state. Errors are unobservable here by design — the
            // session is gone either way.
            let _ = c.detach();
        }
    }
}

/// A held PG session-scoped advisory lock (capability object).
///
/// `try_acquire` is non-blocking: `Ok(None)` means another session
/// holds the lock. While the value lives, the lock is held on its
/// dedicated connection ([`Self::conn`] runs cycle statements on it
/// where session affinity is wanted). Release paths:
///
/// - [`Self::release`]: unlock THROUGH the held connection; `Ok` ⇒
///   the connection returns to the pool; `Err` ⇒ the connection is
///   detached (closed) and PG frees the lock with the session.
/// - Drop (incl. future cancellation and panics): detach — never a
///   pooled connection with the lock held.
pub(crate) struct PgSessionLock {
    conn: SessionConn,
    lock_id: i64,
}

impl PgSessionLock {
    /// Acquire `lock_id` non-blockingly on a dedicated pooled
    /// connection. `Ok(None)` = held elsewhere (the connection used
    /// for the probe returns to the pool clean — a failed
    /// `pg_try_advisory_lock` leaves no session state).
    pub(crate) async fn try_acquire(
        pool: &PgPool,
        lock_id: i64,
    ) -> Result<Option<Self>, sqlx::Error> {
        // The guard exists BEFORE the probe await (merged_bug_223):
        // a cancellation while pg_try_advisory_lock is in flight —
        // after PG granted the lock server-side but before the client
        // read the result — drops `conn` ⇒ detach ⇒ PG frees the lock
        // with the session. The pre-fix shape held a bare
        // PoolConnection across that await; its drop returned the
        // lock-holding connection to the shared pool.
        let mut conn = SessionConn::acquire(pool).await?;
        let acquired: bool = sqlx::query_scalar("SELECT pg_try_advisory_lock($1)")
            .bind(lock_id)
            .fetch_one(&mut **conn.conn())
            .await?;
        if !acquired {
            // Clean miss: a failed try-lock leaves no session state.
            conn.release_to_pool();
            return Ok(None);
        }
        Ok(Some(Self { conn, lock_id }))
    }

    /// The lock-holding connection (for statements that must share the
    /// lock's session).
    pub(crate) fn conn(&mut self) -> &mut PoolConnection<Postgres> {
        self.conn.conn()
    }

    /// Unlock through the held connection, then return it to the pool.
    /// On error the connection is detached instead — the lock can
    /// never ride a pooled connection. Cancellation mid-await drops
    /// `self` ⇒ detach, same guarantee.
    pub(crate) async fn release(mut self) -> Result<(), sqlx::Error> {
        sqlx::query("SELECT pg_advisory_unlock($1)")
            .bind(self.lock_id)
            .execute(&mut **self.conn.conn())
            .await?;
        self.conn.release_to_pool();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    /// A scratch lock id distinct from the production GC id.
    const TEST_LOCK_ID: i64 = 0x7252_4c4b_7e57;

    /// (a) release() unlocks and returns the connection: a second
    /// acquire on the same (size-limited) pool succeeds immediately.
    #[tokio::test]
    async fn release_unlocks_and_returns_the_connection() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let lock = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID)
            .await
            .unwrap()
            .expect("first acquire wins");
        lock.release().await.unwrap();
        let again = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID)
            .await
            .unwrap();
        assert!(again.is_some(), "released lock is immediately acquirable");
        again.unwrap().release().await.unwrap();
    }

    /// (b) While held, a second acquire reports held-elsewhere.
    #[tokio::test]
    async fn second_acquire_is_none_while_held() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let held = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID)
            .await
            .unwrap()
            .expect("first acquire wins");
        let second = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID)
            .await
            .unwrap();
        assert!(second.is_none(), "lock is exclusive while held");
        held.release().await.unwrap();
    }

    /// merged_bug_223 source pin: every pooled acquire in gc/
    /// production sources routes through [`SessionConn::acquire`] —
    /// `pool.acquire(` appears nowhere outside this file (and exactly
    /// once inside it). Test modules are excluded (they may construct
    /// freely). RED (recorded pre-fix): lock.rs try_acquire's bare
    /// pre-wrap acquire + collect.rs cycle acquire + sweep.rs sweep
    /// acquire.
    #[test]
    fn gc_pool_acquires_route_through_session_conn() {
        fn production_half(src: &str) -> &str {
            // Cut at the tests MODULE (not the first cfg(test)
            // attribute — files carry cfg(test) consts mid-body).
            src.split("#[cfg(test)]\nmod tests").next().unwrap_or(src)
        }
        let outside = [
            ("collect.rs", include_str!("collect.rs")),
            ("sweep.rs", include_str!("sweep.rs")),
            ("mod.rs", include_str!("mod.rs")),
            ("state.rs", include_str!("state.rs")),
            ("orphan.rs", include_str!("orphan.rs")),
            ("drain.rs", include_str!("drain.rs")),
            ("mark.rs", include_str!("mark.rs")),
            ("tenant.rs", include_str!("tenant.rs")),
        ];
        for (name, src) in outside {
            let hits = production_half(src).matches("pool.acquire(").count();
            assert_eq!(
                hits, 0,
                "{name}: bare pool.acquire( in gc production code — route through SessionConn::acquire"
            );
        }
        let own = production_half(include_str!("lock.rs"))
            .matches("pool.acquire(")
            .count();
        assert_eq!(own, 1, "lock.rs holds the single pooled-acquire site");
    }

    /// merged_bug_223: cancelling `try_acquire` at ANY await point —
    /// including mid-probe after PG granted the lock server-side —
    /// must never return a lock-holding connection to the pool. Sweep
    /// cancellation points with stepped timeouts, then prove the lock
    /// is acquirable and no advisory lock leaked.
    #[tokio::test]
    async fn cancelled_acquire_never_pools_the_lock() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        for k in 0..30u64 {
            let fut = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID);
            // Stepped deadline: 0..~1.5ms cancels across the acquire
            // and probe awaits; longer steps complete normally.
            match tokio::time::timeout(std::time::Duration::from_micros(k * 50), fut).await {
                Ok(Ok(Some(l))) => l.release().await.unwrap(),
                Ok(Ok(None)) => panic!("lock contended in a single-test database"),
                Ok(Err(e)) => panic!("acquire error: {e}"),
                Err(_) => { /* cancelled mid-flight — the case under test */ }
            }
        }
        // After the sweep: the lock must be acquirable (no leaked
        // holder on a pooled connection) and exactly our hold shows.
        let mut held = None;
        for _ in 0..100 {
            if let Some(l) = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID)
                .await
                .unwrap()
            {
                held = Some(l);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        let held = held.expect("lock must be acquirable after cancellation sweep");
        let advisory: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory' AND objid = ($1::bigint & x'FFFFFFFF'::bigint)::oid",
        )
        .bind(TEST_LOCK_ID)
        .fetch_one(&db.pool)
        .await
        .unwrap();
        assert_eq!(
            advisory, 1,
            "exactly the live hold — no leaked advisory locks"
        );
        held.release().await.unwrap();
    }

    /// (c) Drop (the cancel/panic path) frees the lock via session
    /// close — a later acquire succeeds without any explicit unlock.
    #[tokio::test]
    async fn drop_frees_the_lock_via_session_close() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let held = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID)
            .await
            .unwrap()
            .expect("first acquire wins");
        drop(held);
        // The detach closes the connection asynchronously; PG frees
        // the lock when the session terminates. Poll briefly.
        let mut freed = false;
        for _ in 0..50 {
            if let Some(l) = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID)
                .await
                .unwrap()
            {
                l.release().await.unwrap();
                freed = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        assert!(freed, "dropping the lock must free it via session close");
    }
}
