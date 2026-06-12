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

    /// Parse the gc module tree's FILE-module declarations from the
    /// declaring `mod.rs` source (bug_002, R31's in-crate tier): the
    /// census universe DERIVES from the one authoritative declaration
    /// list instead of a hand-maintained second copy of the module
    /// tree. `cfg(test)`-gated declarations are lawfully outside the
    /// production universe (returned separately so the exclusion is
    /// ASSERTED, never silent); inline modules (brace form) are
    /// discharged via their HOST file's census row.
    fn file_modules(mod_src: &str) -> (Vec<String>, Vec<String>) {
        let mut production = Vec::new();
        let mut test_gated = Vec::new();
        let mut prev_cfg_test = false;
        for line in mod_src.lines() {
            let t = line.trim();
            if t == "#[cfg(test)]" {
                prev_cfg_test = true;
                continue;
            }
            let rest = t
                .strip_prefix("pub(crate) mod ")
                .or_else(|| t.strip_prefix("pub mod "))
                .or_else(|| t.strip_prefix("mod "));
            if let Some(rest) = rest
                && let Some(name) = rest.strip_suffix(';')
                && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            {
                if prev_cfg_test {
                    test_gated.push(name.to_string());
                } else {
                    production.push(name.to_string());
                }
            }
            if !t.is_empty() {
                prev_cfg_test = false;
            }
        }
        (production, test_gated)
    }

    /// merged_bug_223 source pin: every pooled acquire in gc/
    /// production sources routes through [`SessionConn::acquire`] —
    /// `pool.acquire(` appears nowhere outside this file (and exactly
    /// once inside it). Test modules are excluded (they may construct
    /// freely). RED (recorded pre-fix): lock.rs try_acquire's bare
    /// pre-wrap acquire + collect.rs cycle acquire + sweep.rs sweep
    /// acquire.
    ///
    /// THE UNIVERSE DERIVES (bug_002, R31's in-crate tier): the
    /// `include_str!` sibling array was a hand-maintained second copy
    /// of the module tree that went stale the moment a sibling landed
    /// — wave-10 added `lane.rs` without enrolling it, so a future
    /// bare `pool.acquire(` there would have silently bypassed this
    /// law. The array is now PINNED against the `mod.rs` declaration
    /// parse with TWO TYPED exceptions asserted rather than skipped:
    /// `lock` (the census home — discharged by the in-file
    /// exactly-once rule below, not an array row) and the
    /// cfg(test)-gated declarations (`mark_scan_bench` — lawfully
    /// outside the production universe, asserted gated); inline
    /// modules (`hold`) discharge via their host file's row (mod.rs
    /// itself is in the array). Population face: the parse must find
    /// at least one declaration (the resolve face is
    /// compile-discharged by `include_str!`).
    // r[verify store.gc.acquire-census-derived]
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
            ("lane.rs", include_str!("lane.rs")),
        ];

        // The universe derivation: every non-cfg(test) FILE module
        // declared by mod.rs has an array row (lock excepted, typed).
        let (declared, test_gated) = file_modules(include_str!("mod.rs"));
        assert!(
            !declared.is_empty(),
            "population face: the mod.rs parse found no declarations — \
             the grammar broke, not the tree"
        );
        assert!(
            test_gated.contains(&"mark_scan_bench".to_string()),
            "typed exception: mark_scan_bench is cfg(test)-gated (a \
             de-gating moves it into the derived universe and reds the \
             coverage check below)"
        );
        assert!(
            include_str!("mod.rs").contains("pub mod hold {"),
            "typed exception: hold is an INLINE module discharged via \
             mod.rs's own census row; if it moves to a file, the parse \
             enrolls it"
        );
        for m in &declared {
            if m == "lock" {
                // Typed exception: the census home itself — discharged
                // by the in-file exactly-once rule below.
                continue;
            }
            let file = format!("{m}.rs");
            assert!(
                outside.iter().any(|(f, _)| *f == file),
                "census universe drifted: mod.rs declares `{m}` but the \
                 include_str! array carries no `{file}` row — enroll the \
                 sibling (bug_002: a hand list is a second copy of the \
                 module tree)"
            );
        }

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

    // r[verify store.gc.acquire-census-derived]
    /// W12-V (bug_002): the planted red — a strawman FILE-module
    /// declaration outside the previously-scanned array population is
    /// auto-joined by the LIVE derivation (the jurisdiction face: the
    /// universe is the declaration list, so a new sibling cannot hide
    /// from it), and the coverage predicate goes red for it against
    /// the committed array. Driven through the SAME parse path as
    /// production.
    #[test]
    fn census_universe_plant_red() {
        let doctored = format!("{}\npub mod strawman_lane;\n", include_str!("mod.rs"));
        let (declared, _) = file_modules(&doctored);
        assert!(
            declared.contains(&"strawman_lane".to_string()),
            "the derivation auto-joins the planted declaration"
        );
        // The coverage predicate (the census's own check) reds: no
        // array row exists for the plant.
        let array_files = [
            "collect.rs",
            "sweep.rs",
            "mod.rs",
            "state.rs",
            "orphan.rs",
            "drain.rs",
            "mark.rs",
            "tenant.rs",
            "lane.rs",
        ];
        let missing: Vec<&String> = declared
            .iter()
            .filter(|m| *m != "lock")
            .filter(|m| !array_files.contains(&format!("{m}.rs").as_str()))
            .collect();
        assert_eq!(
            missing,
            vec![&"strawman_lane".to_string()],
            "exactly the plant is uncovered — the lawful exceptions stay \
             typed, never silently skipped"
        );
        // The cfg(test) polarity control: a GATED plant stays outside.
        let gated = format!(
            "{}\n#[cfg(test)]\nmod strawman_gated;\n",
            include_str!("mod.rs")
        );
        let (declared2, test_gated2) = file_modules(&gated);
        assert!(!declared2.contains(&"strawman_gated".to_string()));
        assert!(test_gated2.contains(&"strawman_gated".to_string()));
    }

    /// merged_bug_223: cancelling `try_acquire` at ANY await point —
    /// including mid-probe after PG granted the lock server-side —
    /// must never return a lock-holding connection to the pool. Sweep
    /// cancellation points with stepped timeouts, then prove
    /// STRUCTURALLY that no holder survived: the pg_locks population
    /// settles to zero and the lock is acquirable with exactly one
    /// hold showing.
    ///
    /// The defect this guards: a cancelled acquire RETURNING a
    /// lock-holding connection to the pool — a permanent holder no
    /// retry can clear. NOT the defect: transient `None` answers
    /// mid-sweep, which only mean a PREVIOUS iteration's cancelled
    /// future detached its connection and PostgreSQL has not finished
    /// closing it (the release is asynchronous by design). The old
    /// test panicked on that transient ("lock contended in a
    /// single-test database") — a wall-clock assumption that the
    /// async close outruns the next loop iteration. Three CI strikes
    /// under full-gate parallel load (round-2 S10, round-3 S5 gate,
    /// round-3 S6a dev-run) catalogued it; per the structural > retry
    /// > widen strategy the assertion now counts OBSERVED STATES:
    /// transients are counted (not fatal), and the invariant — zero
    /// holders once the connections finish dying — is asserted
    /// against pg_locks, which a pooled (leaked) holder would fail
    /// forever, not just under load.
    #[tokio::test]
    async fn cancelled_acquire_never_pools_the_lock() {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let mut transient_contention = 0u32;
        for k in 0..30u64 {
            let fut = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID);
            // Stepped deadline: 0..~1.5ms cancels across the acquire
            // and probe awaits; longer steps complete normally.
            match tokio::time::timeout(std::time::Duration::from_micros(k * 50), fut).await {
                Ok(Ok(Some(l))) => l.release().await.unwrap(),
                // A dying (detached) session from an earlier cancelled
                // iteration still holds the lock server-side: counted,
                // verified released below.
                Ok(Ok(None)) => transient_contention += 1,
                Ok(Err(e)) => panic!("acquire error: {e}"),
                Err(_) => { /* cancelled mid-flight — the case under test */ }
            }
        }

        // Structural settle: every cancelled future's connection was
        // detached (closed), so the advisory-lock population for this
        // id MUST reach zero — a holder that NEVER leaves pg_locks is
        // a lock-holding connection in the pool, the exact leak under
        // test. Bounded retries of a state observation, no wall-clock
        // assumption about HOW FAST the close lands.
        let count_holders = || async {
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM pg_locks WHERE locktype = 'advisory' \
                   AND objid = ($1::bigint & x'FFFFFFFF'::bigint)::oid",
            )
            .bind(TEST_LOCK_ID)
            .fetch_one(&db.pool)
            .await
            .unwrap()
        };
        let mut settled = false;
        for _ in 0..200 {
            if count_holders().await == 0 {
                settled = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        assert!(
            settled,
            "advisory holders never settled to zero: a cancelled acquire \
             left a lock-holding connection alive (the pooled-holder leak); \
             transient_contention={transient_contention}"
        );

        // And the lock is acquirable with exactly our hold showing.
        let held = PgSessionLock::try_acquire(&db.pool, TEST_LOCK_ID)
            .await
            .unwrap()
            .expect("lock must be acquirable once the holders settled");
        assert_eq!(
            count_holders().await,
            1,
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
