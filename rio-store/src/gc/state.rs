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

/// `$1` = interval seconds (float). TRUE when no live cycle has ever
/// run or the last one is at least the interval old — evaluated on
/// the DB clock (no cross-replica clock enters the cadence decision).
const BACKSTOP_DUE_SQL: &str = "SELECT last_live_cycle_at IS NULL \
        OR (now() - last_live_cycle_at) >= make_interval(secs => $1) \
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
    /// the stop cursor, decrements the backlog estimate (floor 0).
    /// The cursor/backlog decision is taken from the typed
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

/// The held collect-cycle lease: the GC advisory lock plus the
/// lock-snapshot of the durable state. While this value lives, this
/// replica is the cluster's collector.
pub(crate) struct GcCycleLease {
    lock: PgSessionLock,
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
        Ok(Some(Self { lock, state }))
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

    // r[impl store.gc.collect-cadence]
    /// Commit a finished cycle to the row (epoch+1, stamps), then
    /// release the lock. Commit and unlock ride the SAME session: a
    /// failure detaches the connection, freeing the lock with the
    /// session and leaving the row at its previous epoch — the next
    /// holder re-reads consistent state.
    pub(crate) async fn commit_cycle(mut self, commit: CycleCommit) -> Result<(), sqlx::Error> {
        match commit {
            CycleCommit::Live {
                disposition,
                victims_collected,
                observation,
            } => {
                // r[impl store.gc.completion-witness+2]
                sqlx::query(
                    "UPDATE gc_collect_state SET \
                       cycle_epoch = cycle_epoch + 1, \
                       last_live_cycle_at = now(), \
                       cursor = $1, \
                       backlog_estimate = CASE \
                         WHEN $2 THEN 0 \
                         WHEN backlog_estimate IS NULL THEN NULL \
                         ELSE GREATEST(backlog_estimate - $3, 0) END, \
                       last_mark_set_size = $4, \
                       updated_at = now() \
                     WHERE singleton",
                )
                .bind(disposition.cursor_at_stop().map(<[u8]>::to_vec))
                .bind(disposition.anchors_backlog_zero())
                .bind(victims_collected as i64)
                .bind(observation.mark_set_size())
                .execute(&mut **self.lock.conn())
                .await?;
            }
            CycleCommit::Shadow { observation } => {
                sqlx::query(
                    "UPDATE gc_collect_state SET \
                       cycle_epoch = cycle_epoch + 1, \
                       backlog_estimate = $1, \
                       last_would_collect = $1, \
                       last_mark_set_size = $2, \
                       updated_at = now() \
                     WHERE singleton",
                )
                .bind(observation.would_collect())
                .bind(observation.mark_set_size())
                .execute(&mut **self.lock.conn())
                .await?;
            }
        }
        self.lock.release().await
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
