//! ADR-023 phase-13 hw-band + capacity-type cost model.
//!
//! Two halves, both feeding [`super::solve::solve_full`]:
//!
//! - [`CostTable`]: per-`(Band, Cap)` `$/vCPU·hr` snapshot. Populated by
//!   [`spot_price_poller`] (lease-gated, 10min tick, 3h-halflife EMA over
//!   `DescribeSpotPriceHistory`) and persisted to `sla_ema_state` so a
//!   restart doesn't re-warm. `expected_cost` turns a candidate
//!   `(band, cap, c*, T(c*))` into a comparable scalar for the softmax.
//! - λ\[h\]: per-hw-band Poisson interrupt rate. Gamma-Poisson partial
//!   pooling over `interrupt_samples` (controller-appended): the seed
//!   acts as a prior with weight `n_λ = 1day · max(1, node_count_ema)`
//!   so a single interrupt doesn't spike λ, and exiled-spot decay
//!   collapses to the seed rather than freezing at the spike.
//!
//! [`IceBackoff`] is the in-process insufficient-capacity ladder: a
//! `(band, cap)` that left a pod Pending past the Pending-watch window
//! is marked infeasible fleet-wide for 60s so the next solve excludes
//! it. Superseded by the admissible-set + lead-time design; retained
//! until that lands.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::db::SchedulerDb;
use crate::lease::LeaderState;

use super::hw::HwTable;
use super::types::{FittedParams, RawCores};

/// Hardware-generation band. Maps to the `rio.build/hw-band` Node label
/// (12-NodePool topology). The label is the source of truth — bands are
/// non-disjoint by `instance-generation` (helm `mid` admits gen 6+7 so
/// `mid-nvme-x86` can fall back to c6id), so the generation digit alone
/// cannot recover the band.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Band {
    Hi,
    Mid,
    Lo,
}

impl Band {
    pub const ALL: [Band; 3] = [Band::Hi, Band::Mid, Band::Lo];

    /// `rio.build/hw-band` label value.
    pub fn label(self) -> &'static str {
        match self {
            Band::Hi => "hi",
            Band::Mid => "mid",
            Band::Lo => "lo",
        }
    }
}

/// Capacity type. Maps to `karpenter.sh/capacity-type`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Cap {
    Spot,
    OnDemand,
}

impl Cap {
    pub const ALL: [Cap; 2] = [Cap::Spot, Cap::OnDemand];

    /// `karpenter.sh/capacity-type` label value.
    pub fn label(self) -> &'static str {
        match self {
            Cap::Spot => "spot",
            Cap::OnDemand => "on-demand",
        }
    }
}

/// Where `$/vCPU·hr` numbers come from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HwCostSource {
    /// Live `DescribeSpotPriceHistory` poll (IRSA). Spot prices EMA'd;
    /// on-demand falls back to [`ON_DEMAND_SEED`] (no public on-demand
    /// price API without `pricing:GetProducts`).
    Spot,
    /// Static seeds only — no AWS calls. The cost ranking degenerates
    /// to "spot < on-demand, hi > mid > lo" with fixed ratios.
    Static,
}

/// Decayed EMA of a ratio: `value = numerator / denominator` where both
/// halves are independently EMA-decayed. Used for λ\[h\] (interrupts ÷
/// node-seconds) so a burst of node churn doesn't spike λ — the
/// denominator absorbs it.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct RatioEma {
    pub numerator: f64,
    pub denominator: f64,
    /// Unix-epoch seconds of last update. Drives the decay factor.
    pub updated_at: f64,
}

impl RatioEma {
    /// Fold `(num, den)` into the EMA at wall-clock `now` (epoch secs)
    /// with `halflife_secs`. Both running sums decay by
    /// `0.5^(Δt/halflife)` then the new sample is added — so the ratio
    /// is `Σ decayed-num / Σ decayed-den`, not an EMA of instantaneous
    /// ratios (which would over-weight low-denominator ticks).
    pub fn update(&mut self, num: f64, den: f64, now: f64, halflife_secs: f64) {
        let dt = (now - self.updated_at).max(0.0);
        let decay = if self.updated_at == 0.0 {
            0.0 // first sample: no prior to decay
        } else {
            0.5f64.powf(dt / halflife_secs)
        };
        self.numerator = self.numerator * decay + num;
        self.denominator = self.denominator * decay + den;
        self.updated_at = now;
    }

    /// `numerator / denominator`, or `seed` if `denominator ≈ 0` (no
    /// exposure yet / fully decayed).
    pub fn value_or(&self, seed: f64) -> f64 {
        if self.denominator > f64::EPSILON {
            self.numerator / self.denominator
        } else {
            seed
        }
    }
}

/// Seed λ (interrupts/sec) — the Gamma-Poisson prior mean. ~1/3h:
/// AWS's published spot-interruption frequency floor for the deepest
/// pools is "<5%/hr"; 1/3h is a conservative middle. With pooling the
/// seed contributes ~50% at one wall-clock day of exposure regardless
/// of fleet size.
pub const LAMBDA_SEED: f64 = 1.0 / (3.0 * 3600.0);

/// Gamma-Poisson prior pseudo-exposure unit: one day of node-seconds.
/// `n_λ = N_LAMBDA_DAY_SECS · max(1, node_count_ema)` — see
/// [`lambda_hat`].
const N_LAMBDA_DAY_SECS: f64 = 86400.0;

/// Spot-price poller tick. 10min — well under the AWS API rate limits;
/// the 3h price halflife smooths sub-tick granularity.
pub const POLL_INTERVAL_SECS: u64 = 600;

/// `_hw_cost_stale_seconds` threshold past which [`CostTable::price`]
/// clamps to seed and `_hw_cost_fallback_total{reason="stale"}` fires.
/// 6× the poll interval = 1h: enough to absorb a few transient AWS
/// failures, short enough that a wedged poller doesn't drive the solve
/// off month-old prices.
pub const STALE_CLAMP_AFTER_SECS: f64 = 6.0 * POLL_INTERVAL_SECS as f64;

// r[impl sched.sla.hw-class.lambda-gamma-poisson]
/// Gamma-Poisson partial-pooling λ estimate. The seed acts as a prior
/// with pseudo-exposure `n_λ = 1day · max(1, node_count_ema)`:
///
/// ```text
/// λ̂ = (EMA(interrupts) + n_λ·seed) / (EMA(exposure) + n_λ)
/// ```
///
/// The `max(1, ·)` floor keeps the prior from vanishing when spot is
/// exiled and `node_count → 0` — without it λ̂ freezes at the spike that
/// caused the exile. Replaces the linear-decay-to-seed-after-48h design,
/// which under persistent capacity stress had a ~48h limit cycle (spike
/// → exile → exposure→0 → decay → re-admit → spike).
pub fn lambda_hat(
    ema_interrupts: f64,
    ema_exposure_secs: f64,
    ema_node_count: f64,
    lambda_seed: f64,
) -> f64 {
    let n_lambda = N_LAMBDA_DAY_SECS * ema_node_count.max(1.0);
    (ema_interrupts + n_lambda * lambda_seed) / (ema_exposure_secs + n_lambda)
}

/// Seed `$/vCPU·hr` per band, on-demand. Roughly c6a/c7a/c8g list
/// price ÷ vCPU. Ratios are what matter (softmax normalizes); absolute
/// values only surface in `SlaExplain`.
pub const ON_DEMAND_SEED: [(Band, f64); 3] =
    [(Band::Hi, 0.048), (Band::Mid, 0.043), (Band::Lo, 0.038)];

/// Spot discount applied to [`ON_DEMAND_SEED`] when the poller has no
/// live data (source=static or first tick).
const SPOT_SEED_DISCOUNT: f64 = 0.35;

/// Spot-price EMA halflife. 3h: long enough to smooth the ~5min AWS
/// price-update granularity, short enough to track intra-day swings.
const SPOT_HALFLIFE_SECS: f64 = 3.0 * 3600.0;

/// λ\[h\] EMA halflife. 24h: spot interruption rates move on a daily
/// cadence (capacity rebalancing); a 3h halflife would chase noise.
/// Same halflife for the `node_count_ema` that scales the prior.
const LAMBDA_HALFLIFE_SECS: f64 = 24.0 * 3600.0;

/// ICE-backoff TTL. A `(band, cap)` that left a pod Pending past the
/// Pending-watch window is fleet-wide infeasible for this long. Short
/// — capacity recovers in minutes; the ladder re-probes.
const ICE_TTL: Duration = Duration::from_secs(60);

