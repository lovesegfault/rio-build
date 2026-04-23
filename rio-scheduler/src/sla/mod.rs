//! ADR-023 SLA-driven per-derivation sizing.

use std::collections::{HashMap, HashSet};
use std::num::NonZeroUsize;
use std::sync::Arc;

use lru::LruCache;
use parking_lot::{Mutex, RwLock};

use crate::db::{SchedulerDb, SlaOverrideRow};

/// Startup-time guard for `[sla].reference_hw_class` changes. Replaces
/// the dead `SlaConfig::validate_reload` (which compared against a
/// `prev` config that doesn't exist across restarts — there is no
/// `[sla]` SIGHUP hot-reload, and helm rollout restarts the pod).
///
/// Reads `sla_config_epoch` (M_058) for `cluster`, compares the
/// persisted `reference_hw_class` against `cfg`'s. On first boot
/// (no row) records the current value at epoch 0. On mismatch:
///
/// - without `--allow-reference-change`: returns `Err` (caller maps to
///   process-abort) — the change invalidates every stored ref-second;
/// - with the flag: bumps `epoch`, records the new value, and resets
///   the ref-second-denominated state (`sla_ema_state` for this
///   cluster; `build_samples` + `hw_perf_samples` unconditionally —
///   neither is cluster-scoped, see `rio_store::migrations::M_058`).
///
/// Idempotent: a second boot with the same `cfg` is a no-op.
// r[impl sched.sla.hw-class.config]
pub async fn check_reference_epoch(
    db: &SchedulerDb,
    cfg: &config::SlaConfig,
    allow_reference_change: bool,
) -> anyhow::Result<()> {
    let cluster = cfg.cluster.as_str();
    let want = cfg.reference_hw_class.as_deref();
    let row: Option<(Option<String>, i64)> =
        sqlx::query_as("SELECT reference_hw_class, epoch FROM sla_config_epoch WHERE cluster = $1")
            .bind(cluster)
            .fetch_optional(db.pool())
            .await?;

    match row {
        None => {
            // First boot for this cluster — record, no reset.
            sqlx::query(
                "INSERT INTO sla_config_epoch (cluster, reference_hw_class, epoch) \
                 VALUES ($1, $2, 0) ON CONFLICT (cluster) DO NOTHING",
            )
            .bind(cluster)
            .bind(want)
            .execute(db.pool())
            .await?;
            tracing::info!(cluster, reference_hw_class = ?want, epoch = 0, "sla reference epoch initialised");
            Ok(())
        }
        Some((prev, _)) if prev.as_deref() == want => Ok(()),
        Some((prev, _)) if !allow_reference_change => anyhow::bail!(
            "sla.referenceHwClass changed {prev:?}→{want:?}; pass --allow-reference-change \
             to reset ref-second state (build_samples, sla_ema_state, hw_perf_samples) \
             and re-import corpus — all ref-seconds are invalidated"
        ),
        Some((prev, epoch)) => {
            tracing::warn!(
                cluster,
                prev = ?prev,
                new = ?want,
                epoch_from = epoch,
                epoch_to = epoch + 1,
                "sla.referenceHwClass changed with --allow-reference-change; \
                 RESETTING build_samples + hw_perf_samples (cluster-unscoped) + \
                 sla_ema_state[cluster] — re-import seed corpus"
            );
            let mut tx = db.pool().begin().await?;
            // sla_ema_state is cluster-scoped (M_043).
            sqlx::query("DELETE FROM sla_ema_state WHERE cluster = $1")
                .bind(cluster)
                .execute(&mut *tx)
                .await?;
            // build_samples / hw_perf_samples are NOT cluster-scoped —
            // see M_058 doc-const for the multi-cluster caveat.
            sqlx::query("TRUNCATE build_samples, hw_perf_samples")
                .execute(&mut *tx)
                .await?;
            sqlx::query(
                "UPDATE sla_config_epoch \
                 SET reference_hw_class = $2, epoch = epoch + 1, set_at = now() \
                 WHERE cluster = $1",
            )
            .bind(cluster)
            .bind(want)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            Ok(())
        }
    }
}

pub mod alpha;
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

/// Per-tenant fit-cache key: `(pname, system)`. The tenant lives in the
/// outer `HashMap` so the LRU bound is per-tenant
/// (`r[sched.sla.threat.corpus-clamp]` — one tenant's 10⁶ distinct
/// pnames can't OOM the scheduler or evict other tenants' fits).
type FitKey = (String, String);
type FitCache = HashMap<String, LruCache<FitKey, types::FittedParams>>;

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

/// Minimum distinct-tenant count before [`prior::fleet_median`] is
/// trusted. A "median-of-tenant-medians" with one tenant is that
/// tenant's median — not a fleet aggregate. Below this fall through to
/// [`prior::PriorSource::Operator`]. ADR-023 §Threat-model gap (b)
/// raised 2→5: with 2 tenants the "fleet median" IS one of them.
pub(super) const FLEET_MEDIAN_MIN_TENANTS: usize = 5;

