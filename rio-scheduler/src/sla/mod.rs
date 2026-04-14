//! ADR-023 SLA-driven per-derivation sizing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::db::SchedulerDb;

pub mod config;
pub mod fit;
pub mod ingest;
pub mod solve;
pub mod types;

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
    pub async fn refresh(&self, db: &SchedulerDb) -> anyhow::Result<usize> {
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
            let rows = db
                .read_build_samples_for_key(&key.pname, &key.system, &key.tenant, self.ring_buffer)
                .await?;
            let fit = ingest::refit(key, &rows, self.halflife_secs);
            self.cache.write().insert(key.clone(), fit);
        }

        *self.last_tick.write() = hwm;
        Ok(touched.len())
    }
}
