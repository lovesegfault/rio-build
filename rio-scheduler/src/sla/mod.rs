//! ADR-023 SLA-driven per-derivation sizing.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use parking_lot::RwLock;

use crate::db::{SchedulerDb, SlaOverrideRow};

pub mod bootstrap;
pub mod config;
pub mod cost;
pub mod dip;
pub mod explain;
pub mod explore;
pub mod fit;
pub mod hw;
pub mod ingest;
pub mod metrics;
pub mod r#override;
pub mod prior;
pub mod quantile;
pub mod solve;
pub mod types;

/// cgroup poll interval (`executor::monitors`). Feeds the MAD floor in
/// [`ingest::is_outlier`] — a 1s sampler on a 10s build is ±10% wall-
/// clock noise; the gate's relative-granularity floor stops that being
/// flagged.
const DT_POLL_SECS: f64 = 1.0;

/// Minimum fitted-key count before [`prior::fleet_median`] is trusted.
/// Below this the fleet aggregate is noisier than the operator probe it
/// would override; ADR-023 §2.10 picks 50 as "enough distinct pnames
/// that one tenant's monoculture can't drag the median".
const FLEET_MEDIAN_MIN_KEYS: usize = 50;

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
    /// Non-expired `sla_overrides` rows. Full-table snapshot, refreshed
    /// each [`Self::refresh`] tick. Dispatch reads via
    /// [`Self::resolved_override`] (O(n) scan; n is operator-written —
    /// tens of rows). Separate `Arc` so the admin RPC can swap it
    /// out-of-band after `SetSlaOverride` without waiting for the next
    /// tick (phase-7; phase-6 just re-reads on tick).
    overrides: Arc<RwLock<Vec<SlaOverrideRow>>>,
    /// Per-hw_class median microbench factor (`hw_perf_factors` view).
    /// Refreshed each tick alongside overrides; passed by-ref into
    /// `ingest::refit` so wall-seconds → reference-seconds before T(c).
    hw: Arc<RwLock<hw::HwTable>>,
    /// ADR-023 §2.10 prior inputs. `seed` is loaded from
    /// `[sla].seed_corpus` at startup and/or `ImportSlaCorpus` at
    /// runtime; `fleet` is recomputed each refresh tick from the cache;
    /// `operator`/`default_tier_p90` are static from config. `None` ⇔
    /// `[sla]` unconfigured (refit gets `None`, no blending).
    priors: Option<Arc<RwLock<prior::PriorSources>>>,
    last_tick: RwLock<f64>,
    halflife_secs: f64,
    ring_buffer: u32,
}