/// EMA-smoothed scalar with its own last-update timestamp. Used for
/// `$/vCPU·hr` and per-band `node_count`. Per-key timestamp (mirroring
/// [`RatioEma`]) so a key absent from a partial observation keeps its
/// OWN decay reference — a single global timestamp under-decays absent
/// keys when the global stamp moves forward. Serde-derives so the whole
/// struct round-trips a `jsonb` column without per-field plumbing.
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct PriceEma {
    pub value: f64,
    /// Unix-epoch seconds of last update. `0.0` ⇒ seed (first fold gets
    /// `decay = 0.0`).
    pub updated_at: f64,
}

impl PriceEma {
    /// Fold one sample at wall-clock `now` with `halflife_secs`. Same
    /// `0.5^(Δt/H)` decay as [`RatioEma::update`]; `updated_at = 0.0`
    /// is treated as "no prior" so the first fold takes the sample
    /// verbatim.
    pub fn update(&mut self, sample: f64, now: f64, halflife_secs: f64) {
        let dt = (now - self.updated_at).max(0.0);
        let decay = if self.updated_at == 0.0 {
            0.0
        } else {
            0.5f64.powf(dt / halflife_secs)
        };
        self.value = self.value * decay + sample * (1.0 - decay);
        self.updated_at = now;
    }
}

/// Static `$/vCPU·hr` seed for `(band, cap)`. Backstop for
/// [`CostTable::price`] (missing key, or stale-clamped).
fn seed_price(band: Band, cap: Cap) -> f64 {
    let od = ON_DEMAND_SEED
        .iter()
        .find(|(b, _)| *b == band)
        .map(|(_, p)| *p)
        .unwrap_or(0.043);
    match cap {
        Cap::OnDemand => od,
        Cap::Spot => od * SPOT_SEED_DISCOUNT,
    }
}

/// Per-`(band, cap)` `$/vCPU·hr` + per-band λ. Cheap to clone (three
/// small maps); the solve takes a snapshot by value.
#[derive(Debug, Clone)]
pub struct CostTable {
    /// EMA-smoothed `$/vCPU·hr`. Missing key → seed.
    price: HashMap<(Band, Cap), PriceEma>,
    /// Per-band interrupt-rate EMA. `numerator` = Σ interrupts,
    /// `denominator` = Σ exposure-secs (24h halflife). Read via
    /// [`lambda_hat`], not as a bare ratio.
    lambda: HashMap<Band, RatioEma>,
    /// Per-band 24h-EMA of live spot-node count. The `n_λ` scaler in
    /// [`lambda_hat`]: keeps the prior's relative weight ~constant at
    /// "one day of fleet exposure" regardless of fleet size. Derived
    /// from `interrupt_samples` exposure rows in
    /// [`CostTable::refresh_lambda`] as `Σ exposure_secs / Δt`.
    node_count: HashMap<Band, PriceEma>,
    /// `price_updated_at() > 6 × pollInterval` ago. Set by
    /// [`CostTable::apply_stale_clamp`] each tick; while true,
    /// [`CostTable::price`] returns the static seed so a wedged poller
    /// can't drive the solve off month-old data.
    stale_clamp: bool,
    /// `sla_ema_state.cluster` / `interrupt_samples.cluster` scope
    /// (ADR-023 §2.13). Set by [`CostTable::load`]; empty for the
    /// single-cluster default. Carried on the struct so
    /// [`CostTable::persist`] / [`CostTable::refresh_lambda`] (called
    /// from the poller's snapshot-mutate-swap) don't need it threaded
    /// separately.
    cluster: String,
}

impl Default for CostTable {
    fn default() -> Self {
        let mut price = HashMap::new();
        for &(b, od) in &ON_DEMAND_SEED {
            price.insert(
                (b, Cap::OnDemand),
                PriceEma {
                    value: od,
                    updated_at: 0.0,
                },
            );
            price.insert(
                (b, Cap::Spot),
                PriceEma {
                    value: od * SPOT_SEED_DISCOUNT,
                    updated_at: 0.0,
                },
            );
        }
        Self {
            price,
            lambda: HashMap::new(),
            node_count: HashMap::new(),
            stale_clamp: false,
            cluster: String::new(),
        }
    }
}

impl CostTable {
    /// `$/vCPU·hr` for `(band, cap)`. Seed-backed — never `None`.
    /// Clamps to seed while [`Self::apply_stale_clamp`] has the
    /// stale-clamp latched.
    pub fn price(&self, band: Band, cap: Cap) -> f64 {
        if self.stale_clamp {
            return seed_price(band, cap);
        }
        self.price
            .get(&(band, cap))
            .map(|p| p.value)
            .unwrap_or_else(|| seed_price(band, cap))
    }

    /// Per-band Poisson interrupt rate (events/sec) via [`lambda_hat`].
    /// `(EMA(interrupts) + n_λ·seed) / (EMA(exposure) + n_λ)` with
    /// `n_λ = 1day · max(1, node_count_ema)`. Returns [`LAMBDA_SEED`]
    /// for a band with no observations (default RatioEma + node_count=0
    /// reduces exactly).
    pub fn lambda_band(&self, band: Band) -> f64 {
        let ema = self.lambda.get(&band).copied().unwrap_or_default();
        let nc = self.node_count.get(&band).map(|p| p.value).unwrap_or(0.0);
        lambda_hat(ema.numerator, ema.denominator, nc, LAMBDA_SEED)
    }

    /// `E[cost]` for a candidate: `price · c* · E[wall] / 3600` where
    /// `E[wall] = (T(c*)/factor) / (1-p)` accounts for geometric
    /// retries under preemption probability `p = 1 - e^{-λT}`.
    /// Memory contributes via the same per-vCPU-hr proxy (memory
    /// scales with cores in the c/m/r families the NodePools admit).
    #[allow(clippy::too_many_arguments)]
    pub fn expected_cost(
        &self,
        band: Band,
        cap: Cap,
        c_star: RawCores,
        _mem: u64,
        fit: &FittedParams,
        hw_factor: f64,
        lambda: f64,
    ) -> f64 {
        let t = fit.fit.t_at(c_star).0 / hw_factor;
        let p = if lambda > 0.0 {
            (1.0 - (-lambda * t).exp()).min(0.499)
        } else {
            0.0
        };
        let e_wall = t / (1.0 - p);
        self.price(band, cap) * c_star.0 * e_wall / 3600.0
    }

    /// Seed-backed table scoped to `cluster`. Use instead of
    /// [`Default`] when the load fallback needs the cluster carried
    /// forward to `persist`.
    pub fn seeded(cluster: &str) -> Self {
        Self {
            cluster: cluster.to_owned(),
            ..Self::default()
        }
    }

    /// Recompute the stale-clamp latch from the price timestamps.
    /// Returns `true` while the clamp is engaged. Level-triggered: each
    /// call while stale increments `_hw_cost_fallback_total{reason=
    /// "stale"}` so the rate surfaces in alerting.
    pub fn apply_stale_clamp(&mut self, now: f64) -> bool {
        let stale = now - self.price_updated_at();
        self.stale_clamp = stale > STALE_CLAMP_AFTER_SECS;
        if self.stale_clamp {
            super::metrics::hw_cost_fallback("stale");
        }
        self.stale_clamp
    }

    /// Unix-epoch seconds of the most-recently-updated price key. Feeds
    /// `rio_scheduler_sla_hw_cost_stale_seconds`. Derived (not stored)
    /// so it can never drift from the per-key timestamps.
    pub fn price_updated_at(&self) -> f64 {
        self.price
            .values()
            .map(|p| p.updated_at)
            .fold(0.0, f64::max)
    }

    /// `sla_ema_state.cluster` scope. Exposed so the poller's
    /// leader-edge reload can re-`load()` from the in-mem snapshot's
    /// own scope.
    pub fn cluster(&self) -> &str {
        &self.cluster
    }

    /// Load persisted EMAs from `sla_ema_state`. Called once at
    /// startup so a scheduler restart doesn't re-warm. `cluster`
    /// scopes the rows (ADR-023 §2.13 global-DB safety).
    pub async fn load(db: &SchedulerDb, cluster: &str) -> anyhow::Result<Self> {
        type Row = (String, f64, Option<f64>, Option<f64>, f64);
        let mut t = Self::seeded(cluster);
        let rows: Vec<Row> = sqlx::query_as(
            "SELECT key, value, numerator, denominator, \
             EXTRACT(EPOCH FROM updated_at)::float8 FROM sla_ema_state WHERE cluster = $1",
        )
        .bind(cluster)
        .fetch_all(db.pool())
        .await?;
        for (key, value, num, den, at) in rows {
            if let Some(rest) = key.strip_prefix("price:")
                && let Some((b, c)) = parse_band_cap(rest)
            {
                t.price.insert(
                    (b, c),
                    PriceEma {
                        value,
                        updated_at: at,
                    },
                );
            } else if let Some(rest) = key.strip_prefix("lambda:")
                && let Some(b) = parse_band(rest)
            {
                t.lambda.insert(
                    b,
                    RatioEma {
                        numerator: num.unwrap_or(0.0),
                        denominator: den.unwrap_or(0.0),
                        updated_at: at,
                    },
                );
            } else if let Some(rest) = key.strip_prefix("node_count:")
                && let Some(b) = parse_band(rest)
            {
                t.node_count.insert(
                    b,
                    PriceEma {
                        value,
                        updated_at: at,
                    },
                );
            }
        }
        Ok(t)
    }