/// Cache of per-`ModelKey` [`FittedParams`](types::FittedParams). The
/// dispatch path reads via [`Self::cached`] (lock-free clone of one
/// entry); a background tick calls [`Self::refresh`] to refit only the
/// keys that gained new samples since the previous tick.
///
/// `last_tick` is a Unix-epoch f64 (matches `BuildSampleRow.completed_at`
/// — workspace sqlx has no chrono/time feature). It starts at 0.0 so the
/// first refresh is a full warm: every existing sample is "new".
pub struct SlaEstimator {
    /// `tenant → LruCache<(pname, system), FittedParams>` capped at
    /// `[sla].max_keys_per_tenant`. ADR-023 §Threat-model: per-tenant
    /// LRU so one tenant submitting 10⁶ distinct pnames evicts only
    /// THEIR oldest fits, not other tenants'. `RwLock` (not per-tenant
    /// `Mutex`) because [`Self::cached`] needs `&mut LruCache` to bump
    /// recency — the dispatch hot-path takes a write guard either way.
    cache: Arc<RwLock<FitCache>>,
    /// `[sla].max_keys_per_tenant` — captured here so [`Self::insert`]
    /// can construct a fresh `LruCache` for a never-seen tenant.
    max_keys_per_tenant: NonZeroUsize,
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
    /// `operator`/`default_tier_target` are static from config.
    priors: Arc<RwLock<prior::PriorSources>>,
    /// `[sla].seed_corpus` parsed but NOT yet rescaled. `new()` runs
    /// before the first DB tick so `self.hw` is still the empty default
    /// (`factor(anything)=1.0`); converting there would silently bypass
    /// cross-fleet rescaling. [`Self::refresh`] one-shot-converts this
    /// after the first POPULATED `HwTable::load` (Ok ∧ non-empty).
    pending_seed: Mutex<Option<prior::ValidatedSeedCorpus>>,
    /// `[sla].default_tier`'s [`binding_bound`] in the operator-facing
    /// **wall-second** basis (as written in config). `refresh()`
    /// converts to ref-seconds (`× factor[hw.reference]`) and stores
    /// into `priors.default_tier_target` after each populated hw load,
    /// so [`prior::operator_to_spq`] / `clamp_to_operator` compare
    /// ref-second fleet medians against a ref-second basis. Contrast
    /// `reassign_tier`, which converts the OTHER direction (ref→wall
    /// via `/ min_factor`) to compare against `binding_bound`.
    ///
    /// [`binding_bound`]: solve::Tier::binding_bound
    default_tier_target_wall: f64,
    /// `[sla].cluster` — passed to `read_sla_overrides` so rows scoped
    /// to a different cluster are filtered at SQL read time.
    cluster: String,
    last_tick: RwLock<f64>,
    ring_buffer: u32,
}