impl SlaEstimator {
    pub fn new(halflife_secs: f64, ring_buffer: u32, cfg: Option<&config::SlaConfig>) -> Self {
        // Seed corpus load is best-effort: a missing/malformed file
        // warns and falls back to an empty seed table rather than
        // refusing to start. The corpus is an optimization (skip the
        // probe ladder), not a correctness input.
        let priors = cfg.map(|c| {
            let seed = c
                .seed_corpus
                .as_deref()
                .and_then(|p| match prior::SeedCorpus::load(p) {
                    Ok(corpus) => {
                        let (map, scale) = corpus.into_seed_map(&hw::HwTable::default());
                        tracing::info!(
                            entries = map.len(),
                            rescale = scale,
                            path = %p.display(),
                            "sla seed corpus loaded"
                        );
                        Some(map)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sla seed corpus load failed; starting empty");
                        None
                    }
                })
                .unwrap_or_default();
            let default_tier_p90 = c
                .tiers
                .iter()
                .find(|t| t.name == c.default_tier)
                .and_then(|t| t.p90)
                .unwrap_or(1200.0);
            Arc::new(RwLock::new(prior::PriorSources {
                seed,
                fleet: None,
                operator: c.probe.clone(),
                default_tier_p90,
            }))
        });
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            overrides: Arc::new(RwLock::new(Vec::new())),
            hw: Arc::new(RwLock::new(hw::HwTable::default())),
            priors,
            last_tick: RwLock::new(0.0),
            halflife_secs,
            ring_buffer,
        }
    }

    /// Snapshot the hw normalization table. For SlaExplain.
    pub fn hw_table(&self) -> hw::HwTable {
        self.hw.read().clone()
    }

    /// Snapshot the prior-source inputs. For
    /// `AdminService.ExportSlaCorpus` (which needs `seed.len()` for the
    /// "already seeded" hint) and tests.
    pub fn prior_sources(&self) -> Option<prior::PriorSources> {
        self.priors.as_ref().map(|p| p.read().clone())
    }

    /// Dump every cached fit with `n_eff ≥ min_n` as a [`SeedCorpus`].
    /// `tenant = Some(t)` restricts to one tenant's keys; `None`
    /// exports all. `ref_hw_class` is the current hw table's reference
    /// so an importer can rescale. Probe fits and `MemFit::Independent`
    /// keys are skipped — neither has the `(a, b)` a seed needs.
    ///
    /// [`SeedCorpus`]: prior::SeedCorpus
    pub fn export_corpus(&self, tenant: Option<&str>, min_n: u32) -> prior::SeedCorpus {
        let cache = self.cache.read();
        let entries = cache
            .values()
            .filter(|f| tenant.is_none_or(|t| f.key.tenant == t))
            .filter(|f| f.n_eff >= f64::from(min_n))
            .filter_map(|f| {
                let (s, p, q) = f.fit.spq();
                if !s.is_finite() {
                    return None; // Probe
                }
                let (a, b) = match &f.mem {
                    types::MemFit::Coupled { a, b, .. } => (*a, *b),
                    types::MemFit::Independent { .. } => return None,
                };
                // p_bar=∞ (Amdahl) → 0.0 sentinel: JSON has no Infinity
                // (serde_json emits null → round-trip parse fails on
                // non-optional f64). p_bar is informational here; the
                // prior only reads (s,p,q,a,b).
                let p_bar = f.fit.p_bar().0;
                Some(prior::SeedEntry {
                    pname: f.key.pname.clone(),
                    system: f.key.system.clone(),
                    s,
                    p,
                    q,
                    p_bar: if p_bar.is_finite() { p_bar } else { 0.0 },
                    a,
                    b,
                    n: f.n_eff as u32,
                })
            })
            .collect();
        prior::SeedCorpus {
            ref_hw_class: self.hw.read().reference.clone(),
            entries,
        }
    }

    /// Merge a parsed corpus into the seed-prior table, rescaling
    /// time-domain params via the current hw table. Returns `(entries,
    /// applied_factor)`. Err ⇔ `[sla]` unconfigured (no priors slot).
    pub fn import_seed(&self, corpus: prior::SeedCorpus) -> anyhow::Result<(usize, f64)> {
        let priors = self
            .priors
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("[sla] not configured; nowhere to import seeds"))?;
        let (map, scale) = corpus.into_seed_map(&self.hw.read());
        let n = map.len();
        priors.write().seed.extend(map);
        Ok((n, scale))
    }

    /// Snapshot one cached fit. `None` for never-seen keys — caller falls
    /// back to the cold-start probe path.
    pub fn cached(&self, key: &types::ModelKey) -> Option<types::FittedParams> {
        self.cache.read().get(key).cloned()
    }

    /// Drop one cached fit. Pairs with
    /// [`SchedulerDb::delete_build_samples_for_key`] for `ResetSlaModel`
    /// — next dispatch falls back to the cold-start probe path. Returns
    /// whether an entry was present.
    pub fn evict(&self, key: &types::ModelKey) -> bool {
        self.cache.write().remove(key).is_some()
    }

    /// Most-specific override matching `key` from the last-refreshed
    /// snapshot. Dispatch consults this BEFORE [`solve::intent_for`]'s
    /// fit/explore branch; `forced_cores`/`forced_mem` short-circuit the
    /// model entirely.
    pub fn resolved_override(&self, key: &types::ModelKey) -> Option<r#override::ResolvedTarget> {
        r#override::resolve(key, &self.overrides.read())
    }

    /// Snapshot the override cache. For `AdminService.SlaStatus` /
    /// `ListSlaOverrides`' actor-side view.
    pub fn overrides(&self) -> Vec<SlaOverrideRow> {
        self.overrides.read().clone()
    }

    /// Seed one entry. Test-only: bypasses the DB refit so dispatch
    /// integration tests can assert `intent_for(cached(key))` without an
    /// ephemeral PG round-trip.
    #[cfg(test)]
    pub fn seed(&self, fit: types::FittedParams) {
        self.cache.write().insert(fit.key.clone(), fit);
    }

    /// Test constructor: `[sla]`-configured estimator without touching
    /// disk. `cfg`'s `seed_corpus` is ignored (use [`Self::import_seed`]).
    #[cfg(test)]
    pub fn for_test(cfg: &config::SlaConfig) -> Self {
        let mut c = cfg.clone();
        c.seed_corpus = None;
        Self::new(c.halflife_secs, c.ring_buffer, Some(&c))
    }

    /// Seed the override cache. Test-only.
    #[cfg(test)]
    pub fn seed_overrides(&self, rows: Vec<SlaOverrideRow>) {
        *self.overrides.write() = rows;
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
        // Override snapshot first: cheap (operator-written, tens of
        // rows) and independent of the sample refit, so a PG blip on
        // the heavier incremental query below still leaves the override
        // cache fresh for this tick.
        match db.read_sla_overrides(None).await {
            Ok(rows) => *self.overrides.write() = rows,
            Err(e) => tracing::warn!(error = %e, "sla override refresh failed; keeping previous"),
        }
        // r[impl sched.sla.hw-ref-seconds]
        // Hw factor table: same cheap-and-independent treatment as
        // overrides (a few dozen rows from a view). Failure → keep
        // previous; an empty table is factor=1.0 everywhere.
        match hw::HwTable::load(db).await {
            Ok(t) => *self.hw.write() = t,
            Err(e) => tracing::warn!(error = %e, "sla hw-table refresh failed; keeping previous"),
        }
        let hw_snapshot = self.hw.read().clone();
        let priors_snapshot = self.priors.as_ref().map(|p| p.read().clone());

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
            let fit = ingest::refit(
                key,
                &rows,
                self.halflife_secs,
                prev.as_ref(),
                tiers,
                &hw_snapshot,
                priors_snapshot.as_ref(),
            );
            self.cache.write().insert(key.clone(), fit);
            // Trim AFTER refit so the fit always sees the full ring even
            // if a previous tick's trim was skipped (PG blip). Best-effort
            // — a failed trim is a transient leak the next tick fixes.
            let _ = db
                .trim_build_samples(&key.pname, &key.system, &key.tenant, self.ring_buffer)
                .await;
        }

        *self.last_tick.write() = hwm;

        // r[impl sched.sla.prior-partial-pool]
        // Fleet-median recompute AFTER the refit loop so this tick's new
        // fits feed the NEXT tick's prior (one-tick lag is fine — the
        // fleet aggregate moves slowly). Only Coupled-mem fits
        // contribute (FitParams.{a,b} are meaningless for Independent).
        if let Some(priors) = &self.priors {
            let coupled: Vec<(types::ModelKey, prior::FitParams)> = self
                .cache
                .read()
                .values()
                .filter_map(|f| {
                    let (s, p, q) = f.fit.spq();
                    if !s.is_finite() {
                        return None;
                    }
                    let types::MemFit::Coupled { a, b, .. } = f.mem else {
                        return None;
                    };
                    Some((f.key.clone(), prior::FitParams { s, p, q, a, b }))
                })
                .collect();
            priors.write().fleet = prior::fleet_median(&coupled, FLEET_MEDIAN_MIN_KEYS);
        }

        Ok(touched.len())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::{
        DurationFit, ExploreState, FittedParams, MemBytes, MemFit, ModelKey, RawCores, RefSeconds,
        WallSeconds,
    };

    fn cfg() -> config::SlaConfig {
        config::SlaConfig {
            tiers: vec![config::Tier {
                name: "normal".into(),
                p50: None,
                p90: Some(1200.0),
                p99: None,
            }],
            default_tier: "normal".into(),
            probe: config::ProbeShape {
                cpu: 4.0,
                mem_per_core: 2 << 30,
                mem_base: 4 << 30,
            },
            feature_probes: HashMap::new(),
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
            fuse_cache_budget: 0,
            log_budget: 0,
            ring_buffer: 32,
            halflife_secs: 7.0 * 86400.0,
            seed_corpus: None,
            hw_cost_source: None,
            hw_softmax_temp: 0.3,
            hw_fallback_after_secs: 120.0,
        }
    }

    fn fitted(pname: &str, n_eff: f64) -> FittedParams {
        FittedParams {
            key: ModelKey {
                pname: pname.into(),
                system: "x86_64-linux".into(),
                tenant: "t0".into(),
            },
            fit: DurationFit::Amdahl {
                s: RefSeconds(10.0),
                p: RefSeconds(400.0),
            },
            mem: MemFit::Coupled {
                a: 22.0,
                b: 0.5,
                r1: 0.9,
            },
            disk_p90: None,
            sigma_resid: 0.1,
            log_residuals: vec![],
            n_eff,
            span: 8.0,
            explore: ExploreState {
                distinct_c: 3,
                min_c: RawCores(2.0),
                max_c: RawCores(16.0),
                frozen: true,
                saturated: true,
                last_wall: WallSeconds(100.0),
            },
            t_min_ci: None,
            ci_computed_at: None,
            tier: None,
            hw_bias: HashMap::new(),
            prior_source: None,
        }
    }

    // r[verify sched.sla.prior-partial-pool]
    #[test]
    fn export_then_import_seeds_prior() {
        let src = SlaEstimator::for_test(&cfg());
        // 5 fitted keys, 1 below min_n.
        for (i, n) in [5.0, 6.0, 7.0, 8.0, 9.0, 1.0].into_iter().enumerate() {
            src.seed(fitted(&format!("pkg{i}"), n));
        }
        let corpus = src.export_corpus(None, 3);
        assert_eq!(corpus.entries.len(), 5, "n_eff=1 filtered");
        let json = serde_json::to_string(&corpus).unwrap();

        // Fresh estimator, import the corpus.
        let dst = SlaEstimator::for_test(&cfg());
        let parsed: prior::SeedCorpus = serde_json::from_str(&json).unwrap();
        let (n, _) = dst.import_seed(parsed).unwrap();
        assert_eq!(n, 5);

        // prior_for on a key from the corpus → Seed; on an unknown →
        // Operator (no fleet, seed miss).
        let priors = dst.prior_sources().unwrap();
        let hit = ModelKey {
            pname: "pkg0".into(),
            system: "x86_64-linux".into(),
            tenant: "any-tenant".into(),
        };
        let (_, prov) = prior::prior_for(&hit, &priors);
        assert_eq!(prov, prior::PriorSource::Seed);
        let miss = ModelKey {
            pname: "never-seen".into(),
            ..hit
        };
        let (_, prov) = prior::prior_for(&miss, &priors);
        assert_eq!(prov, prior::PriorSource::Operator);
    }

    #[test]
    fn export_tenant_filter() {
        let src = SlaEstimator::for_test(&cfg());
        src.seed(fitted("a", 5.0));
        let mut other = fitted("b", 5.0);
        other.key.tenant = "t1".into();
        src.seed(other);
        assert_eq!(src.export_corpus(Some("t0"), 3).entries.len(), 1);
        assert_eq!(src.export_corpus(None, 3).entries.len(), 2);
    }

    #[test]
    fn export_skips_probe_and_independent_mem() {
        let src = SlaEstimator::for_test(&cfg());
        let mut probe = fitted("p", 10.0);
        probe.fit = DurationFit::Probe;
        src.seed(probe);
        let mut indep = fitted("i", 10.0);
        indep.mem = MemFit::Independent { p90: MemBytes(0) };
        src.seed(indep);
        src.seed(fitted("ok", 10.0));
        assert_eq!(src.export_corpus(None, 3).entries.len(), 1);
    }

    #[test]
    fn import_fails_when_sla_unconfigured() {
        let est = SlaEstimator::new(7.0 * 86400.0, 32, None);
        let corpus = prior::SeedCorpus {
            ref_hw_class: "x".into(),
            entries: vec![],
        };
        assert!(est.import_seed(corpus).is_err());
    }
}
