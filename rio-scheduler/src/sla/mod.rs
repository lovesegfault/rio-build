//! ADR-023 SLA-driven per-derivation sizing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::db::SchedulerDb;

pub mod bootstrap;
pub mod config;
pub mod explore;
pub mod fit;
pub mod ingest;
pub mod metrics;
pub mod solve;
pub mod types;

/// cgroup poll interval (`executor::monitors`). Feeds the MAD floor in
/// [`ingest::is_outlier`] — a 1s sampler on a 10s build is ±10% wall-
/// clock noise; the gate's relative-granularity floor stops that being
/// flagged.
const DT_POLL_SECS: f64 = 1.0;

/// Cache of per-`ModelKey` [`FittedParams`](types::FittedParams). The
/// dispatch path reads via [`Self::cached`] (lock-free clone of one
/// entry); a background tick calls [`Self::refresh`] to refit only the
/// keys that gained new samples since the previous tick.
///
/// `last_tick` is a Unix-epoch f64 (matches `BuildSampleRow.completed_at`
/// — workspace sqlx has no chrono/time feature). It starts at 0.0 so the
/// first refresh is a full warm: every existing sample is "new".
pub struct SlaEstimator {
    cache: Arc<RwLock<HashMap<types::ModelKey, types::FittedParams>>>,
    last_tick: RwLock<f64>,
    halflife_secs: f64,
    ring_buffer: u32,
}

impl SlaEstimator {
    pub fn new(halflife_secs: f64, ring_buffer: u32) -> Self {
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            last_tick: RwLock::new(0.0),
            halflife_secs,
            ring_buffer,
        }
    }

    /// Snapshot one cached fit. `None` for never-seen keys — caller falls
    /// back to the cold-start probe path.
    pub fn cached(&self, key: &types::ModelKey) -> Option<types::FittedParams> {
        self.cache.read().get(key).cloned()
    }

    /// Seed one entry. Test-only: bypasses the DB refit so dispatch
    /// integration tests can assert `intent_for(cached(key))` without an
    /// ephemeral PG round-trip.
    #[cfg(test)]
    pub fn seed(&self, fit: types::FittedParams) {
        self.cache.write().insert(fit.key.clone(), fit);
    }

    /// Pull samples completed since the last tick, refit each touched
    /// key from its `ring_buffer` most-recent rows, swap into the cache.
    /// Returns the number of keys refit.
    ///
    /// The incremental query and the per-key reads are separate round-
    /// trips: incremental tells us *which* keys moved (cheap, indexed
    /// range scan); per-key gives the full ring including rows older
    /// than `last_tick` that the fit still needs. One refit per key per
    /// tick regardless of how many new rows landed.
    ///
    /// `tiers` (sorted tightest-first, as from
    /// [`config::SlaConfig::solve_tiers`]) feeds the Schmitt-trigger tier
    /// reassignment; empty → tier reassignment is a no-op.
    pub async fn refresh(&self, db: &SchedulerDb, tiers: &[solve::Tier]) -> anyhow::Result<usize> {
        let since = *self.last_tick.read();
        let new_rows = db.read_build_samples_incremental(since).await?;
        // High-water mark from the rows themselves, not wall-clock now():
        // avoids skipping a row that committed between the SELECT and the
        // tick-update under clock skew.
        let hwm = new_rows
            .iter()
            .map(|r| r.completed_at)
            .fold(since, f64::max);

        let touched: HashSet<types::ModelKey> = new_rows
            .iter()
            .map(|r| types::ModelKey {
                pname: r.pname.clone(),
                system: r.system.clone(),
                tenant: r.tenant.clone(),
            })
            .collect();

        for key in &touched {
            let prev = self.cache.read().get(key).cloned();
            // r[impl sched.sla.outlier-mad-reject]
            // BEFORE refit: score each NEW sample for this key against
            // the PREVIOUS fit. A 3·1.4826·MAD outlier is flagged in PG
            // (forensics-kept, fit-excluded — both per-key reads filter
            // `WHERE NOT outlier_excluded`). Using prev not new fit:
            // the new fit would already be contaminated by the outlier.
            if let Some(prev) = prev.as_ref() {
                for r in new_rows.iter().filter(|r| {
                    r.pname == key.pname && r.system == key.system && r.tenant == key.tenant
                }) {
                    if let Some(c) = r.cpu_limit_cores
                        && ingest::is_outlier(r.duration_secs, c, prev, DT_POLL_SECS)
                    {
                        let _ = db.mark_outlier_excluded(r.id).await;
                        metrics::outlier_rejected(&key.tenant);
                    }
                }
            }
            let rows = db
                .read_build_samples_for_key(&key.pname, &key.system, &key.tenant, self.ring_buffer)
                .await?;
            let fit = ingest::refit(key, &rows, self.halflife_secs, prev.as_ref(), tiers);
            self.cache.write().insert(key.clone(), fit);
            // Trim AFTER refit so the fit always sees the full ring even
            // if a previous tick's trim was skipped (PG blip). Best-effort
            // — a failed trim is a transient leak the next tick fixes.
            let _ = db
                .trim_build_samples(&key.pname, &key.system, &key.tenant, self.ring_buffer)
                .await;
        }

        *self.last_tick.write() = hwm;
        Ok(touched.len())
    }
}