impl SlaEstimator {
    pub fn new(cfg: &config::SlaConfig) -> Self {
        // Seed corpus load is best-effort: a missing/malformed file
        // warns and falls back to an empty seed table rather than
        // refusing to start. The corpus is an optimization (skip the
        // probe ladder), not a correctness input. Conversion to the
        // seed map (which rescales by hw factor) is DEFERRED to the
        // first refresh tick — `self.hw` is still empty here.
        let pending_seed =
            cfg.seed_corpus
                .as_deref()
                .and_then(|p| match prior::SeedCorpus::load(p, cfg) {
                    Ok(corpus) => {
                        tracing::info!(
                            entries = corpus.entries_len(),
                            path = %p.display(),
                            "sla seed corpus parsed; rescale deferred to first refresh"
                        );
                        Some(corpus)
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "sla seed corpus load failed; starting empty");
                        None
                    }
                });
        let default_tier_target_wall = cfg
            .tiers
            .iter()
            .find(|t| t.name == cfg.default_tier)
            .and_then(solve::Tier::binding_bound)
            .unwrap_or(1200.0);
        let priors = Arc::new(RwLock::new(prior::PriorSources {
            seed: HashMap::new(),
            fleet: None,
            operator: cfg.probe.clone(),
            // factor=1.0 until first populated hw load — same harmless
            // degradation as the rest of the empty-hw path.
            default_tier_target: default_tier_target_wall,
        }));
        Self {
            cache: Arc::new(RwLock::new(HashMap::new())),
            max_keys_per_tenant: NonZeroUsize::new(cfg.max_keys_per_tenant.max(1))
                .expect("max(1) ⇒ nonzero"),
            overrides: Arc::new(RwLock::new(Vec::new())),
            hw: Arc::new(RwLock::new(hw::HwTable::default())),
            priors,
            pending_seed: Mutex::new(pending_seed),
            default_tier_target_wall,
            cluster: cfg.cluster.clone(),
            last_tick: RwLock::new(0.0),
            ring_buffer: cfg.ring_buffer,
        }
    }

    /// Swap in a freshly-loaded hw table, plus the per-populated-load
    /// side effects (pending-seed rescale, operator-basis wall→ref).
    /// Separate from `refresh()` so it can be unit-tested without a
    /// `SchedulerDb`.
    fn apply_hw(&self, t: hw::HwTable) {
        // Pending seed conversion gated on a POPULATED load (Ok ∧
        // non-empty, checked inside apply_pending_seed): running it
        // against the empty default would rescale by factor=1.0
        // everywhere and `.take()` would consume the seed so the next
        // tick can't retry.
        self.apply_pending_seed(&t);
        if !t.is_empty() {
            // r[impl sched.sla.hw-ref-seconds]
            // binding_bound() is wall-seconds (operator-facing);
            // operator_to_spq / clamp_to_operator compare against
            // ref-second fleet medians. Convert at the chokepoint where
            // hw becomes available.
            self.priors.write().default_tier_target = self.default_tier_target_wall
                * t.factor(&t.reference)
                    .map_or(1.0, |f| alpha::dot(alpha::UNIFORM, f));
        }
        *self.hw.write() = t;
    }

    /// One-shot: rescale and merge any pending startup seed corpus
    /// against `hw`. Separate from `refresh()` so it can be unit-tested
    /// without a `SchedulerDb`.
    fn apply_pending_seed(&self, hw: &hw::HwTable) {
        if hw.is_empty() {
            // Ok-but-empty (no hw_class has ≥3 benched pods yet — i.e.
            // always on a fresh cluster) is functionally the Err arm:
            // scale would be 1.0/1.0 and `.take()` would consume the
            // seed so the next tick can't retry. Defer.
            return;
        }
        if let Some(corpus) = self.pending_seed.lock().take() {
            let (map, scale) = corpus.into_seed_map(hw);
            tracing::info!(
                entries = map.len(),
                rescale = scale,
                "sla seed corpus applied"
            );
            self.priors.write().seed.extend(map);
        }
    }

    /// Snapshot the hw normalization table. For SlaExplain.
    pub fn hw_table(&self) -> hw::HwTable {
        self.hw.read().clone()
    }

    /// Look up `α · factor[hw_class]` without cloning the whole table.
    /// `None` / unknown / <3-pod class → 1.0. Hot-path equivalent of
    /// `hw_table().normalize(1.0, h, α)` for the per-completion
    /// ref-seconds normalization. Caller supplies the per-pname α (from
    /// [`Self::cached_alpha`]).
    pub fn hw_factor(&self, hw_class: Option<&str>, alpha: alpha::Alpha) -> f64 {
        hw_class
            .and_then(|h| self.hw.read().factor(h))
            .map_or(1.0, |f| alpha::dot(alpha, f))
    }

    /// Per-pname α for `key` from the cache, or [`alpha::UNIFORM`] when
    /// unfitted. For per-completion metrics paths that need a scalar
    /// `α·factor` before the new sample lands in the ring. Same
    /// write-guard `.get()` recency-bump as [`Self::cached`].
    pub fn cached_alpha(&self, key: &types::ModelKey) -> alpha::Alpha {
        self.cache
            .write()
            .get_mut(&key.tenant)
            .and_then(|lru| lru.get(&(key.pname.clone(), key.system.clone())))
            .map_or(alpha::UNIFORM, |f| f.alpha)
    }

    /// Snapshot the prior-source inputs. For
    /// `AdminService.ExportSlaCorpus` (which needs `seed.len()` for the
    /// "already seeded" hint) and tests.
    pub fn prior_sources(&self) -> prior::PriorSources {
        self.priors.read().clone()
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
        let mut entries: Vec<prior::SeedEntry> = cache
            .iter()
            .filter(|(t, _)| tenant.is_none_or(|want| *t == want))
            .flat_map(|(_, lru)| lru.iter().map(|(_, f)| f))
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
                    // v2: f64 n_eff (the partial-pool weight). `version`/
                    // `alpha` are §13a fields populated by A13's
                    // per-hw_class residual computation.
                    n_eff: f.n_eff,
                    version: String::new(),
                    alpha: vec![],
                })
            })
            .collect();
        // tenant=None: cache is keyed by (pname, system, tenant) but
        // SeedEntry is tenant-agnostic (prior::SeedKey), so N tenants ×
        // gcc/x86_64-linux produce N entries with identical (pname,
        // system). into_seed_map's HashMap collect is last-write-wins on
        // HashMap iteration order — non-deterministic. Dedup HERE
        // (export, not import) so the on-disk corpus is clean: keep the
        // highest-n representative (most observations → most reliable
        // single fit), tie-break on (pname, system) for determinism.
        if tenant.is_none() {
            entries.sort_by(|a, b| {
                (&a.pname, &a.system)
                    .cmp(&(&b.pname, &b.system))
                    .then(b.n.cmp(&a.n))
                    .then(a.s.total_cmp(&b.s))
            });
            entries.dedup_by(|a, b| a.pname == b.pname && a.system == b.system);
        }
        prior::SeedCorpus {
            // §13a full factor vector populated by A13.
            ref_factor_vec: vec![],
            ref_hw_class: self.hw.read().reference.clone(),
            entries,
        }
    }

    /// Merge a parsed corpus into the seed-prior table, rescaling
    /// time-domain params via the current hw table. Returns `(entries,
    /// applied_factor)`. `applied_factor.is_nan()` ⇔ hw table not yet
    /// populated (RPC arrived before ≥3 pods/hw_class benched, e.g.
    /// `rio-cli sla import-corpus` scripted immediately after `helm
    /// install`); the corpus is stashed into `pending_seed` and
    /// rescaled on the next populated `refresh` tick.
    pub fn import_seed(&self, corpus: prior::ValidatedSeedCorpus) -> (usize, f64) {
        let hw = self.hw.read();
        if hw.is_empty() {
            let n = corpus.entries_len();
            *self.pending_seed.lock() = Some(corpus);
            return (n, f64::NAN);
        }
        let (map, scale) = corpus.into_seed_map(&hw);
        let n = map.len();
        self.priors.write().seed.extend(map);
        (n, scale)
    }

    /// Snapshot one cached fit. `None` for never-seen keys — caller falls
    /// back to the cold-start probe path. Takes the write guard:
    /// `LruCache::get` bumps recency (`&mut self`).
    pub fn cached(&self, key: &types::ModelKey) -> Option<types::FittedParams> {
        self.cache
            .write()
            .get_mut(&key.tenant)?
            .get(&(key.pname.clone(), key.system.clone()))
            .cloned()
    }

    /// Insert/replace one fit, creating the per-tenant LRU on first
    /// sight. Evicts the tenant's least-recently-used key if at cap;
    /// `on_evict` receives the evicted key so the caller can drop the
    /// paired [`solve::SolveCache`] entry (the bound there is "live
    /// SlaEstimator keys × overrides" — only holds if eviction here
    /// propagates).
    pub(crate) fn insert(
        &self,
        key: &types::ModelKey,
        fit: types::FittedParams,
        on_evict: impl FnOnce(&types::ModelKey),
    ) {
        let mut cache = self.cache.write();
        let lru = cache
            .entry(key.tenant.clone())
            .or_insert_with(|| LruCache::new(self.max_keys_per_tenant));
        let k = (key.pname.clone(), key.system.clone());
        // r[impl sched.sla.threat.corpus-clamp+2]
        // `LruCache::push` (unlike `put`) returns the EVICTED entry, so
        // an at-cap insert and an in-place overwrite are distinguishable
        // by `evicted_k != k`.
        if let Some((evicted_k, _)) = lru.push(k.clone(), fit)
            && evicted_k != k
        {
            ::metrics::counter!(
                "rio_scheduler_sla_keys_evicted_total",
                "tenant" => key.tenant.clone()
            )
            .increment(1);
            on_evict(&types::ModelKey {
                pname: evicted_k.0,
                system: evicted_k.1,
                tenant: key.tenant.clone(),
            });
        }
    }

    /// `T_min` (**ref-seconds**, hw-normalized — NOT wall-clock) for
    /// one cached key. `None` for never-seen keys AND for `Probe`-stage
    /// fits (n_eff<3 ∨ span<4) — caller falls back to
    /// `critical_path::DEFAULT_DURATION_SECS`. Feeds critical-path
    /// priority (D1): a relative ordering, so the point estimate at
    /// `min(p̄, c_opt)` suffices — no solve, no tier/ceiling dependency.
    ///
    /// r[sched.sla.hw-ref-seconds]: the fit ingests hw-normalized
    /// samples, so `t_min()` is in ref-seconds. Downstream consumers
    /// that surface this to users (`critical_path_remaining_secs`,
    /// `ca_cutoff_seconds_saved`) carry ref-seconds, NOT wall-clock —
    /// divide by fleet hw_factor for a wall-time estimate.
    ///
    /// `Probe.t_min() = ∞` MUST NOT propagate: priority is additive
    /// bottom-up (∞ taints the whole ancestor cone, defeating
    /// `INTERACTIVE_BOOST` and degenerating dispatch order to
    /// insertion-order), and `est_duration` feeds two metrics that
    /// saturate on ∞ (`ca_cutoff_seconds_saved` → `u64::MAX`;
    /// `critical_path_accuracy` → `actual/∞ = 0`). The flat default is
    /// the same "unfitted" treatment a never-seen key gets.
    pub fn ref_estimate(&self, key: &types::ModelKey) -> Option<f64> {
        self.cached(key)
            .map(|p| p.fit.t_min().0)
            .filter(|t| t.is_finite())
    }

    /// Drop one cached fit. Pairs with
    /// [`SchedulerDb::delete_build_samples_for_key`] for `ResetSlaModel`
    /// — next dispatch falls back to the cold-start probe path. Returns
    /// whether an entry was present.
    pub fn evict(&self, key: &types::ModelKey) -> bool {
        self.cache
            .write()
            .get_mut(&key.tenant)
            .is_some_and(|lru| lru.pop(&(key.pname.clone(), key.system.clone())).is_some())
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
        let key = fit.key.clone();
        self.insert(&key, fit, |_| {});
    }

    /// Test constructor: `[sla]`-configured estimator without touching
    /// disk. `cfg`'s `seed_corpus` is ignored (use [`Self::import_seed`]).
    #[cfg(test)]
    pub fn for_test(cfg: &config::SlaConfig) -> Self {
        let mut c = cfg.clone();
        c.seed_corpus = None;
        Self::new(&c)
    }

    /// Total live `FittedParams` across all per-tenant LRUs. Test-only
    /// accessor for the `sla_contract` invariant
    /// `solve_cache.len() ≤ live_fit_count()` (the `on_evict` hook is
    /// what makes that bound hold).
    #[cfg(test)]
    pub(crate) fn live_fit_count(&self) -> usize {
        self.cache.read().values().map(|lru| lru.len()).sum()
    }

    /// Seed the hw-factor table. Test-only: bypasses the
    /// `hw_perf_factors` view so `solve_full` sees a populated
    /// `HwTable` without an ephemeral PG round-trip.
    #[cfg(test)]
    pub fn seed_hw(&self, t: hw::HwTable) {
        *self.hw.write() = t;
    }

    /// Seed the override cache. Test-only.
    #[cfg(test)]
    pub fn seed_overrides(&self, rows: Vec<SlaOverrideRow>) {
        *self.overrides.write() = rows;
    }

    /// Pull samples completed since the last tick, refit each touched
    /// key from its `ring_buffer` most-recent rows, swap into the cache.
    /// Returns `(n_refit, hw_table_changed)`: `hw_table_changed` is true
    /// iff the [`hw::HwTable`] reload produced a content-hash different
    /// from the previous snapshot — caller bumps
    /// [`solve::SolveCache::bump_inputs_gen`] only then (an
    /// unconditional bump re-rolls ε_h every 60s, so explore Jobs are
    /// reaped before Karpenter provisions).
    ///
    /// The incremental query and the per-key reads are separate round-
    /// trips: incremental tells us *which* keys moved (cheap, indexed
    /// range scan); per-key gives the full ring including rows older
    /// than `last_tick` that the fit still needs. One refit per key per
    /// tick regardless of how many new rows landed.
    ///
    /// `tiers` (sorted tightest-first, as from
    /// [`config::SlaConfig::solve_tiers`]) feeds the Schmitt-trigger tier
    /// reassignment; empty → tier reassignment is a no-op. `on_evict`
    /// fires for each LRU-evicted key so the caller can drop the paired
    /// [`solve::SolveCache`] entry.
    pub async fn refresh(
        &self,
        db: &SchedulerDb,
        tiers: &[solve::Tier],
        on_evict: impl Fn(&types::ModelKey),
    ) -> anyhow::Result<(usize, bool)> {
        // Override snapshot first: cheap (operator-written, tens of
        // rows) and independent of the sample refit, so a PG blip on
        // the heavier incremental query below still leaves the override
        // cache fresh for this tick.
        match db.read_sla_overrides(&self.cluster, None).await {
            Ok(rows) => *self.overrides.write() = rows,
            Err(e) => tracing::warn!(error = %e, "sla override refresh failed; keeping previous"),
        }
        // r[impl sched.sla.hw-ref-seconds]
        // Hw factor table: same cheap-and-independent treatment as
        // overrides (a few dozen rows from a view). Failure → keep
        // previous; an empty table is factor=1.0 everywhere.
        let hw_before = self.hw.read().content_hash();
        match hw::HwTable::load(db).await {
            Ok(t) => self.apply_hw(t),
            Err(e) => tracing::warn!(error = %e, "sla hw-table refresh failed; keeping previous"),
        }
        let hw_snapshot = self.hw.read().clone();
        let hw_changed = hw_snapshot.content_hash() != hw_before;
        let priors_snapshot = self.priors.read().clone();

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

        // Batch the per-key ring reads into a single round-trip. On a
        // fresh process the first refresh has since=0.0 → every key in
        // the table is touched; the previous per-key sequential awaits
        // (read+trim ×N) blocked the actor for O(N×RTT) — ~30s at 10k
        // keys × 1.5ms cross-AZ RDS — during which no SubmitBuild /
        // CompletionReport / dispatch_ready was processed. With the
        // batch, the loop body below has no DB awaits — but it IS
        // CPU-bound (ingest::refit → t_min_ci(500 reps) ≈ 1ms/key when
        // bootstrapping a cold cache), so it yields every 64 keys to
        // bound per-slice actor stall to ~64ms.
        let mut pnames = Vec::with_capacity(touched.len());
        let mut systems = Vec::with_capacity(touched.len());
        let mut tenants = Vec::with_capacity(touched.len());
        for k in &touched {
            pnames.push(k.pname.clone());
            systems.push(k.system.clone());
            tenants.push(k.tenant.clone());
        }
        let mut rings: HashMap<types::ModelKey, Vec<crate::db::BuildSampleRow>> = db
            .read_build_samples_for_keys(&pnames, &systems, &tenants, self.ring_buffer)
            .await?
            .into_iter()
            .fold(HashMap::new(), |mut m, r| {
                m.entry(types::ModelKey {
                    pname: r.pname.clone(),
                    system: r.system.clone(),
                    tenant: r.tenant.clone(),
                })
                .or_default()
                .push(r);
                m
            });

        let mut outlier_ids: HashSet<i64> = HashSet::new();
        for (i, key) in touched.iter().enumerate() {
            if i > 0 && i.is_multiple_of(64) {
                tokio::task::yield_now().await;
            }
            // peek (not get): don't bump recency on the refit pass —
            // `cached()` from dispatch is the authoritative "this key
            // is in use" signal.
            let prev = self
                .cache
                .read()
                .get(&key.tenant)
                .and_then(|lru| lru.peek(&(key.pname.clone(), key.system.clone())))
                .cloned();
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
                    // r[impl sched.sla.hw-ref-seconds]
                    // prev.fit and prev.log_residuals are in
                    // reference-seconds; normalize the new sample's
                    // wall-clock before comparing or an on-curve sample
                    // from a fast hw_class is falsely flagged by
                    // |ln(1/factor)|.
                    let ref_t =
                        hw_snapshot.normalize(r.duration_secs, r.hw_class.as_deref(), prev.alpha);
                    if let Some(c) = r.cpu_limit_cores
                        && ingest::is_outlier(ref_t, r.duration_secs, c, prev, DT_POLL_SECS)
                    {
                        outlier_ids.insert(r.id);
                        ::metrics::counter!(
                            "rio_scheduler_sla_outlier_rejected_total",
                            "tenant" => key.tenant.clone()
                        )
                        .increment(1);
                    }
                }
            }
            // Batch read happened BEFORE outlier detection, so drop
            // the just-flagged ids in-memory (the PG `WHERE NOT
            // outlier_excluded` filter hasn't seen the UPDATE yet).
            let mut rows = rings.remove(key).unwrap_or_default();
            if !outlier_ids.is_empty() {
                rows.retain(|r| !outlier_ids.contains(&r.id));
            }
            let fit = ingest::refit(
                key,
                &rows,
                prev.as_ref(),
                tiers,
                &hw_snapshot,
                Some(&priors_snapshot),
            );
            self.insert(key, fit, |k| on_evict(k));
        }

        // Persist outlier flags BEFORE advancing `hwm`. NOT best-effort:
        // if the UPDATE fails and `hwm` advances anyway, the unmarked
        // row is past `hwm` on tick N+1 so it's never in `new_rows`
        // again — the in-memory `retain` above only filters ids
        // detected from `new_rows`, so the outlier contaminates
        // `read_build_samples_for_keys` for up to `ring_buffer=32`
        // refits. Propagating the error keeps `last_tick` unchanged so
        // the row reappears next tick and is re-detected.
        // `metrics::outlier_rejected` will double-count once on the
        // retry — acceptable, it's a "this happened" counter not a
        // billing meter.
        let outlier_ids: Vec<i64> = outlier_ids.into_iter().collect();
        db.mark_outliers_excluded(&outlier_ids).await?;
        // Trim stays best-effort — a missed trim only affects ring-
        // buffer size and is re-trimmed next tick with the same WHERE.
        if let Err(e) = db
            .trim_build_samples_batch(&pnames, &systems, &tenants, self.ring_buffer)
            .await
        {
            tracing::warn!(error = %e, "build_samples trim failed");
        }

        *self.last_tick.write() = hwm;

        // r[impl sched.sla.prior-partial-pool]
        // Fleet-median recompute AFTER the refit loop so this tick's new
        // fits feed the NEXT tick's prior (one-tick lag is fine — the
        // fleet aggregate moves slowly). Only Coupled-mem fits
        // contribute (FitParams.{a,b} are meaningless for Independent).
        // §A17: FOD fits are excluded — download-time outliers, not
        // build-time, and a tenant's fetchurl corpus would otherwise
        // drag the cross-tenant median.
        let coupled: Vec<(types::ModelKey, prior::FitParams)> = self
            .cache
            .read()
            .values()
            .flat_map(|lru| lru.iter().map(|(_, f)| f))
            .filter(|f| !f.is_fod)
            .filter_map(|f| {
                let (s, p, q) = f.fit.spq();
                if !s.is_finite() {
                    return None;
                }
                let types::MemFit::Coupled { a, b, .. } = f.mem else {
                    return None;
                };
                Some((
                    f.key.clone(),
                    prior::FitParams {
                        s,
                        p,
                        q,
                        a,
                        b,
                        alpha: f.alpha,
                    },
                ))
            })
            .collect();
        self.priors.write().fleet =
            prior::fleet_median(&coupled, FLEET_MEDIAN_MIN_KEYS, FLEET_MEDIAN_MIN_TENANTS);

        Ok((touched.len(), hw_changed))
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
            tiers: vec![solve::Tier {
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
                deadline_secs: 3600,
            },
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
            ..config::SlaConfig::test_default()
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
            n_distinct_c: 5,
            sum_w: n_eff,
            span: 8.0,
            explore: ExploreState {
                distinct_c: 3,
                min_c: RawCores(2.0),
                max_c: RawCores(16.0),
                saturated: true,
                last_wall: WallSeconds(100.0),
            },
            t_min_ci: None,
            ci_computed_at: None,
            tier: None,
            hw_bias: HashMap::new(),
            alpha: crate::sla::alpha::UNIFORM,
            prior_source: None,
            is_fod: false,
        }
    }

    /// §A15(b): per-tenant LRU evicts least-recently-used at cap.
    /// `cached()` bumps recency; `seed()`/`insert()` past cap evicts.
    #[test]
    fn lru_evicts_oldest_at_cap_per_tenant() {
        let est = SlaEstimator::for_test(&config::SlaConfig {
            max_keys_per_tenant: 3,
            ..cfg()
        });
        for i in 0..3 {
            est.seed(fitted(&format!("p{i}"), 5.0));
        }
        // All present.
        for i in 0..3 {
            assert!(est.cached(&fitted(&format!("p{i}"), 0.0).key).is_some());
        }
        // 4th insert evicts. The cached() loop above bumped p0..p2 in
        // order, so p0 is least-recent.
        est.seed(fitted("p3", 5.0));
        assert!(est.cached(&fitted("p0", 0.0).key).is_none(), "p0 evicted");
        assert!(est.cached(&fitted("p1", 0.0).key).is_some());
        assert!(est.cached(&fitted("p3", 0.0).key).is_some());
        // Different tenant has its OWN cap — t0's eviction didn't touch
        // t1, and t1 at cap=3 holds 3 independently.
        for i in 0..3 {
            let mut f = fitted(&format!("q{i}"), 5.0);
            f.key.tenant = "t1".into();
            est.seed(f);
        }
        let mut f = fitted("q0", 0.0);
        f.key.tenant = "t1".into();
        assert!(est.cached(&f.key).is_some(), "t1's q0 unaffected by t0");
        // t0's surviving entries also untouched by t1's inserts.
        assert!(est.cached(&fitted("p1", 0.0).key).is_some());
    }

    /// §A17: `is_fod=true` fits are excluded from the fleet-median input
    /// set (filter happens at the call site in `refresh()`, not in
    /// `fleet_median` itself — the filter chain in refresh() is what
    /// production runs). This test exercises the filter chain directly.
    #[test]
    fn fleet_median_input_excludes_fods() {
        let est = SlaEstimator::for_test(&cfg());
        // 3 normal fits across 3 tenants (so fleet_median's min-tenants
        // gate passes), 1 FOD with a huge outlier P.
        for (t, p) in [("t0", 100.0), ("t1", 200.0), ("t2", 300.0)] {
            let mut f = fitted("hello", 10.0);
            f.key.tenant = t.into();
            f.fit = DurationFit::Amdahl {
                s: RefSeconds(10.0),
                p: RefSeconds(p),
            };
            est.seed(f);
        }
        let mut fod = fitted("source.tar.gz", 10.0);
        fod.is_fod = true;
        fod.key.tenant = "t3".into();
        fod.fit = DurationFit::Amdahl {
            s: RefSeconds(1e6),
            p: RefSeconds(1e6),
        };
        est.seed(fod);
        // Replicate refresh()'s fleet-median input collection.
        let coupled: Vec<(ModelKey, prior::FitParams)> = est
            .cache
            .read()
            .values()
            .flat_map(|lru| lru.iter().map(|(_, f)| f))
            .filter(|f| !f.is_fod)
            .filter_map(|f| {
                let (s, p, q) = f.fit.spq();
                let MemFit::Coupled { a, b, .. } = f.mem else {
                    return None;
                };
                Some((
                    f.key.clone(),
                    prior::FitParams {
                        s,
                        p,
                        q,
                        a,
                        b,
                        alpha: f.alpha,
                    },
                ))
            })
            .collect();
        assert_eq!(coupled.len(), 3, "FOD excluded from input set");
        let m = prior::fleet_median(&coupled, 3, 3).expect("3 keys, 3 tenants");
        assert!(
            (m.p - 200.0).abs() < 1e-9,
            "median{{100,200,300}}, FOD ignored"
        );
        assert!(m.s < 1000.0, "FOD outlier did NOT contaminate");
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

        // Fresh estimator, import the corpus. hw must be populated or
        // import_seed defers (factor=NaN).
        let dst = SlaEstimator::for_test(&cfg());
        let mut m = HashMap::new();
        m.insert("ref".into(), 1.0);
        dst.seed_hw(hw::HwTable::from_map(m));
        let parsed: prior::SeedCorpus = serde_json::from_str(&json).unwrap();
        let (n, _) = dst.import_seed(prior::ValidatedSeedCorpus::assume_valid(parsed));
        assert_eq!(n, 5);

        // prior_for on a key from the corpus → Seed; on an unknown →
        // Operator (no fleet, seed miss).
        let priors = dst.prior_sources();
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
    fn export_corpus_dedup_across_tenants() {
        let src = SlaEstimator::for_test(&cfg());
        // Same (pname, system), two tenants, distinct n_eff.
        let mut t0 = fitted("gcc", 5.0);
        t0.key.tenant = "t0".into();
        src.seed(t0);
        let mut t1 = fitted("gcc", 10.0);
        t1.key.tenant = "t1".into();
        src.seed(t1);
        // Distinct pname, one tenant.
        src.seed(fitted("hello", 5.0));

        let entries = src.export_corpus(None, 3).entries;
        assert_eq!(entries.len(), 2, "gcc dedup'd to 1; hello = 1");
        let gcc = entries.iter().find(|e| e.pname == "gcc").unwrap();
        assert_eq!(gcc.n, 10, "highest-n representative survives");
        // Determinism: two exports moments apart yield identical output.
        let again = src.export_corpus(None, 3).entries;
        assert_eq!(entries.len(), again.len());
        for (a, b) in entries.iter().zip(again.iter()) {
            assert_eq!(a.pname, b.pname);
            assert_eq!(a.n, b.n);
        }
    }

    #[test]
    fn ref_estimate_probe_is_none_not_infinity() {
        let est = SlaEstimator::for_test(&cfg());
        let mut p = fitted("probing", 1.0);
        p.fit = DurationFit::Probe;
        let key = p.key.clone();
        est.seed(p);
        // Probe.t_min() = ∞ is filtered → None, so critical_path falls
        // back to DEFAULT_DURATION_SECS instead of poisoning priority.
        assert_eq!(est.ref_estimate(&key), None);
        // Control: a fitted Amdahl curve returns a finite estimate.
        let f = fitted("fitted", 10.0);
        let fkey = f.key.clone();
        est.seed(f);
        let w = est.ref_estimate(&fkey).expect("fitted → Some");
        assert!(w.is_finite() && w > 0.0);
    }

    #[test]
    fn seed_corpus_rescaled_after_first_refresh() {
        // ref_hw_class="fast" with one entry s=100. `new()` must NOT
        // convert (hw table is empty → factor=1.0 → rescale silently
        // bypassed); `apply_pending_seed` against a populated table
        // must rescale by `factor[fast]/factor[reference]`.
        let est = SlaEstimator::for_test(&cfg());
        *est.pending_seed.lock() = Some(prior::ValidatedSeedCorpus::assume_valid(
            prior::SeedCorpus {
                ref_hw_class: "fast".into(),
                entries: vec![prior::SeedEntry {
                    pname: "hello".into(),
                    system: "x86_64-linux".into(),
                    s: 100.0,
                    p: 200.0,
                    q: 0.0,
                    p_bar: 0.0,
                    a: 22.0,
                    b: 0.5,
                    n: 5,
                    ..Default::default()
                }],
                ..Default::default()
            },
        ));
        assert!(
            est.prior_sources().seed.is_empty(),
            "new() defers conversion"
        );

        let mut m = HashMap::new();
        m.insert("fast".into(), 2.0);
        m.insert("slow".into(), 1.0);
        let hw = hw::HwTable::from_map(m);
        est.apply_pending_seed(&hw);

        let seed = est.prior_sources().seed;
        let entry = seed
            .get(&("hello".into(), "x86_64-linux".into()))
            .expect("seeded");
        // scale = factor[fast]/factor[reference] = 2.0/1.0 (slow is
        // reference, factor closest to 1.0).
        assert_eq!(entry.s, 200.0, "s rescaled by 2.0");
        assert_eq!(entry.p, 400.0, "p rescaled by 2.0");
        assert_eq!(entry.a, 22.0, "mem params not rescaled");

        // One-shot: second call is a no-op.
        est.apply_pending_seed(&hw);
        assert_eq!(est.prior_sources().seed.len(), 1);
    }

    #[test]
    fn pending_seed_survives_empty_hw_table() {
        // HwTable::load returns Ok({}) when hw_perf_factors has zero
        // rows (HAVING count(DISTINCT pod_id) >= 3 floor — always on a
        // fresh cluster). apply_pending_seed must NOT consume the seed
        // against an empty table (scale would be 1.0/1.0).
        let est = SlaEstimator::for_test(&cfg());
        *est.pending_seed.lock() = Some(prior::ValidatedSeedCorpus::assume_valid(
            prior::SeedCorpus {
                ref_hw_class: "fast".into(),
                entries: vec![prior::SeedEntry {
                    pname: "hello".into(),
                    system: "x86_64-linux".into(),
                    s: 100.0,
                    p: 200.0,
                    q: 0.0,
                    p_bar: 0.0,
                    a: 22.0,
                    b: 0.5,
                    n: 5,
                    ..Default::default()
                }],
                ..Default::default()
            },
        ));
        est.apply_pending_seed(&hw::HwTable::default());
        assert!(
            est.pending_seed.lock().is_some(),
            "seed not consumed against empty hw"
        );
        assert!(est.prior_sources().seed.is_empty());

        // Populated table → consumed and rescaled.
        let mut m = HashMap::new();
        m.insert("fast".into(), 2.0);
        m.insert("slow".into(), 1.0);
        est.apply_pending_seed(&hw::HwTable::from_map(m));
        let seed = est.prior_sources().seed;
        assert_eq!(seed.len(), 1);
        assert_eq!(
            seed[&("hello".into(), "x86_64-linux".into())].s,
            200.0,
            "rescaled by 2.0, not 1.0"
        );
    }

    #[test]
    fn import_seed_defers_on_empty_hw() {
        let est = SlaEstimator::for_test(&cfg());
        let corpus = prior::SeedCorpus {
            ref_hw_class: "fast".into(),
            entries: vec![prior::SeedEntry {
                pname: "x".into(),
                system: "x86_64-linux".into(),
                s: 100.0,
                p: 200.0,
                q: 0.0,
                p_bar: 0.0,
                a: 22.0,
                b: 0.5,
                n: 5,
                ..Default::default()
            }],
            ..Default::default()
        };
        // hw empty → stashed, factor=NaN sentinel.
        let (n, scale) = est.import_seed(prior::ValidatedSeedCorpus::assume_valid(corpus));
        assert_eq!(n, 1);
        assert!(scale.is_nan(), "deferred sentinel");
        assert!(est.prior_sources().seed.is_empty());
        assert!(est.pending_seed.lock().is_some(), "stashed for next tick");

        // Next refresh with populated hw picks it up.
        let mut m = HashMap::new();
        m.insert("fast".into(), 2.0);
        m.insert("slow".into(), 1.0);
        est.apply_pending_seed(&hw::HwTable::from_map(m));
        assert_eq!(
            est.prior_sources().seed[&("x".into(), "x86_64-linux".into())].s,
            200.0
        );
    }

    // r[verify sched.sla.hw-ref-seconds]
    #[test]
    fn operator_basis_tracks_hw_reference() {
        // cfg().tiers[0].p90 = 1200 (wall-seconds). After a populated
        // hw load with factor[reference]=1.3, priors.default_tier_target
        // must be 1200 × 1.3 ref-seconds so operator_to_spq /
        // clamp_to_operator compare ref-against-ref. Before the fix the
        // wall value was used directly.
        let est = SlaEstimator::for_test(&cfg());
        assert_eq!(
            est.prior_sources().default_tier_target,
            1200.0,
            "factor=1.0 until first populated load"
        );
        let mut m = HashMap::new();
        m.insert("ref".into(), 1.3);
        est.apply_hw(hw::HwTable::from_map(m));
        let got = est.prior_sources().default_tier_target;
        assert!(
            (got - 1200.0 * 1.3).abs() < 1e-9,
            "wall→ref converted: got {got}, want {}",
            1200.0 * 1.3
        );
        // Empty load → unchanged (no conversion against factor=1.0).
        let est2 = SlaEstimator::for_test(&cfg());
        est2.apply_hw(hw::HwTable::default());
        assert_eq!(est2.prior_sources().default_tier_target, 1200.0);
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
}