    /// Persist all EMAs to `sla_ema_state` (upsert). One row per
    /// `(cluster, key)`; small (≤9 rows), so no batching.
    pub async fn persist(&self, db: &SchedulerDb) -> anyhow::Result<()> {
        for (&(b, c), p) in &self.price {
            // `to_timestamp($4)` (data-time), NOT `now()`: a tick where
            // `poll_spot_once` failed must not advance the persisted
            // timestamp, or on reload staleness is lost and the next
            // `fold_prices` decay `dt` is wrong.
            sqlx::query(
                "INSERT INTO sla_ema_state (cluster, key, value, updated_at) \
                 VALUES ($1, $2, $3, to_timestamp($4)) \
                 ON CONFLICT (cluster, key) DO UPDATE SET value = $3, updated_at = to_timestamp($4)",
            )
            .bind(&self.cluster)
            .bind(format!("price:{}:{}", b.label(), c.label()))
            .bind(p.value)
            .bind(p.updated_at)
            .execute(db.pool())
            .await?;
        }
        for (&b, ema) in &self.lambda {
            sqlx::query(
                "INSERT INTO sla_ema_state (cluster, key, value, numerator, denominator, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, to_timestamp($6)) \
                 ON CONFLICT (cluster, key) DO UPDATE SET \
                   value = $3, numerator = $4, denominator = $5, updated_at = to_timestamp($6)",
            )
            .bind(&self.cluster)
            .bind(format!("lambda:{}", b.label()))
            .bind(ema.value_or(LAMBDA_SEED))
            .bind(ema.numerator)
            .bind(ema.denominator)
            .bind(ema.updated_at)
            .execute(db.pool())
            .await?;
        }
        for (&b, nc) in &self.node_count {
            sqlx::query(
                "INSERT INTO sla_ema_state (cluster, key, value, updated_at) \
                 VALUES ($1, $2, $3, to_timestamp($4)) \
                 ON CONFLICT (cluster, key) DO UPDATE SET value = $3, updated_at = to_timestamp($4)",
            )
            .bind(&self.cluster)
            .bind(format!("node_count:{}", b.label()))
            .bind(nc.value)
            .bind(nc.updated_at)
            .execute(db.pool())
            .await?;
        }
        Ok(())
    }

    /// Recompute λ\[h\] from `interrupt_samples` rows newer than each
    /// band's `updated_at`. Called from the poller tick (lease-gated)
    /// — controller appends, scheduler aggregates.
    pub async fn refresh_lambda(&mut self, db: &SchedulerDb) -> anyhow::Result<()> {
        // Aggregate per-hw_class since the last per-band update; map
        // hw_class → band via the band segment. Unknown hw_classes
        // (band segment absent or `unknown` — non-builder/fetcher
        // nodes) are dropped — they contribute to neither numerator
        // nor denominator.
        let rows: Vec<(String, String, f64, f64)> = sqlx::query_as(
            "SELECT hw_class, kind, COALESCE(SUM(value), 0), \
                    EXTRACT(EPOCH FROM MAX(at))::float8 \
             FROM interrupt_samples WHERE cluster = $1 AND at > to_timestamp($2) \
             GROUP BY hw_class, kind",
        )
        .bind(&self.cluster)
        .bind(
            self.lambda
                .values()
                .map(|e| e.updated_at)
                .fold(0.0, f64::max),
        )
        .fetch_all(db.pool())
        .await?;
        // HWM from the rows' MAX(at), NOT wall-clock now(): a row whose
        // PG-stamped `at` is behind the scheduler clock (skew, or commit
        // lagged the SELECT) would otherwise be permanently skipped on
        // the next tick. Same pattern as `SlaEstimator::refresh`.
        let mut hwm = self
            .lambda
            .values()
            .map(|e| e.updated_at)
            .fold(0.0, f64::max);
        let prev_hwm = hwm;
        let mut per_band: HashMap<Band, (f64, f64)> = HashMap::new();
        for (hw_class, kind, sum, max_at) in rows {
            hwm = hwm.max(max_at);
            let Some(band) = band_of_hw_class(&hw_class) else {
                continue;
            };
            let e = per_band.entry(band).or_default();
            match kind.as_str() {
                "interrupt" => e.0 += sum,
                "exposure" => e.1 += sum,
                _ => {}
            }
        }
        // Per-band node_count = Σ exposure_secs / Δt over the batch
        // window (each `kind='exposure'` row is "node-seconds accrued
        // since last flush", so the sum ÷ wall-window is mean live
        // nodes). Skip when `prev_hwm == 0` — first refresh has no
        // window baseline, and a `Δt` from epoch would zero the count.
        let dt = hwm - prev_hwm;
        for (band, (n, d)) in per_band {
            self.lambda
                .entry(band)
                .or_default()
                .update(n, d, hwm, LAMBDA_HALFLIFE_SECS);
            if prev_hwm > 0.0 && dt > 0.0 {
                self.node_count
                    .entry(band)
                    .or_default()
                    .update(d / dt, hwm, LAMBDA_HALFLIFE_SECS);
            }
        }
        Ok(())
    }

    /// Fold one round of spot-price observations into the price EMA.
    /// `obs` is `$/vCPU·hr` keyed by `(band, cap)` — already
    /// vCPU-normalized by the caller. Per-key `dt`: a key absent from
    /// `obs` keeps its OWN `updated_at`, so when it next appears its
    /// decay reflects the full elapsed interval, not just the gap since
    /// the last (partial) fold.
    pub fn fold_prices(&mut self, obs: &HashMap<(Band, Cap), f64>, now: f64) {
        for (&k, &v) in obs {
            self.price
                .entry(k)
                .or_default()
                .update(v, now, SPOT_HALFLIFE_SECS);
        }
    }

    /// Test constructor.
    #[cfg(test)]
    pub fn from_parts(price: HashMap<(Band, Cap), f64>, lambda: HashMap<Band, RatioEma>) -> Self {
        let now = now_epoch();
        Self {
            price: price
                .into_iter()
                .map(|(k, v)| {
                    (
                        k,
                        PriceEma {
                            value: v,
                            updated_at: now,
                        },
                    )
                })
                .collect(),
            lambda,
            node_count: HashMap::new(),
            stale_clamp: false,
            cluster: String::new(),
        }
    }

    /// Test setter: insert a price with an explicit `updated_at`.
    #[cfg(test)]
    pub fn set_price(&mut self, band: Band, cap: Cap, value: f64, updated_at: f64) {
        self.price
            .insert((band, cap), PriceEma { value, updated_at });
    }

    /// Test setter: per-band node-count EMA.
    #[cfg(test)]
    pub fn set_node_count(&mut self, band: Band, value: f64, updated_at: f64) {
        self.node_count.insert(band, PriceEma { value, updated_at });
    }
}

/// Map `"{mfr}-{gen}-{storage}-{band}"` → `Band` via the trailing band
/// segment (the node's `rio.build/hw-band` label, captured by
/// `node_informer`). `None` for hw_classes with no/unknown band segment
/// — fetcher/metal/non-Karpenter nodes carry no `rio.build/hw-band`
/// label, and the 12-NodePool builder topology always sets one.
///
/// Keys on the band label, NOT the generation digit: helm
/// `builderBands.mid` admits gen 6+7, so a c6id node provisioned via the
/// mid NodePool has gen=`6` but band=`mid`.
pub fn band_of_hw_class(hw_class: &str) -> Option<Band> {
    parse_band(hw_class.split('-').nth(3)?)
}

fn parse_band(s: &str) -> Option<Band> {
    Band::ALL.into_iter().find(|b| b.label() == s)
}

fn parse_band_cap(s: &str) -> Option<(Band, Cap)> {
    let (b, c) = s.split_once(':')?;
    let band = parse_band(b)?;
    let cap = Cap::ALL.into_iter().find(|x| x.label() == c)?;
    Some((band, cap))
}

/// Build the `(band, cap)` nodeSelector — inverse of
/// [`parse_selector`]. Same shape as
/// [`super::solve::Candidate::selector`] but usable from a bare
/// `(Band, Cap)` (the Pending-watch pin reuses this on re-emit).
pub fn selector_for(band: Band, cap: Cap) -> std::collections::BTreeMap<String, String> {
    std::collections::BTreeMap::from([
        ("rio.build/hw-band".into(), band.label().into()),
        ("karpenter.sh/capacity-type".into(), cap.label().into()),
    ])
}

