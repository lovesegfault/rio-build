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
/// only mint sites are [`GcCycleLease::commit_cycle`]'s success paths,
/// so the `outcome="ok"` tick — [`CycleCommitted::record_ok_outcome`],
/// its sole producer — structurally cannot run for a cycle whose
/// stamp/cursor/backlog update was lost: metric attribution and the
/// commit result cannot diverge. `#[must_use]`: dropping the witness
/// without recording is a compile-time warning at every caller.
#[must_use = "record_ok_outcome() — the ok tick rides the commit witness"]
pub(crate) struct CycleCommitted(());

impl CycleCommitted {
    /// The ONLY producer of `rio_store_gc_collect_cycles_total{outcome="ok"}`.
    pub(crate) fn record_ok_outcome(self) {
        metrics::counter!("rio_store_gc_collect_cycles_total", "outcome" => "ok").increment(1);
    }
}

/// Test-only commit-failure injection: 0 = off, 1 = primary UPDATE
/// (lock session) fails, 2 = primary AND the epoch-guarded retry fail.
/// Cleared on consume.
#[cfg(test)]
pub(crate) static COMMIT_FAIL_INJECT: std::sync::atomic::AtomicU8 =
    std::sync::atomic::AtomicU8::new(0);

/// The held collect-cycle lease: the GC advisory lock plus the
/// lock-snapshot of the durable state. While this value lives, this
/// replica is the cluster's collector.
pub(crate) struct GcCycleLease {
    lock: PgSessionLock,
    pool: PgPool,
    pub(crate) state: GcCollectState,
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
        sqlx::query(
            "UPDATE gc_collect_state SET last_attempt_at = now(), updated_at = now() \
             WHERE singleton",
        )
        .execute(&mut **self.lock.conn())
        .await?;
        Ok(())
    }

    // r[impl store.gc.collect-cadence+2]
    /// Commit a finished cycle to the row (epoch+1, stamps), then
    /// release the lock. The primary UPDATE rides the lock's session;
    /// if that session died while it sat idle through the multi-minute
    /// cycle (pgbouncer/NLB idle killers, a PG restart — the lock
    /// connection does NOTHING during the cycle, merged_bug_218), the
    /// commit is retried ONCE on a fresh pooled connection, guarded by
    /// `cycle_epoch = <the epoch this lease read at acquire>`: the
    /// advisory lock was already freed with the dead session, so
    /// another replica may have started — the guard makes a stale
    /// late commit a no-op instead of a clobber. Success on either
    /// path mints the [`CycleCommitted`] witness; total failure
    /// returns the primary error and the caller ticks
    /// `outcome="commit_failed"` — never "ok".
    pub(crate) async fn commit_cycle(
        mut self,
        commit: CycleCommit,
    ) -> Result<CycleCommitted, sqlx::Error> {
        let expected_epoch = self.state.cycle_epoch;
        let primary = {
            #[cfg(test)]
            {
                use std::sync::atomic::Ordering;
                if COMMIT_FAIL_INJECT.load(Ordering::SeqCst) >= 1 {
                    Err(sqlx::Error::Protocol(
                        "gc-collect: injected primary commit failure (test only)".into(),
                    ))
                } else {
                    Self::execute_commit(&commit, None, &mut *self.lock.conn()).await
                }
            }
            #[cfg(not(test))]
            {
                Self::execute_commit(&commit, None, &mut *self.lock.conn()).await
            }
        };
        match primary {
            Ok(_) => {
                self.lock.release().await?;
                Ok(CycleCommitted(()))
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
                        use std::sync::atomic::Ordering;
                        if COMMIT_FAIL_INJECT.swap(0, Ordering::SeqCst) >= 2 {
                            Err(sqlx::Error::Protocol(
                                "gc-collect: injected retry commit failure (test only)".into(),
                            ))
                        } else {
                            Self::retry_commit_on_fresh_conn(&self.pool, &commit, expected_epoch)
                                .await
                        }
                    }
                    #[cfg(not(test))]
                    {
                        Self::retry_commit_on_fresh_conn(&self.pool, &commit, expected_epoch).await
                    }
                };
                match retry {
                    Ok(1) => Ok(CycleCommitted(())),
                    Ok(_) => {
                        tracing::warn!(
                            expected_epoch,
                            "gc-collect: epoch-guarded commit retry no-oped \
                             (another holder committed first); cycle stamp lost"
                        );
                        Err(primary_e)
                    }
                    Err(retry_e) => {
                        tracing::warn!(error = %retry_e, "gc-collect: commit retry failed");
                        Err(primary_e)
                    }
                }
            }
        }
    }

    /// The epoch-guarded retry on a FRESH connection, routed through
    /// [`super::lock::SessionConn`] (the gc-wide acquire discipline:
    /// the guard test bans bare `pool.acquire` in gc code). The
    /// statement is a single session-state-free UPDATE, so on success
    /// the connection goes straight back to the pool; on failure the
    /// drop detaches it -- a suspect connection never re-enters the
    /// pool.
    async fn retry_commit_on_fresh_conn(
        pool: &PgPool,
        commit: &CycleCommit,
        expected_epoch: i64,
    ) -> Result<u64, sqlx::Error> {
        let mut conn = super::lock::SessionConn::acquire(pool).await?;
        let rows = Self::execute_commit(commit, Some(expected_epoch), &mut *conn.conn()).await?;
        conn.release_to_pool();
        Ok(rows)
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