/// Recover `(Band, Cap)` from a [`super::solve::Candidate::selector`]
/// nodeSelector. `None` for band-agnostic selectors (`solve_mvp` /
/// `BestEffort` paths). Used by the actor's Pending-watch to map a
/// stuck intent back to its ICE-backoff cell.
pub fn parse_selector(ns: &std::collections::BTreeMap<String, String>) -> Option<(Band, Cap)> {
    let band = parse_band(ns.get("rio.build/hw-band")?)?;
    let cap = Cap::ALL
        .into_iter()
        .find(|c| Some(c.label()) == ns.get("karpenter.sh/capacity-type").map(String::as_str))?;
    Some((band, cap))
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// In-process insufficient-capacity backoff. A `(band, cap)` that left
/// a pod Pending past the Pending-watch window is marked here for 60s;
/// [`super::solve::solve_full`] skips marked cells. Shared across all
/// dispatch threads (DashMap). Superseded by the admissible-set +
/// lead-time design; retained until that lands.
///
/// # Ladder protocol
///
/// 1. Dispatch picks `(b₀, c₀)` via `solve_full`, spawns.
/// 2. Pending-watch sees the pod still `Pending` after the 60s window
///    → [`Self::mark`]`(b₀, c₀)`, records it in the actor's per-drv
///    `ice_attempts`, deletes the pod, re-runs `solve_full` (which now
///    skips `(b₀, c₀)`).
/// 3. Repeat up to [`Self::ladder_cap`] times. On exhaust the drv
///    falls through to band-agnostic dispatch (`solve_intent_for`
///    skips `solve_full`; emits `infeasible_total{reason=
///    capacity_exhausted}`); the [`Self::encode_attempted`] JSON of
///    the attempt log is emitted at `debug!` for forensics. The
///    `builds.attempted_candidates` JSONB column (M_042) is reserved
///    for a future persisted write — currently log-only.
///
/// Step 2 is scheduler-side: the actor records `(drv_hash → (band,
/// cap, emitted_at))` when `solve_intent_for` first emits a
/// band-targeted SpawnIntent, clears the entry when a heartbeat with
/// `intent_id == drv_hash` arrives (pod made it past Pending), and on
/// each housekeeping tick marks ICE for entries older than the 60s
/// window (jittered ±20%). Tradeoff vs a controller-side Pod
/// watch: less precise (cannot distinguish `phase=Pending` from
/// "controller hasn't spawned yet" or "container crashed before first
/// heartbeat") — but all three cases mean the `(band, cap)` failed to
/// produce capacity within the window, and the 60s ICE TTL bounds the
/// false-positive cost. Avoids a proto change + cross-component RPC.
#[derive(Debug, Default)]
pub struct IceBackoff(DashMap<(Band, Cap), Instant>);

impl IceBackoff {
    /// Mark `(band, cap)` infeasible for `ICE_TTL` (60s).
    pub fn mark(&self, band: Band, cap: Cap) {
        self.0.insert((band, cap), Instant::now() + ICE_TTL);
    }

    /// Whether `(band, cap)` is currently backed off. Expired entries
    /// are reaped lazily on read.
    pub fn is_infeasible(&self, band: Band, cap: Cap) -> bool {
        // `.map(|r| *r)` copies the `Instant` and drops the `Ref` guard
        // BEFORE the match arms run. `get()` then `remove()` while the
        // guard is live deadlocks (DashMap shard RwLock is non-
        // reentrant) — this fired the first time any cell crossed the
        // 60s TTL and froze the single-threaded actor.
        match self.0.get(&(band, cap)).map(|r| *r) {
            Some(until) if until > Instant::now() => true,
            Some(_) => {
                self.0.remove(&(band, cap));
                false
            }
            None => false,
        }
    }

    /// Max ladder depth: `min(⌈max_tier_bound / hw_fallback_after / 4⌉, 8)`.
    /// ADR-023 §ICE: bound retries so a tier with a 1h p90 doesn't
    /// burn 30 fallback rounds before demoting.
    pub fn ladder_cap(max_tier_bound_secs: f64, hw_fallback_after_secs: f64) -> u32 {
        let raw = (max_tier_bound_secs / hw_fallback_after_secs / 4.0).ceil() as u32;
        raw.clamp(1, 8)
    }

    /// Count of currently-live backoff entries. For tests and
    /// debugging.
    pub fn live(&self) -> usize {
        let now = Instant::now();
        self.0.iter().filter(|e| *e.value() > now).count()
    }

    /// All 6 `(band, cap)` cells currently backed off → no candidate
    /// can survive `solve_full`'s ICE gate. Caller should demote a
    /// tier (or, at terminal tier, emit `infeasible_total{reason=
    /// capacity_exhausted}` and fail).
    pub fn exhausted(&self) -> bool {
        Band::ALL
            .into_iter()
            .all(|b| Cap::ALL.into_iter().all(|c| self.is_infeasible(b, c)))
    }

    /// `attempted_candidates` JSONB encoding for one ladder run.
    /// `[{band:"hi", cap:"spot"}, ...]` — forensics; the live state is
    /// in-process.
    pub fn encode_attempted(attempted: &[(Band, Cap)]) -> serde_json::Value {
        serde_json::Value::Array(
            attempted
                .iter()
                .map(|(b, c)| serde_json::json!({"band": b.label(), "cap": c.label()}))
                .collect(),
        )
    }
}

/// Lease-gated spot-price poller: every 10min, the leader pulls
/// `DescribeSpotPriceHistory` for each band's representative instance
/// type, EMA-smooths into `cost`, refreshes λ from `interrupt_samples`,
/// persists to PG, and exports the staleness gauge.
///
/// `source = None | Some(Static)` → AWS calls skipped; only the λ
/// refresh + gauge run. Standby replicas emit the staleness gauge
/// (per-replica, observability.md says it "climbs when … this replica is
/// standby") but skip the AWS/PG body. On a false→true leader edge the
/// in-mem table is reloaded from PG so the next leader picks up where
/// the last left off — without this, the standby's startup snapshot
/// (loaded once at main.rs) would be `persist()`ed on the first leader
/// tick, overwriting the previous leader's evolved EMA.
pub async fn spot_price_poller(
    db: SchedulerDb,
    leader: LeaderState,
    cost: std::sync::Arc<parking_lot::RwLock<CostTable>>,
    source: Option<HwCostSource>,
    shutdown: rio_common::signal::Token,
) {
    // EC2 client built once. Same `from_env()` chain as
    // `rio_common::s3::default_client` — IRSA in-cluster, profile/env
    // locally. None when source≠Spot so non-live deploys don't pay the
    // credential-chain probe.
    let ec2 = if matches!(source, Some(HwCostSource::Spot)) {
        Some(aws_sdk_ec2::Client::new(
            &aws_config::from_env().load().await,
        ))
    } else {
        None
    };
    let mut tick = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut was_leader = false;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {},
        }
        if !poller_tick_prelude(&mut was_leader, leader.is_leader(), &cost, &db, now_epoch()).await
        {
            continue;
        }
        // Snapshot → mutate → swap: parking_lot guards aren't Send, so
        // no await while holding one. CostTable is two small maps;
        // clone is cheap.
        let mut snap = cost.read().clone();
        if let Some(ec2) = &ec2 {
            match poll_spot_once(ec2).await {
                Ok(obs) if !obs.is_empty() => snap.fold_prices(&obs, now_epoch()),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "spot-price poll failed; keeping previous"),
            }
        }
        if let Err(e) = snap.refresh_lambda(&db).await {
            tracing::warn!(error = %e, "λ refresh failed; keeping previous");
        }
        if let Err(e) = sweep_interrupt_samples(&db, &snap.cluster).await {
            tracing::warn!(error = %e, "interrupt_samples retention sweep failed");
        }
        if let Err(e) = snap.persist(&db).await {
            tracing::warn!(error = %e, "cost-table persist failed");
        }
        // Re-evaluate stale-clamp on the post-fold timestamps and
        // re-emit the gauge so the leader's view doesn't lag one tick.
        let now = now_epoch();
        snap.apply_stale_clamp(now);
        super::metrics::hw_cost_stale_seconds(now - snap.price_updated_at());
        *cost.write() = snap;
    }
}

/// Retention sweep for `interrupt_samples`. The 24h-halflife EMA in
/// [`CostTable::refresh_lambda`] means rows >7d contribute ≈0, but the
/// controller's 60s exposure flush writes ~N_hw_classes `kind=
/// 'exposure'` rows/min with `event_uid=NULL` (unconstrained by M_047)
/// — append-only ~5-10M rows/yr/cluster without this. Mirrors the
/// `build_samples` age-sweep at `db/history.rs`; the `(cluster, at)`
/// index from M_043 makes the range delete cheap. Lease-gated via the
/// caller (one writer).
pub(crate) async fn sweep_interrupt_samples(db: &SchedulerDb, cluster: &str) -> sqlx::Result<u64> {
    let r = sqlx::query(
        "DELETE FROM interrupt_samples \
         WHERE cluster = $1 AND at < now() - interval '7 days'",
    )
    .bind(cluster)
    .execute(db.pool())
    .await?;
    Ok(r.rows_affected())
}

/// Per-tick gauge-emit + leader-edge-reload, factored out of
/// [`spot_price_poller`] for unit-testability (the poller body needs a
/// live EC2 client). Returns `true` if the caller should proceed with
/// the AWS/PG tick body.
///
/// - Emits `rio_scheduler_sla_hw_cost_stale_seconds` BEFORE the leader
///   gate (per-replica metric — observability.md says it "climbs when …
///   this replica is standby"; `r[obs.metric.scheduler-leader-gate]`
///   does NOT list it).
/// - On a false→true leader edge, reloads from PG so the new leader
///   resumes from the previous leader's persisted state, not its own
///   startup snapshot.
pub(crate) async fn poller_tick_prelude(
    was_leader: &mut bool,
    is_leader: bool,
    cost: &std::sync::Arc<parking_lot::RwLock<CostTable>>,
    db: &SchedulerDb,
    now: f64,
) -> bool {
    super::metrics::hw_cost_stale_seconds(now - cost.read().price_updated_at());
    if !is_leader {
        *was_leader = false;
        return false;
    }
    // r[impl sched.sla.cost-leader-edge-reload]
    if !*was_leader {
        let cluster = cost.read().cluster().to_owned();
        match CostTable::load(db, &cluster).await {
            Ok(mut fresh) => {
                // Reloaded EMA may itself be stale (previous leader
                // wedged before failover) — evaluate clamp on the
                // resumed timestamps so the FIRST solve under new
                // leadership doesn't trust a dead snapshot.
                fresh.apply_stale_clamp(now);
                *cost.write() = fresh;
                *was_leader = true;
            }
            Err(e) => {
                tracing::warn!(error = %e, "cost reload on leader-acquire failed; retrying next tick");
                // Do NOT latch `was_leader` and do NOT proceed: the
                // tick body would `persist()` this replica's stale
                // startup snapshot over the previous leader's evolved
                // EMA. Retried on the next tick.
                return false;
            }
        }
    }
    true
}

/// Representative instance types per band for the spot-price poll. The
/// 12-NodePool topology admits c/m/r families across gen 6-8; querying a
/// few c+m `.large` shapes per band and taking the median gives a stable
/// `$/vCPU·hr` (the per-vCPU price is near-flat across sizes within a
/// family). Graviton + x86 are both included so a band-wide ARM
/// discount (or x86 premium) shows up in the EMA.
///
/// Band is tagged explicitly per entry — bands are non-disjoint by
/// generation (helm `mid` spans 6+7), so it cannot be recovered by
/// parsing the family's generation digit.
const BAND_INSTANCE_TYPES: &[(Band, &str)] = &[
    (Band::Hi, "c8g.large"),
    (Band::Hi, "m8g.large"),
    (Band::Mid, "c7a.large"),
    (Band::Mid, "c7g.large"),
    (Band::Mid, "m7a.large"),
    (Band::Mid, "m7g.large"),
    (Band::Lo, "c6a.large"),
    (Band::Lo, "c6g.large"),
    (Band::Lo, "m6a.large"),
    (Band::Lo, "m6g.large"),
];

/// One `DescribeSpotPriceHistory` round. Returns vCPU-normalized
/// `$/vCPU·hr` per `(band, Cap::Spot)`.
///
/// Queries the last hour of `Linux/UNIX` spot-price history for
/// [`BAND_INSTANCE_TYPES`], normalizes each row by its vCPU count
/// (from `DescribeInstanceTypes`, cached for the process lifetime),
/// then takes the per-band median. Median not mean: a single AZ's
/// spot spike for one type shouldn't drag the whole band. On-demand
/// prices stay seed-backed (no public on-demand API without
/// `pricing:GetProducts`).
async fn poll_spot_once(ec2: &aws_sdk_ec2::Client) -> anyhow::Result<HashMap<(Band, Cap), f64>> {
    use aws_sdk_ec2::types::InstanceType;

    // vCPU-count cache. Process-lifetime static: instance-type vCPU
    // counts are immutable. One DescribeInstanceTypes round-trip on
    // first poll, then never again.
    static VCPU: tokio::sync::OnceCell<HashMap<String, f64>> = tokio::sync::OnceCell::const_new();
    let vcpu = VCPU
        .get_or_try_init(|| async {
            let r = ec2
                .describe_instance_types()
                .set_instance_types(Some(
                    BAND_INSTANCE_TYPES
                        .iter()
                        .map(|(_, t)| InstanceType::from(*t))
                        .collect(),
                ))
                .send()
                .await?;
            anyhow::Ok(
                r.instance_types()
                    .iter()
                    .filter_map(|it| {
                        Some((
                            it.instance_type()?.as_str().to_owned(),
                            f64::from(it.v_cpu_info()?.default_v_cpus()?),
                        ))
                    })
                    .collect::<HashMap<_, _>>(),
            )
        })
        .await?;

    // Spot history, last hour, all configured types, paginated. AWS
    // returns one row per (type, AZ, price-change); a quiet hour can
    // be empty for some types — those just drop out of the median.
    let band_of: HashMap<&str, Band> = BAND_INSTANCE_TYPES.iter().map(|&(b, t)| (t, b)).collect();
    let start = aws_sdk_ec2::primitives::DateTime::from_secs((now_epoch() - 3600.0) as i64);
    let mut per_band: HashMap<Band, Vec<f64>> = HashMap::new();
    let mut pages = ec2
        .describe_spot_price_history()
        .set_instance_types(Some(
            BAND_INSTANCE_TYPES
                .iter()
                .map(|(_, t)| InstanceType::from(*t))
                .collect(),
        ))
        .product_descriptions("Linux/UNIX")
        .start_time(start)
        .into_paginator()
        .send();
    while let Some(page) = pages.try_next().await? {
        for row in page.spot_price_history() {
            let Some(t) = row.instance_type().map(|t| t.as_str()) else {
                continue;
            };
            let Some(&band) = band_of.get(t) else {
                continue;
            };
            let Some(v) = vcpu.get(t).copied().filter(|v| *v > 0.0) else {
                continue;
            };
            let Some(price) = row.spot_price().and_then(|p| p.parse::<f64>().ok()) else {
                continue;
            };
            per_band.entry(band).or_default().push(price / v);
        }
    }

    Ok(per_band
        .into_iter()
        .filter_map(|(b, mut xs)| {
            if xs.is_empty() {
                return None;
            }
            xs.sort_by(|a, b| a.total_cmp(b));
            Some(((b, Cap::Spot), xs[xs.len() / 2]))
        })
        .collect())
}

impl HwTable {
    /// Per-pname effective-slowest hw_class within `band`: the `h ∈
    /// band` minimizing `factor[h] / bias[pname,h]`. ADR-023 §h†: the
    /// envelope solve is conservative — it sizes for the SLOWEST
    /// hardware the pod might land on within the band, adjusted for
    /// this pname's per-hw bias (a mem-bandwidth-bound build that
    /// underperforms on a fast-core class gets that class's effective
    /// factor pulled down). Returns `(hw_class, effective_factor)`;
    /// falls back to `("", 1.0)` if no hw_class in the band has ≥3
    /// samples (factor=1.0 → reference timeline).
    pub fn h_dagger(
        &self,
        _pname: &str,
        band: Band,
        hw_bias: &HashMap<String, f64>,
    ) -> (String, f64) {
        self.iter()
            .filter(|(h, _)| band_of_hw_class(h) == Some(band))
            .map(|(h, f)| {
                let bias = hw_bias.get(h).copied().unwrap_or(1.0);
                (h.clone(), f / bias)
            })
            .min_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            // `f / bias` can push below [`HW_FACTOR_SANITY_FLOOR`] even
            // with clamped inputs (e.g. 0.25/2.0). The result is divided
            // into `T(c)` at solve_full's `feasible` gate; an unclamped
            // tiny factor blows `t = T(c)/factor` up → `feasible(cap_c)
            // = false` → entire band drops out for both Spot AND
            // OnDemand.
            .map(|(h, f)| (h, f.max(super::hw::HW_FACTOR_SANITY_FLOOR)))
            .unwrap_or_else(|| (String::new(), 1.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ratio_ema_decays() {
        let mut e = RatioEma::default();
        e.update(10.0, 100.0, 1000.0, 3600.0);
        assert!((e.value_or(0.0) - 0.1).abs() < 1e-9);
        // One halflife later, add nothing → both halves halved → ratio unchanged.
        e.update(0.0, 0.0, 4600.0, 3600.0);
        assert!((e.value_or(0.0) - 0.1).abs() < 1e-9);
        // Add a burst of interrupts with no exposure → ratio rises.
        e.update(5.0, 0.0, 4600.0, 3600.0);
        assert!(e.value_or(0.0) > 0.1);
    }

    // r[verify sched.sla.hw-class.lambda-gamma-poisson]
    #[test]
    fn lambda_gamma_poisson_pools_toward_seed() {
        // 1 interrupt over 1h exposure, 1-node fleet, seed=1e-5/s.
        // n_λ = 86400·max(1,1) = 86400.
        // λ̂ = (1 + 86400·1e-5) / (3600 + 86400) = 1.864 / 90000 ≈ 2.071e-5.
        // Bare ratio is 1/3600 ≈ 2.78e-4 — pooling pulls it 13× toward
        // the seed instead of letting one event spike the band.
        let l = lambda_hat(1.0, 3600.0, 1.0, 1e-5);
        let want = (1.0 + 86400.0 * 1e-5) / (3600.0 + 86400.0);
        assert!((l - want).abs() < 1e-12, "{l}");
        assert!((l - 2.071e-5).abs() / 2.071e-5 < 0.01);
        assert!(l < 1.0 / 3600.0, "pooled below the bare ratio");
    }

    #[test]
    fn lambda_seed_floor_when_spot_exiled() {
        // node_count_ema=0 → max(1,0)=1 → n_λ=86400. With 0 interrupts,
        // 0 exposure: λ̂ = (0 + 86400·seed) / (0 + 86400) = seed. The
        // floor is what keeps λ̂ from freezing at the spike when exile
        // drives node_count → 0.
        let l = lambda_hat(0.0, 0.0, 0.0, 1e-5);
        assert!((l - 1e-5).abs() < 1e-12);
        // Same floor at the CostTable level (no entries → seed).
        assert!((CostTable::default().lambda_band(Band::Mid) - LAMBDA_SEED).abs() < 1e-12);
    }

    #[test]
    fn lambda_band_uses_node_count_scaler() {
        // 100-node fleet: n_λ = 86400·100 = 8.64e6. Prior swamps a
        // single interrupt over 1h — λ̂ ≈ seed (within 0.05%).
        let mut t = CostTable::default();
        t.lambda.insert(
            Band::Mid,
            RatioEma {
                numerator: 1.0,
                denominator: 3600.0,
                updated_at: 1000.0,
            },
        );
        t.set_node_count(Band::Mid, 100.0, 1000.0);
        let l = t.lambda_band(Band::Mid);
        assert!((l - LAMBDA_SEED).abs() / LAMBDA_SEED < 1e-3, "{l}");
    }

    #[test]
    fn band_of_hw_class_parses_band_label() {
        assert_eq!(band_of_hw_class("aws-8-nvme-hi"), Some(Band::Hi));
        assert_eq!(band_of_hw_class("intel-7-ebs-mid"), Some(Band::Mid));
        assert_eq!(band_of_hw_class("amd-6-ebs-lo"), Some(Band::Lo));
        // Regression: helm `mid` admits gen 6+7. A c6id provisioned via
        // the mid NodePool is `gen=6, band=mid` — must map to Mid, NOT
        // Lo. This is why band is keyed on the label segment, not the
        // generation digit.
        assert_eq!(band_of_hw_class("amd-6-nvme-mid"), Some(Band::Mid));
        // No band segment / unknown band → None.
        assert_eq!(band_of_hw_class("aws-7-ebs"), None);
        assert_eq!(band_of_hw_class("unknown-unknown-ebs-unknown"), None);
    }

    #[test]
    fn parse_selector_roundtrips_candidate() {
        let ns = std::collections::BTreeMap::from([
            ("rio.build/hw-band".into(), "mid".into()),
            ("karpenter.sh/capacity-type".into(), "spot".into()),
        ]);
        assert_eq!(parse_selector(&ns), Some((Band::Mid, Cap::Spot)));
        assert_eq!(parse_selector(&Default::default()), None);
    }

    /// `poll_spot_once`: per-band median of `price/vCPU` over the
    /// returned history. Mock both EC2 calls; assert the median pick
    /// (not mean — the 0.10 outlier for Mid is ignored).
    #[tokio::test]
    async fn poll_spot_once_median_per_band() {
        use aws_sdk_ec2::types::{InstanceTypeInfo, SpotPrice, VCpuInfo};
        use aws_smithy_mocks::{RuleMode, mock, mock_client};
        type Ec2 = aws_sdk_ec2::Client;

        let it = |name: &str, vcpu: i32| {
            InstanceTypeInfo::builder()
                .instance_type(name.into())
                .v_cpu_info(VCpuInfo::builder().default_v_cpus(vcpu).build())
                .build()
        };
        let sp = |name: &str, price: &str| {
            SpotPrice::builder()
                .instance_type(name.into())
                .spot_price(price)
                .build()
        };
        let types = mock!(Ec2::describe_instance_types).then_output(move || {
            aws_sdk_ec2::operation::describe_instance_types::DescribeInstanceTypesOutput::builder()
                .instance_types(it("c8g.large", 2))
                .instance_types(it("c7a.large", 2))
                .instance_types(it("m7a.large", 4))
                .build()
        });
        let history = mock!(Ec2::describe_spot_price_history).then_output(move || {
            aws_sdk_ec2::operation::describe_spot_price_history::DescribeSpotPriceHistoryOutput::builder()
                // Hi: one sample → 0.04/2 = 0.02.
                .spot_price_history(sp("c8g.large", "0.0400"))
                // Mid: three samples → median of [0.03/2, 0.05/2, 0.40/4]
                // = median of [0.015, 0.025, 0.10] = 0.025.
                .spot_price_history(sp("c7a.large", "0.0300"))
                .spot_price_history(sp("c7a.large", "0.0500"))
                .spot_price_history(sp("m7a.large", "0.4000"))
                // Unparseable price + unknown type: dropped.
                .spot_price_history(sp("c7a.large", "n/a"))
                .spot_price_history(sp("c5.large", "0.0100"))
                .build()
        });
        let client = mock_client!(aws_sdk_ec2, RuleMode::MatchAny, &[&types, &history]);

        let obs = poll_spot_once(&client).await.unwrap();
        assert_eq!(obs.len(), 2, "Lo had no rows → absent");
        assert!((obs[&(Band::Hi, Cap::Spot)] - 0.02).abs() < 1e-9);
        assert!((obs[&(Band::Mid, Cap::Spot)] - 0.025).abs() < 1e-9);
        assert!(!obs.contains_key(&(Band::Lo, Cap::Spot)));
    }

    /// `persist`/`load`/`refresh_lambda` are cluster-scoped: two
    /// schedulers writing to the same global DB with different
    /// `cluster` keys don't read each other's EMAs or interrupt rows.
    /// ADR-023 §2.13 regression — pre-043 the `key` PK collided.
    #[tokio::test]
    async fn persist_load_cluster_scoped() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());

        // Cluster A writes price=0.5 for (Hi, Spot) and a Hi-band
        // interrupt row.
        let mut a = CostTable::seeded("us-east-1");
        a.set_price(Band::Hi, Cap::Spot, 0.5, 1000.0);
        a.persist(&sdb).await.unwrap();
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value) \
             VALUES ('us-east-1', 'aws-8-nvme-hi', 'interrupt', 5), \
                    ('us-east-1', 'aws-8-nvme-hi', 'exposure', 100)",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Cluster B loads → sees seeds (NOT A's 0.5), and refresh_lambda
        // sees no rows.
        let mut b = CostTable::load(&sdb, "eu-west-2").await.unwrap();
        assert!(
            (b.price(Band::Hi, Cap::Spot) - 0.5).abs() > 1e-3,
            "B leaked A's price"
        );
        b.refresh_lambda(&sdb).await.unwrap();
        assert!(b.lambda.is_empty(), "B leaked A's interrupt rows");

        // Cluster A reload roundtrips its own price.
        let a2 = CostTable::load(&sdb, "us-east-1").await.unwrap();
        assert!((a2.price(Band::Hi, Cap::Spot) - 0.5).abs() < 1e-9);
        // And sees its own interrupt rows.
        let mut a3 = CostTable::seeded("us-east-1");
        a3.refresh_lambda(&sdb).await.unwrap();
        assert!(a3.lambda.contains_key(&Band::Hi));

        // B persists then A reloads: A's price unchanged (PK is
        // (cluster, key) — no overwrite).
        b.set_price(Band::Hi, Cap::Spot, 0.01, 2000.0);
        b.persist(&sdb).await.unwrap();
        let a4 = CostTable::load(&sdb, "us-east-1").await.unwrap();
        assert!((a4.price(Band::Hi, Cap::Spot) - 0.5).abs() < 1e-9);
    }

    /// Regression: `persist()` wrote `updated_at = now()` instead of
    /// the per-key data-time, so a tick where `poll_spot_once` failed
    /// still advanced the persisted timestamp → on reload staleness was
    /// lost and the next `fold_prices` `dt` was wrong.
    #[tokio::test]
    async fn persist_preserves_price_updated_at() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());
        let mut t = CostTable::seeded("c");
        t.set_price(Band::Hi, Cap::Spot, 0.5, 1000.0);
        t.persist(&sdb).await.unwrap();
        let r = CostTable::load(&sdb, "c").await.unwrap();
        let at = r.price.get(&(Band::Hi, Cap::Spot)).unwrap().updated_at;
        assert!(
            (at - 1000.0).abs() < 1.0,
            "reloaded updated_at must be data-time 1000, not now(); got {at}"
        );
    }

    /// Full EMA-state (price + λ num/den + node_count) round-trips PG
    /// so a lease failover resumes the smoothed values rather than
    /// resetting to seed. ADR-023 §Cost-model "persisted to PG each
    /// tick".
    #[tokio::test]
    async fn ema_state_round_trips_pg() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());
        let mut t = CostTable::seeded("c");
        t.set_price(Band::Mid, Cap::Spot, 0.0123, 7000.0);
        t.lambda.insert(
            Band::Mid,
            RatioEma {
                numerator: 3.0,
                denominator: 9000.0,
                updated_at: 7100.0,
            },
        );
        t.set_node_count(Band::Mid, 12.5, 7100.0);
        t.persist(&sdb).await.unwrap();

        let r = CostTable::load(&sdb, "c").await.unwrap();
        assert!((r.price(Band::Mid, Cap::Spot) - 0.0123).abs() < 1e-9);
        let l = r.lambda.get(&Band::Mid).unwrap();
        assert!((l.numerator - 3.0).abs() < 1e-9);
        assert!((l.denominator - 9000.0).abs() < 1e-9);
        assert!((l.updated_at - 7100.0).abs() < 1.0);
        let nc = r.node_count.get(&Band::Mid).unwrap();
        assert!((nc.value - 12.5).abs() < 1e-9);
        assert!((nc.updated_at - 7100.0).abs() < 1.0);
        // λ̂ recomputed identically from the round-tripped state.
        assert!((r.lambda_band(Band::Mid) - t.lambda_band(Band::Mid)).abs() < 1e-12);
    }

    /// `> 6 × pollInterval` stale → `price()` clamps to the static seed
    /// and `_hw_cost_fallback_total{reason="stale"}` fires. Fresh →
    /// clamp clears and `price()` reads through.
    #[test]
    fn stale_price_clamps_to_seed_and_emits_fallback() {
        let mut t = CostTable::seeded("c");
        t.set_price(Band::Hi, Cap::Spot, 0.5, 1000.0);
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _g = metrics::set_default_local_recorder(&rec);

        // Stale: now − updated_at = 7200 > 3600.
        assert!(t.apply_stale_clamp(1000.0 + STALE_CLAMP_AFTER_SECS + 1.0));
        assert!(
            (t.price(Band::Hi, Cap::Spot) - seed_price(Band::Hi, Cap::Spot)).abs() < 1e-9,
            "clamped → seed, not 0.5"
        );
        let fired = snap.snapshot().into_vec().iter().any(|(ck, _, _, _)| {
            ck.key().name() == "rio_scheduler_sla_hw_cost_fallback_total"
                && ck.key().labels().any(|l| l.value() == "stale")
        });
        assert!(fired, "fallback_total{{reason=stale}} must increment");

        // Fresh: clamp clears; price() reads through.
        t.set_price(Band::Hi, Cap::Spot, 0.5, 9000.0);
        assert!(!t.apply_stale_clamp(9000.0 + 60.0));
        assert!((t.price(Band::Hi, Cap::Spot) - 0.5).abs() < 1e-9);
    }

    /// `refresh_lambda` derives `node_count_ema = Σ exposure / Δt` over
    /// the batch window. First refresh has no baseline (`prev_hwm=0`)
    /// → skipped; second refresh computes `120s / 60s = 2 nodes`.
    #[tokio::test]
    async fn refresh_lambda_derives_node_count_from_exposure() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value, at) VALUES \
             ('c', 'aws-8-nvme-hi', 'exposure', 60, to_timestamp(1000))",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let mut t = CostTable::seeded("c");
        t.refresh_lambda(&sdb).await.unwrap();
        assert!(t.node_count.is_empty(), "first refresh: no baseline");

        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value, at) VALUES \
             ('c', 'aws-8-nvme-hi', 'exposure', 120, to_timestamp(1060))",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        t.refresh_lambda(&sdb).await.unwrap();
        let nc = t.node_count.get(&Band::Hi).unwrap().value;
        assert!(
            (nc - 2.0).abs() < 1e-9,
            "120 node-secs / 60s window = 2; got {nc}"
        );
    }

    /// Regression: a single global `price_updated_at` advanced after
    /// folding only keys present in `obs`; a band absent from a partial
    /// obs kept its stale value but its decay reference moved forward
    /// → under-decayed on next fold. With per-key timestamps each
    /// value's decay `dt = now − that value's last-update`.
    #[test]
    fn fold_prices_partial_obs_decays_per_key() {
        let mut t = CostTable {
            price: HashMap::new(),
            lambda: HashMap::new(),
            node_count: HashMap::new(),
            stale_clamp: false,
            cluster: String::new(),
        };
        let mut obs = HashMap::new();
        obs.insert((Band::Hi, Cap::Spot), 0.02);
        obs.insert((Band::Mid, Cap::Spot), 0.01);
        t.fold_prices(&obs, 1000.0);
        // t=1600: Mid only (Hi absent).
        let mut obs2 = HashMap::new();
        obs2.insert((Band::Mid, Cap::Spot), 0.015);
        t.fold_prices(&obs2, 1600.0);
        // Hi's updated_at must NOT have moved.
        assert_eq!(t.price[&(Band::Hi, Cap::Spot)].updated_at, 1000.0);
        // t=2200: Hi reappears. dt=1200 (vs old global-stamp dt=600).
        let mut obs3 = HashMap::new();
        obs3.insert((Band::Hi, Cap::Spot), 0.03);
        t.fold_prices(&obs3, 2200.0);
        // decay = 0.5^(1200/SPOT_HALFLIFE_SECS); SPOT_HALFLIFE_SECS=3h.
        let decay = 0.5f64.powf(1200.0 / SPOT_HALFLIFE_SECS);
        let want = 0.02 * decay + 0.03 * (1.0 - decay);
        assert!(
            (t.price(Band::Hi, Cap::Spot) - want).abs() < 1e-9,
            "want {want}, got {}",
            t.price(Band::Hi, Cap::Spot)
        );
    }

    /// Regression (a): standby replicas `continue`d before the gauge
    /// `.set()` so `hw_cost_stale_seconds` was frozen on standby.
    /// Regression (b): on false→true leader edge the in-mem startup
    /// snapshot was `persist()`ed, overwriting the previous leader's
    /// evolved EMA.
    #[tokio::test]
    async fn poller_prelude_standby_emits_gauge_and_edge_reloads() {
        use std::sync::Arc;
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());

        // Seed PG with the previous leader's evolved state.
        let mut prev = CostTable::seeded("c");
        prev.set_price(Band::Hi, Cap::Spot, 0.08, 5000.0);
        prev.persist(&sdb).await.unwrap();

        // This replica's stale in-mem startup snapshot.
        let mut mine = CostTable::seeded("c");
        mine.set_price(Band::Hi, Cap::Spot, 0.02, 100.0);
        let cost = Arc::new(parking_lot::RwLock::new(mine));

        // (a) standby: emits gauge, returns false. Captured via local
        // recorder so parallel tests can't interfere.
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = rec.snapshotter();
        let mut was_leader = false;
        let proceed = {
            let _g = metrics::set_default_local_recorder(&rec);
            poller_tick_prelude(&mut was_leader, false, &cost, &sdb, 6100.0).await
        };
        assert!(!proceed);
        let saw_gauge = snapshotter
            .snapshot()
            .into_vec()
            .iter()
            .any(|(ck, _, _, _)| ck.key().name() == "rio_scheduler_sla_hw_cost_stale_seconds");
        assert!(saw_gauge, "standby must emit the staleness gauge");
        // Standby did NOT reload (still 0.02).
        assert!((cost.read().price(Band::Hi, Cap::Spot) - 0.02).abs() < 1e-9);

        // (b) false→true edge: reloads from PG, returns true.
        let proceed = poller_tick_prelude(&mut was_leader, true, &cost, &sdb, 6100.0).await;
        assert!(proceed);
        assert!(was_leader);
        assert!(
            (cost.read().price(Band::Hi, Cap::Spot) - 0.08).abs() < 1e-9,
            "leader-edge must reload PG state, not keep startup snapshot"
        );

        // Subsequent leader tick: no reload (would clobber in-flight
        // mutation if it did).
        cost.write().set_price(Band::Hi, Cap::Spot, 0.09, 6000.0);
        let proceed = poller_tick_prelude(&mut was_leader, true, &cost, &sdb, 6100.0).await;
        assert!(proceed);
        assert!((cost.read().price(Band::Hi, Cap::Spot) - 0.09).abs() < 1e-9);
    }

    /// Regression: when `CostTable::load` fails on the false→true
    /// leader edge, the prelude must NOT latch `was_leader=true` and
    /// must NOT return `true` — doing so would let the caller
    /// `persist()` this replica's stale startup snapshot over the
    /// previous leader's evolved EMA, and skip the reload retry on
    /// every subsequent tick.
    // r[verify sched.sla.cost-leader-edge-reload]
    #[tokio::test]
    async fn poller_prelude_load_failure_retries_and_skips_persist() {
        use std::sync::Arc;
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());

        // Seed PG with the previous leader's evolved state.
        let mut prev = CostTable::seeded("c");
        prev.set_price(Band::Hi, Cap::Spot, 0.08, 5000.0);
        prev.persist(&sdb).await.unwrap();

        // This replica's stale in-mem startup snapshot.
        let mut mine = CostTable::seeded("c");
        mine.set_price(Band::Hi, Cap::Spot, 0.02, 100.0);
        let cost = Arc::new(parking_lot::RwLock::new(mine));

        // Broken DB: a separate pool closed before use → load() Errs.
        // (PgPool is Arc-backed; closing a clone of `db.pool` would
        // also break `sdb`.)
        let bad = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        bad.pool.close().await;
        let bad_db = SchedulerDb::new(bad.pool.clone());

        let mut was_leader = false;
        let proceed = poller_tick_prelude(&mut was_leader, true, &cost, &bad_db, 6100.0).await;
        assert!(
            !proceed,
            "load() Err → tick body skipped (no persist of stale snapshot)"
        );
        assert!(
            !was_leader,
            "load() Err → was_leader stays false so next tick retries"
        );
        assert!(
            (cost.read().price(Band::Hi, Cap::Spot) - 0.02).abs() < 1e-9,
            "in-mem unchanged on Err"
        );

        // Retry with a working DB: reload succeeds, latches, proceeds.
        let proceed = poller_tick_prelude(&mut was_leader, true, &cost, &sdb, 6100.0).await;
        assert!(proceed, "retry with working DB → proceed");
        assert!(was_leader, "retry success → latched");
        assert!(
            (cost.read().price(Band::Hi, Cap::Spot) - 0.08).abs() < 1e-9,
            "retry reloaded PG state (previous leader's EMA)"
        );
    }

    /// `refresh_lambda` advances `updated_at` to the rows' `MAX(at)`,
    /// not the scheduler's wall-clock. Regression: the SQL always
    /// computed `MAX(at)` but the destructure discarded it and used
    /// `now_epoch()` — a row whose PG-stamped `at` was behind the
    /// scheduler clock (skew / commit-lag) was permanently skipped on
    /// the next tick.
    #[tokio::test]
    async fn refresh_lambda_hwm_from_rows_not_wallclock() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());
        // Row stamped well in the past — wall-clock now() is ~56 years
        // ahead of this.
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value, at) \
             VALUES ('c', 'aws-8-nvme-hi', 'interrupt', 1, to_timestamp(1000)), \
                    ('c', 'aws-8-nvme-hi', 'exposure', 100, to_timestamp(1500))",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let mut t = CostTable::seeded("c");
        t.refresh_lambda(&sdb).await.unwrap();
        let hwm = t.lambda[&Band::Hi].updated_at;
        assert_eq!(hwm, 1500.0, "HWM must be MAX(at), got {hwm}");
        // Second tick with a row at at=1200 (between prev rows): the
        // `at > to_timestamp(1500)` filter excludes it, AND hwm stays
        // 1500 — does not jump to wall-clock.
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value, at) \
             VALUES ('c', 'aws-8-nvme-hi', 'exposure', 50, to_timestamp(1200))",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        t.refresh_lambda(&sdb).await.unwrap();
        assert_eq!(t.lambda[&Band::Hi].updated_at, 1500.0);
    }

    /// `interrupt_samples` is bounded: rows >7d are swept (the 24h-
    /// halflife EMA gives them ≈0 weight). Regression: the 60s
    /// exposure flush wrote ~N_hw_classes rows/min with no retention
    /// — append-only ~5-10M rows/yr/cluster. Per-cluster scoped: a
    /// stale row in another cluster is not swept.
    #[tokio::test]
    async fn interrupt_samples_retention_sweep() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value, at) VALUES \
             ('c', 'aws-8-nvme-hi', 'exposure', 60, now() - interval '8 days'), \
             ('c', 'aws-8-nvme-hi', 'exposure', 60, now() - interval '6 days'), \
             ('c', 'aws-8-nvme-hi', 'exposure', 60, now()), \
             ('other', 'aws-8-nvme-hi', 'exposure', 60, now() - interval '8 days')",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        let n = sweep_interrupt_samples(&sdb, "c").await.unwrap();
        assert_eq!(n, 1, "exactly the >7d row in cluster c");
        let left: Vec<(String, f64)> = sqlx::query_as(
            "SELECT cluster, EXTRACT(EPOCH FROM now() - at)::float8 \
             FROM interrupt_samples ORDER BY cluster, at",
        )
        .fetch_all(&db.pool)
        .await
        .unwrap();
        assert_eq!(left.len(), 3, "kept: 2×c (≤7d) + 1×other (untouched)");
        assert!(
            left.iter()
                .filter(|(c, _)| c == "c")
                .all(|(_, age)| *age < 7.0 * 86400.0),
            "all surviving cluster-c rows ≤7d"
        );
        assert!(left.iter().any(|(c, _)| c == "other"));
    }

    #[test]
    fn price_seed_backed() {
        let t = CostTable::default();
        // Seeds: spot < on-demand for every band.
        for b in Band::ALL {
            assert!(t.price(b, Cap::Spot) < t.price(b, Cap::OnDemand));
        }
        // Lo cheapest, Hi priciest (on-demand seeds).
        assert!(t.price(Band::Lo, Cap::OnDemand) < t.price(Band::Hi, Cap::OnDemand));
    }

    #[test]
    fn ice_mark_and_expire() {
        let ice = IceBackoff::default();
        assert!(!ice.is_infeasible(Band::Hi, Cap::Spot));
        ice.mark(Band::Hi, Cap::Spot);
        assert!(ice.is_infeasible(Band::Hi, Cap::Spot));
        assert!(!ice.is_infeasible(Band::Hi, Cap::OnDemand));
        assert_eq!(ice.live(), 1);
    }

    /// Regression: the lazy-reap arm previously called `remove()` while
    /// the match scrutinee's `Ref` guard was still live → DashMap shard
    /// deadlock. Seed a past-expiry entry directly so the test doesn't
    /// wait `ICE_TTL`; if the deadlock is reintroduced, this hangs and
    /// nextest's per-test timeout catches it.
    #[test]
    fn ice_expired_entry_reaped_without_deadlock() {
        let ice = IceBackoff::default();
        ice.0.insert((Band::Hi, Cap::Spot), Instant::now());
        assert!(!ice.is_infeasible(Band::Hi, Cap::Spot), "expired → false");
        assert!(
            ice.0.get(&(Band::Hi, Cap::Spot)).is_none(),
            "expired entry removed on read"
        );
        // Same lazy-reap hazard across all 6 cells.
        for b in Band::ALL {
            for c in Cap::ALL {
                ice.0.insert((b, c), Instant::now());
            }
        }
        for b in Band::ALL {
            for c in Cap::ALL {
                assert!(!ice.is_infeasible(b, c), "expired → false, no deadlock");
            }
        }
    }

    #[test]
    fn encode_attempted_roundtrips_labels() {
        let v = IceBackoff::encode_attempted(&[(Band::Hi, Cap::Spot), (Band::Lo, Cap::OnDemand)]);
        assert_eq!(
            v.to_string(),
            r#"[{"band":"hi","cap":"spot"},{"band":"lo","cap":"on-demand"}]"#
        );
    }

    #[test]
    fn ladder_cap_bounds() {
        // 1h tier, 120s fallback → ceil(3600/120/4)=8 → clamp 8.
        assert_eq!(IceBackoff::ladder_cap(3600.0, 120.0), 8);
        // 5min tier → ceil(300/120/4)=1.
        assert_eq!(IceBackoff::ladder_cap(300.0, 120.0), 1);
        // Huge tier → still 8.
        assert_eq!(IceBackoff::ladder_cap(86400.0, 120.0), 8);
    }
}
