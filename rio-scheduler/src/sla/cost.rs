//! ADR-023 phase-13 hw-class + capacity-type cost model.
//!
//! Two halves, both feeding [`super::solve::solve_full`]:
//!
//! - [`CostTable`]: per-[`Cell`] `$/vCPU·hr` snapshot + per-cell
//!   instance-type [`menu`](CostTable::menu). Populated by
//!   [`spot_price_poller`] (lease-gated, 10min tick, 3h-halflife EMA
//!   over `DescribeSpotPriceHistory`) and persisted to `sla_ema_state`
//!   so a restart doesn't re-warm. `smallest_fitting` resolves the
//!   `price_per_vcpu_hr` of what Karpenter would actually launch for
//!   a given `(c*, M(c*))`.
//! - λ\[h\]: per-hw-class Poisson interrupt rate. Gamma-Poisson partial
//!   pooling over `interrupt_samples` (controller-appended): the seed
//!   acts as a prior with weight `n_λ = 1day · max(1, node_count_ema)`
//!   so a single interrupt doesn't spike λ, and exiled-spot decay
//!   collapses to the seed rather than freezing at the spike.
//!
//! [`IceBackoff`] is the in-process insufficient-capacity mask: a
//! [`Cell`] reported `unfulfillable` by the controller (NodeClaim
//! `Launched=False` or `Registered` timeout) is masked fleet-wide with
//! exponential backoff `60s → 120s → … ≤ max_lead_time`, reset on
//! first success. The mask is **read-time** — the per-key solve memo is
//! never overwritten; each dispatch computes `A \ ice_masked`.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::db::SchedulerDb;
use crate::lease::LeaderState;

use super::config::{CapacityType, Cell, HwClassName, cell_label, parse_cell};

/// One instance type in a cell's menu. `price_per_vcpu_hr` is the
/// EMA-smoothed `$/vCPU·hr` (or seed); `cores`/`mem_bytes` gate
/// `smallest_fitting`.
#[derive(Debug, Clone)]
pub struct InstanceType {
    pub name: String,
    pub cores: u32,
    pub mem_bytes: u64,
    pub price_per_vcpu_hr: f64,
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

/// Seed `$/vCPU·hr`, on-demand. Roughly c7a list price ÷ vCPU. Under
/// the admissible-set solve only the spot/od *ratio* matters when the
/// per-h price EMA is unpopulated (every h shares this seed → the
/// solve degenerates to "argmin λ\[h\]").
pub const ON_DEMAND_SEED: f64 = 0.043;

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

/// First step of the per-cell exponential ICE backoff (`60s → 120s →
/// … ≤ max_lead_time`). ADR-023 §Capacity backoff.
const ICE_BASE_TTL: Duration = Duration::from_secs(60);

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

/// Static `$/vCPU·hr` seed for `cap`. Backstop for [`CostTable::price`]
/// (missing key, or stale-clamped). Per-h price differentiation comes
/// from the live poller / menu; the seed is cap-only.
fn seed_price(cap: CapacityType) -> f64 {
    match cap {
        CapacityType::Od => ON_DEMAND_SEED,
        CapacityType::Spot => ON_DEMAND_SEED * SPOT_SEED_DISCOUNT,
    }
}

/// Per-[`Cell`] `$/vCPU·hr` + per-h λ + per-cell instance-type menu.
/// Cheap to clone (small maps); the solve takes a snapshot by value.
#[derive(Debug, Clone, Default)]
pub struct CostTable {
    /// EMA-smoothed `$/vCPU·hr`. Missing key → seed.
    price: HashMap<Cell, PriceEma>,
    /// Per-h interrupt-rate EMA. `numerator` = Σ interrupts,
    /// `denominator` = Σ exposure-secs (24h halflife). Read via
    /// [`lambda_hat`], not as a bare ratio.
    lambda: HashMap<HwClassName, RatioEma>,
    /// Per-h 24h-EMA of live spot-node count. The `n_λ` scaler in
    /// [`lambda_hat`]: keeps the prior's relative weight ~constant at
    /// "one day of fleet exposure" regardless of fleet size. Derived
    /// from `interrupt_samples` exposure rows in
    /// [`CostTable::refresh_lambda`] as `Σ exposure_secs / Δt`.
    node_count: HashMap<HwClassName, PriceEma>,
    /// Per-cell instance-type menu, sorted by `cores` asc. Seeded from
    /// helm `sla.hwClasses[*].menu` (Part-B); the poller refreshes
    /// `price_per_vcpu_hr` in-place. Empty ⇔ [`Self::smallest_fitting`]
    /// degrades to the per-cell EMA scalar (one synthetic "fits-all"
    /// type) so the admissible-set solve produces candidates before
    /// menu population is wired.
    cells: HashMap<Cell, Vec<InstanceType>>,
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

impl CostTable {
    /// `$/vCPU·hr` for `cell`. Seed-backed — never `None`. Clamps to
    /// seed while [`Self::apply_stale_clamp`] has the stale-clamp
    /// latched.
    pub fn price(&self, cell: &Cell) -> f64 {
        if self.stale_clamp {
            return seed_price(cell.1);
        }
        self.price
            .get(cell)
            .map(|p| p.value)
            .unwrap_or_else(|| seed_price(cell.1))
    }

    /// Per-h Poisson interrupt rate (events/sec) via [`lambda_hat`].
    /// `(EMA(interrupts) + n_λ·seed) / (EMA(exposure) + n_λ)` with
    /// `n_λ = 1day · max(1, node_count_ema)`. Returns [`LAMBDA_SEED`]
    /// for an h with no observations (default RatioEma + node_count=0
    /// reduces exactly).
    pub fn lambda_for(&self, h: &str) -> f64 {
        let ema = self.lambda.get(h).copied().unwrap_or_default();
        let nc = self.node_count.get(h).map(|p| p.value).unwrap_or(0.0);
        lambda_hat(ema.numerator, ema.denominator, nc, LAMBDA_SEED)
    }

    /// Instance-type menu for `cell`, sorted by `cores` asc. Empty
    /// before Part-B menu population.
    pub fn menu(&self, cell: &Cell) -> &[InstanceType] {
        self.cells.get(cell).map(Vec::as_slice).unwrap_or(&[])
    }

    /// `price_per_vcpu_hr` of the smallest instance type in `cell`
    /// fitting `(c, mem)` — i.e., what Karpenter would actually launch
    /// (ADR-023 L624). `None` ⇔ menu non-empty AND no type fits
    /// (cell rejected on capacity). Empty menu ⇔ degrade to the
    /// per-cell EMA scalar (one synthetic fits-all type).
    pub fn smallest_fitting(&self, cell: &Cell, c: u32, mem: u64) -> Option<f64> {
        let menu = self.menu(cell);
        if menu.is_empty() {
            return Some(self.price(cell));
        }
        menu.iter()
            .find(|t| t.cores >= c && t.mem_bytes >= mem)
            .map(|t| t.price_per_vcpu_hr)
    }

    /// Cheapest hw_class by `(h, Spot)` price. For the ε_h `A=H`
    /// fallback (`H \ {argmin_H price}`). Seed-backed so always
    /// returns some h when `hw_classes` is non-empty.
    pub fn cheapest_h<'a>(
        &self,
        hw_classes: impl IntoIterator<Item = &'a HwClassName>,
    ) -> Option<HwClassName> {
        hw_classes
            .into_iter()
            .min_by(|a, b| {
                self.price(&((*a).clone(), CapacityType::Spot))
                    .total_cmp(&self.price(&((*b).clone(), CapacityType::Spot)))
            })
            .cloned()
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
            ::metrics::counter!("rio_scheduler_sla_hw_cost_fallback_total", "reason" => "stale")
                .increment(1);
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

    /// Stable hash of the **solve-relevant projection**: every field
    /// [`super::solve::solve_full`] reads through [`Self::price`] /
    /// [`Self::lambda_for`] / [`Self::smallest_fitting`] /
    /// [`Self::cheapest_h`]. Feeds [`super::solve::SolveInputs::inputs_gen`]
    /// (the derived `inputs_gen`). Includes `stale_clamp` — a clamp
    /// flip changes `price()` from EMA→seed without ANY caller action.
    ///
    /// `price`/`node_count` hash **`.value` only** — NOT the whole
    /// [`PriceEma`], whose `updated_at` is a timestamp, not a solve
    /// input; hashing it would re-roll ε_h on every persist. `lambda`
    /// hashes `(numerator, denominator)` (what [`lambda_hat`] reads).
    /// All `f64` via `.to_bits()` since `f64: !Hash`; safe because the
    /// EMA fold is deterministic — same inputs → bit-identical, no NaN.
    /// Sorted by key so iteration order is irrelevant.
    pub fn solve_relevant_hash(&self) -> u64 {
        use std::hash::{DefaultHasher, Hash, Hasher};
        let mut h = DefaultHasher::new();
        self.stale_clamp.hash(&mut h);
        let mut price: Vec<_> = self.price.iter().collect();
        price.sort_by_key(|(k, _)| cell_label(k));
        for (k, v) in price {
            cell_label(k).hash(&mut h);
            v.value.to_bits().hash(&mut h);
        }
        let mut lambda: Vec<_> = self.lambda.iter().collect();
        lambda.sort_by_key(|(k, _)| (*k).clone());
        for (k, v) in lambda {
            k.hash(&mut h);
            v.numerator.to_bits().hash(&mut h);
            v.denominator.to_bits().hash(&mut h);
        }
        let mut nc: Vec<_> = self.node_count.iter().collect();
        nc.sort_by_key(|(k, _)| (*k).clone());
        for (k, v) in nc {
            k.hash(&mut h);
            v.value.to_bits().hash(&mut h);
        }
        let mut cells: Vec<_> = self.cells.iter().collect();
        cells.sort_by_key(|(k, _)| cell_label(k));
        for (k, menu) in cells {
            cell_label(k).hash(&mut h);
            for t in menu {
                t.cores.hash(&mut h);
                t.mem_bytes.hash(&mut h);
                t.price_per_vcpu_hr.to_bits().hash(&mut h);
            }
        }
        h.finish()
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
                && let Some(cell) = parse_cell(rest)
            {
                t.price.insert(
                    cell,
                    PriceEma {
                        value,
                        updated_at: at,
                    },
                );
            } else if let Some(h) = key.strip_prefix("lambda:") {
                t.lambda.insert(
                    h.to_owned(),
                    RatioEma {
                        numerator: num.unwrap_or(0.0),
                        denominator: den.unwrap_or(0.0),
                        updated_at: at,
                    },
                );
            } else if let Some(h) = key.strip_prefix("node_count:") {
                t.node_count.insert(
                    h.to_owned(),
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
    /// `(cluster, key)`; small (≤ 2·|H| + 2·|H| rows), so no batching.
    pub async fn persist(&self, db: &SchedulerDb) -> anyhow::Result<()> {
        for (cell, p) in &self.price {
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
            .bind(format!("price:{}", cell_label(cell)))
            .bind(p.value)
            .bind(p.updated_at)
            .execute(db.pool())
            .await?;
        }
        for (h, ema) in &self.lambda {
            sqlx::query(
                "INSERT INTO sla_ema_state (cluster, key, value, numerator, denominator, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, to_timestamp($6)) \
                 ON CONFLICT (cluster, key) DO UPDATE SET \
                   value = $3, numerator = $4, denominator = $5, updated_at = to_timestamp($6)",
            )
            .bind(&self.cluster)
            .bind(format!("lambda:{h}"))
            .bind(ema.value_or(LAMBDA_SEED))
            .bind(ema.numerator)
            .bind(ema.denominator)
            .bind(ema.updated_at)
            .execute(db.pool())
            .await?;
        }
        for (h, nc) in &self.node_count {
            sqlx::query(
                "INSERT INTO sla_ema_state (cluster, key, value, updated_at) \
                 VALUES ($1, $2, $3, to_timestamp($4)) \
                 ON CONFLICT (cluster, key) DO UPDATE SET value = $3, updated_at = to_timestamp($4)",
            )
            .bind(&self.cluster)
            .bind(format!("node_count:{h}"))
            .bind(nc.value)
            .bind(nc.updated_at)
            .execute(db.pool())
            .await?;
        }
        Ok(())
    }

    /// Recompute λ\[h\] from `interrupt_samples` rows newer than each
    /// h's `updated_at`. Called from [`interrupt_housekeeping`]
    /// (lease-gated) — controller appends, scheduler aggregates. Keyed
    /// directly on `interrupt_samples.hw_class` (the controller-stamped
    /// node label). λ is a solve input; the next poll's
    /// [`super::solve::SolveInputs::inputs_gen`] reflects the change.
    pub async fn refresh_lambda(&mut self, db: &SchedulerDb) -> anyhow::Result<()> {
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
        let prev_hwm = self
            .lambda
            .values()
            .map(|e| e.updated_at)
            .fold(0.0, f64::max);
        let mut hwm = prev_hwm;
        let mut per_h: HashMap<HwClassName, (f64, f64)> = HashMap::new();
        for (hw_class, kind, sum, max_at) in rows {
            hwm = hwm.max(max_at);
            let e = per_h.entry(hw_class).or_default();
            match kind.as_str() {
                "interrupt" => e.0 += sum,
                "exposure" => e.1 += sum,
                _ => {}
            }
        }
        // Per-h node_count = Σ exposure_secs / Δt over the batch window
        // (each `kind='exposure'` row is "node-seconds accrued since
        // last flush", so the sum ÷ wall-window is mean live nodes).
        // Skip when `prev_hwm == 0` — first refresh has no window
        // baseline, and a `Δt` from epoch would zero the count.
        let dt = hwm - prev_hwm;
        for (h, (n, d)) in per_h {
            self.lambda
                .entry(h.clone())
                .or_default()
                .update(n, d, hwm, LAMBDA_HALFLIFE_SECS);
            if prev_hwm > 0.0 && dt > 0.0 {
                self.node_count
                    .entry(h)
                    .or_default()
                    .update(d / dt, hwm, LAMBDA_HALFLIFE_SECS);
            }
        }
        Ok(())
    }

    /// Move `lambda` + `node_count` from `from` into `self`, leaving
    /// `price`/`cells`/`stale_clamp` untouched. Used by
    /// [`interrupt_housekeeping`]'s write-back so a concurrent
    /// [`spot_price_poller`] price update isn't clobbered by a full
    /// snapshot swap.
    pub(crate) fn absorb_lambda(&mut self, from: Self) {
        self.lambda = from.lambda;
        self.node_count = from.node_count;
    }

    /// Fold one round of spot-price observations into the price EMA.
    /// `obs` is `$/vCPU·hr` keyed by [`Cell`] — already vCPU-normalized
    /// by the caller. Per-key `dt`: a key absent from `obs` keeps its
    /// OWN `updated_at`, so when it next appears its decay reflects the
    /// full elapsed interval, not just the gap since the last (partial)
    /// fold.
    pub fn fold_prices(&mut self, obs: &HashMap<Cell, f64>, now: f64) {
        for (k, &v) in obs {
            self.price
                .entry(k.clone())
                .or_default()
                .update(v, now, SPOT_HALFLIFE_SECS);
        }
    }

    /// Test constructor.
    #[cfg(test)]
    pub fn from_parts(price: HashMap<Cell, f64>, lambda: HashMap<HwClassName, RatioEma>) -> Self {
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
            ..Self::default()
        }
    }

    /// Test setter: insert a price with an explicit `updated_at`.
    #[cfg(test)]
    pub fn set_price(&mut self, h: &str, cap: CapacityType, value: f64, updated_at: f64) {
        self.price
            .insert((h.to_owned(), cap), PriceEma { value, updated_at });
    }

    /// Test setter: per-h node-count EMA.
    #[cfg(test)]
    pub fn set_node_count(&mut self, h: &str, value: f64, updated_at: f64) {
        self.node_count
            .insert(h.to_owned(), PriceEma { value, updated_at });
    }

    /// Test setter: per-cell instance-type menu (sorted by `cores`).
    #[cfg(test)]
    pub fn set_menu(&mut self, cell: Cell, mut menu: Vec<InstanceType>) {
        menu.sort_by_key(|t| t.cores);
        self.cells.insert(cell, menu);
    }
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Per-cell exponential backoff state. `until` is the masked-until
/// boundary; `step` doubles `60s → 120s → … ≤ max_lead_time` per
/// consecutive [`IceBackoff::mark`], reset on [`IceBackoff::clear`].
#[derive(Debug, Clone, Copy)]
struct IceState {
    until: Instant,
    step: u32,
}

/// In-process insufficient-capacity mask. A [`Cell`] reported
/// `unfulfillable` by the controller (NodeClaim `Launched=False` or
/// `Registered` timeout — ADR-023 §Capacity backoff) is masked
/// fleet-wide with exponential backoff `60s → 120s → …` capped at
/// `max_lead_time`, reset on first success.
///
/// The mask is **read-time** (`r[sched.sla.hw-class.ice-mask]`): the
/// per-key solve memo holds the full-H `(c*, A)` and is never
/// overwritten; each dispatch computes `A \ masked_cells()`. Unmasking
/// is therefore free (no resolve), and ICE state is NOT in
/// `inputs_gen`.
///
/// In-memory, lease-holder only — a scheduler lease handoff costs at
/// most one wasted NodeClaim round per masked cell.
#[derive(Debug)]
pub struct IceBackoff {
    cells: DashMap<Cell, IceState>,
    max_lead_time: Duration,
}

impl Default for IceBackoff {
    fn default() -> Self {
        Self::new(super::config::default_max_lead_time())
    }
}

impl IceBackoff {
    pub fn new(max_lead_time_secs: f64) -> Self {
        Self {
            cells: DashMap::new(),
            max_lead_time: Duration::from_secs_f64(max_lead_time_secs.max(1.0)),
        }
    }

    /// Mark `cell` infeasible. TTL is `min(60s · 2^step, max_lead_time)`;
    /// `step` increments per consecutive mark and resets via
    /// [`Self::clear`].
    pub fn mark(&self, cell: &Cell) {
        // Match-on-map then insert: see `is_masked` for the
        // DashMap-guard-reentrance hazard.
        let step = self
            .cells
            .get(cell)
            .map(|s| s.step.saturating_add(1))
            .unwrap_or(0);
        let ttl = (ICE_BASE_TTL * 2u32.saturating_pow(step)).min(self.max_lead_time);
        self.cells.insert(
            cell.clone(),
            IceState {
                until: Instant::now() + ttl,
                step,
            },
        );
    }

    /// Reset `cell`'s backoff (first success after a mark). Called via
    /// `AckSpawnedIntents.registered_cells` (controller's NodeClaim
    /// `Registered=True` edge — §13b) or on first heartbeat for a pod
    /// spawned on `cell` (§13a interim path; heartbeat ⇒ pod scheduled
    /// ⇒ node existed). NEVER from `spawned` (Pending ack) — that's
    /// the wrong edge and defeats backoff doubling.
    pub fn clear(&self, cell: &Cell) {
        self.cells.remove(cell);
    }

    /// Current backoff step for `cell` (number of consecutive marks
    /// since the last clear), or `None` if never marked / cleared. For
    /// tests and the §13a contract assertion.
    pub fn step(&self, cell: &Cell) -> Option<u32> {
        self.cells.get(cell).map(|s| s.step)
    }

    /// Whether `cell` is currently masked. Expired entries are NOT
    /// reaped — the `step` must survive expiry so a re-mark doubles
    /// (only `clear` on success resets).
    pub fn is_masked(&self, cell: &Cell) -> bool {
        // `.map(|r| r.until)` copies the `Instant` and drops the `Ref`
        // guard BEFORE comparison. `get()` then `remove()` while the
        // guard is live deadlocks (DashMap shard RwLock is non-
        // reentrant) — this fired the first time any cell crossed TTL
        // and froze the single-threaded actor.
        self.cells
            .get(cell)
            .map(|r| r.until)
            .is_some_and(|u| u > Instant::now())
    }

    /// Snapshot of currently-masked cells for the read-time `A \ masked`
    /// step. O(|ever-marked cells|) — bounded by `|H| × 2`.
    pub fn masked_cells(&self) -> HashSet<Cell> {
        let now = Instant::now();
        self.cells
            .iter()
            .filter(|e| e.value().until > now)
            .map(|e| e.key().clone())
            .collect()
    }

    /// Max ladder steps before `_hw_ladder_exhausted_total{exit="step"}`
    /// — `min(⌈max(tier_bound, ladder_budget) / lead_time / 4⌉, 8)`.
    /// ADR-023 §Capacity backoff exit (a): caps capacity-retry latency
    /// at ~¼ of the tier's wall-clock budget.
    pub fn ladder_cap(
        max_tier_bound_secs: f64,
        ladder_budget_secs: f64,
        lead_time_secs: f64,
    ) -> u32 {
        ((max_tier_bound_secs.max(ladder_budget_secs) / lead_time_secs.max(1.0) / 4.0).ceil()
            as u32)
            .clamp(1, 8)
    }

    /// Count of currently-masked entries. For tests and debugging.
    pub fn live(&self) -> usize {
        let now = Instant::now();
        self.cells.iter().filter(|e| e.value().until > now).count()
    }

    /// All `|H| × 2` cells currently masked → §Capacity backoff exit
    /// (b). Caller emits `infeasible_total{reason=capacity_exhausted}`.
    pub fn exhausted<'a>(&self, hw_classes: impl IntoIterator<Item = &'a HwClassName>) -> bool {
        let mut any = false;
        for h in hw_classes {
            any = true;
            for cap in CapacityType::ALL {
                if !self.is_masked(&(h.clone(), cap)) {
                    return false;
                }
            }
        }
        any
    }
}

/// Lease-gated spot-price poller: every 10min, the leader pulls
/// `DescribeSpotPriceHistory` for each band's representative instance
/// type, EMA-smooths into `cost`, re-evaluates the stale-clamp, exports
/// the staleness gauge, and bumps `inputs_gen`.
///
/// Spot-only — `main.rs` spawns this only under `hw_cost_source =
/// Some(Spot)`. λ refresh / sweep / persist live in
/// [`interrupt_housekeeping`] (which runs unconditionally). Standby
/// replicas emit the staleness gauge (per-replica, observability.md
/// says it "climbs when … this replica is standby") but skip the AWS
/// body. On a false→true leader edge the in-mem table is reloaded from
/// PG (see `poller_tick_prelude`).
pub async fn spot_price_poller(
    db: SchedulerDb,
    leader: LeaderState,
    cost: std::sync::Arc<parking_lot::RwLock<CostTable>>,
    shutdown: rio_common::signal::Token,
) {
    // EC2 client built once. Same `from_env()` chain as
    // `rio_common::s3::default_client` — IRSA in-cluster, profile/env
    // locally. The caller already gated on `hw_cost_source == Spot`, so
    // no `Option` dance.
    let ec2 = aws_sdk_ec2::Client::new(&aws_config::from_env().load().await);
    let mut tick = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut was_leader = false;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {},
        }
        if !poller_tick_prelude(
            &mut was_leader,
            leader.is_leader(),
            &cost,
            &db,
            now_epoch(),
            Some(HwCostSource::Spot),
        )
        .await
        {
            continue;
        }
        // parking_lot guards aren't Send → clone the menu out, await the
        // AWS call, then mutate under a brief sync lock. `fold_spot_poll`
        // touches only `price`; `interrupt_housekeeping` writes only
        // `lambda`/`node_count`, so no swap-race.
        let cells = cost.read().cells.clone();
        let result = poll_spot_once(&ec2, &cells).await;
        let now = now_epoch();
        {
            let mut g = cost.write();
            fold_spot_poll(&mut g, result, now);
            g.apply_stale_clamp(now);
        }
        ::metrics::gauge!("rio_scheduler_sla_hw_cost_stale_seconds")
            .set(now - cost.read().price_updated_at());
        // r[impl sched.sla.hw-class.epsilon-explore+2]
        // Price is a solve input — the next poll's derived
        // `SolveInputs::inputs_gen` reflects the new table.
    }
}

/// Lease-gated λ/persist housekeeping: every 10min, the leader
/// refreshes λ from `interrupt_samples`, sweeps the retention window,
/// and persists the full `CostTable` to PG. Runs unconditionally
/// (independent of `hw_cost_source`) —
/// the controller appends `interrupt_samples` regardless, and the
/// EMA-state persist covers both λ and any spot-price updates from
/// [`spot_price_poller`]. On a false→true leader edge the in-mem table
/// is reloaded from PG so the next leader picks up where the last left
/// off — without this, the standby's startup snapshot (loaded once at
/// main.rs) would be `persist()`ed on the first leader tick,
/// overwriting the previous leader's evolved EMA.
pub async fn interrupt_housekeeping(
    db: SchedulerDb,
    leader: LeaderState,
    cost: std::sync::Arc<parking_lot::RwLock<CostTable>>,
    shutdown: rio_common::signal::Token,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(POLL_INTERVAL_SECS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut was_leader = false;
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {},
        }
        // `source=None`: leader-gate + edge-reload only; no staleness
        // gauge / clamp (Spot-only — meaningless without a live source).
        if !poller_tick_prelude(
            &mut was_leader,
            leader.is_leader(),
            &cost,
            &db,
            now_epoch(),
            None,
        )
        .await
        {
            continue;
        }
        // Snapshot → refresh_lambda → write back λ ONLY (don't clobber
        // a concurrent `spot_price_poller` price fold).
        let mut snap = cost.read().clone();
        if let Err(e) = snap.refresh_lambda(&db).await {
            tracing::warn!(error = %e, "λ refresh failed; keeping previous");
        }
        let cluster = snap.cluster.clone();
        cost.write().absorb_lambda(snap);
        if let Err(e) = sweep_interrupt_samples(&db, &cluster).await {
            tracing::warn!(error = %e, "interrupt_samples retention sweep failed");
        }
        // Persist a FRESH snapshot (re-read after absorb so any
        // concurrent price update is included). Bound to a let:
        // parking_lot guards aren't Send across .await.
        let snap = cost.read().clone();
        if let Err(e) = snap.persist(&db).await {
            tracing::warn!(error = %e, "cost-table persist failed");
        }
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

/// Fold one [`poll_spot_once`] result into `snap` and emit the
/// matching `_hw_cost_fallback_total{reason=…}` on the non-success
/// arms. Factored from [`spot_price_poller`] so the `api_error` /
/// `empty_history` reasons are unit-testable without an EC2 client.
pub(crate) fn fold_spot_poll(
    snap: &mut CostTable,
    result: anyhow::Result<HashMap<Cell, f64>>,
    now: f64,
) {
    match result {
        Ok(obs) if !obs.is_empty() => snap.fold_prices(&obs, now),
        Ok(_) => {
            ::metrics::counter!(
                "rio_scheduler_sla_hw_cost_fallback_total",
                "reason" => "empty_history"
            )
            .increment(1);
        }
        Err(e) => {
            tracing::warn!(error = %e, "spot-price poll failed; keeping previous");
            ::metrics::counter!(
                "rio_scheduler_sla_hw_cost_fallback_total",
                "reason" => "api_error"
            )
            .increment(1);
        }
    }
}

/// Per-tick gauge-emit + leader-edge-reload, shared by
/// [`spot_price_poller`] and [`interrupt_housekeeping`]. Returns `true`
/// if the caller should proceed with the tick body.
///
/// - When `source == Some(Spot)`: emits
///   `rio_scheduler_sla_hw_cost_stale_seconds` BEFORE the leader gate
///   (per-replica metric — observability.md says it "climbs when … this
///   replica is standby"; `r[obs.metric.scheduler-leader-gate]` does
///   NOT list it). The gauge and `apply_stale_clamp` are gated on Spot:
///   "stale relative to a source that doesn't exist" reads as 56 years
///   under `Static`/`None` and trips the false-positive
///   `_hw_cost_fallback_total{reason="stale"}`.
/// - On a false→true leader edge, reloads from PG so the new leader
///   resumes from the previous leader's persisted state, not its own
///   startup snapshot.
pub(crate) async fn poller_tick_prelude(
    was_leader: &mut bool,
    is_leader: bool,
    cost: &std::sync::Arc<parking_lot::RwLock<CostTable>>,
    db: &SchedulerDb,
    now: f64,
    source: Option<HwCostSource>,
) -> bool {
    let spot = matches!(source, Some(HwCostSource::Spot));
    if spot {
        ::metrics::gauge!("rio_scheduler_sla_hw_cost_stale_seconds")
            .set(now - cost.read().price_updated_at());
    }
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
                // leadership doesn't trust a dead snapshot. Spot-only
                // (see fn doc).
                if spot {
                    fresh.apply_stale_clamp(now);
                }
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

/// One `DescribeSpotPriceHistory` round. Returns vCPU-normalized
/// `$/vCPU·hr` per `(h, Spot)`.
///
/// Queries the last hour of `Linux/UNIX` spot-price history for the
/// instance types in `cells` (the per-h menu), normalizes each row by
/// its menu `cores`, then takes the per-h median. Median not mean: a
/// single AZ's spot spike for one type shouldn't drag the whole class.
/// On-demand prices stay seed-backed (no public on-demand API without
/// `pricing:GetProducts`). Empty menu → empty result (poller is a
/// no-op until Part-B menu population).
async fn poll_spot_once(
    ec2: &aws_sdk_ec2::Client,
    cells: &HashMap<Cell, Vec<InstanceType>>,
) -> anyhow::Result<HashMap<Cell, f64>> {
    use aws_sdk_ec2::types::InstanceType as Ec2InstanceType;

    // instance-type → (h, vCPU). Spot only — on-demand is seed-backed.
    let h_of: HashMap<String, (HwClassName, f64)> = cells
        .iter()
        .filter(|((_, c), _)| *c == CapacityType::Spot)
        .flat_map(|((h, _), m)| {
            m.iter()
                .map(|it| (it.name.clone(), (h.clone(), f64::from(it.cores))))
        })
        .collect();
    if h_of.is_empty() {
        return Ok(HashMap::new());
    }

    // Spot history, last hour, all configured types, paginated. AWS
    // returns one row per (type, AZ, price-change); a quiet hour can
    // be empty for some types — those just drop out of the median.
    let start = aws_sdk_ec2::primitives::DateTime::from_secs((now_epoch() - 3600.0) as i64);
    let mut per_h: HashMap<HwClassName, Vec<f64>> = HashMap::new();
    let mut pages = ec2
        .describe_spot_price_history()
        .set_instance_types(Some(
            h_of.keys()
                .map(|t| Ec2InstanceType::from(t.as_str()))
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
            let Some((h, vcpu)) = h_of.get(t).filter(|(_, v)| *v > 0.0) else {
                continue;
            };
            let Some(price) = row.spot_price().and_then(|p| p.parse::<f64>().ok()) else {
                ::metrics::counter!(
                    "rio_scheduler_sla_hw_cost_fallback_total",
                    "reason" => "parse"
                )
                .increment(1);
                continue;
            };
            per_h.entry(h.clone()).or_default().push(price / vcpu);
        }
    }

    Ok(per_h
        .into_iter()
        .filter_map(|(h, mut xs)| {
            if xs.is_empty() {
                return None;
            }
            xs.sort_by(|a, b| a.total_cmp(b));
            Some(((h, CapacityType::Spot), xs[xs.len() / 2]))
        })
        .collect())
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
        assert!((CostTable::default().lambda_for("h") - LAMBDA_SEED).abs() < 1e-12);
    }

    #[test]
    fn lambda_for_uses_node_count_scaler() {
        // 100-node fleet: n_λ = 86400·100 = 8.64e6. Prior swamps a
        // single interrupt over 1h — λ̂ ≈ seed (within 0.05%).
        let mut t = CostTable::default();
        t.lambda.insert(
            "h".into(),
            RatioEma {
                numerator: 1.0,
                denominator: 3600.0,
                updated_at: 1000.0,
            },
        );
        t.set_node_count("h", 100.0, 1000.0);
        let l = t.lambda_for("h");
        assert!((l - LAMBDA_SEED).abs() / LAMBDA_SEED < 1e-3, "{l}");
    }

    fn it(name: &str, cores: u32, mem_gib: u64, p: f64) -> InstanceType {
        InstanceType {
            name: name.into(),
            cores,
            mem_bytes: mem_gib << 30,
            price_per_vcpu_hr: p,
        }
    }

    #[test]
    fn smallest_fitting_is_first_menu_match() {
        let mut t = CostTable::default();
        let cell = ("h".into(), CapacityType::Spot);
        t.set_menu(
            cell.clone(),
            vec![
                it("c.large", 2, 4, 0.04),
                it("c.xlarge", 4, 8, 0.04),
                it("c.4xlarge", 16, 32, 0.038),
            ],
        );
        // (c=3, mem=6Gi) → first fit is xlarge.
        assert_eq!(t.smallest_fitting(&cell, 3, 6 << 30), Some(0.04));
        // (c=12) → 4xlarge.
        assert_eq!(t.smallest_fitting(&cell, 12, 0), Some(0.038));
        // (c=32) → no fit (menu non-empty).
        assert_eq!(t.smallest_fitting(&cell, 32, 0), None);
        // Empty menu cell → degrades to EMA price (seed here).
        let other = ("h2".into(), CapacityType::Spot);
        assert_eq!(
            t.smallest_fitting(&other, 32, 0),
            Some(seed_price(CapacityType::Spot))
        );
    }

    /// `poll_spot_once`: per-h median of `price/vCPU` over the returned
    /// history, with vCPU read from the menu (not a separate EC2 call).
    /// The 0.10 outlier for `intel-7` is the median's mid-value, not
    /// the mean.
    #[tokio::test]
    async fn poll_spot_once_median_per_h() {
        use aws_sdk_ec2::types::SpotPrice;
        use aws_smithy_mocks::{RuleMode, mock, mock_client};
        type Ec2 = aws_sdk_ec2::Client;

        let sp = |name: &str, price: &str| {
            SpotPrice::builder()
                .instance_type(name.into())
                .spot_price(price)
                .build()
        };
        let history = mock!(Ec2::describe_spot_price_history).then_output(move || {
            aws_sdk_ec2::operation::describe_spot_price_history::DescribeSpotPriceHistoryOutput::builder()
                // intel-8: one sample → 0.04/2 = 0.02.
                .spot_price_history(sp("c8g.large", "0.0400"))
                // intel-7: three samples → median of [0.03/2, 0.05/2,
                // 0.40/4] = median of [0.015, 0.025, 0.10] = 0.025.
                .spot_price_history(sp("c7a.large", "0.0300"))
                .spot_price_history(sp("c7a.large", "0.0500"))
                .spot_price_history(sp("m7a.large", "0.4000"))
                // Unparseable price + unknown type: dropped.
                .spot_price_history(sp("c7a.large", "n/a"))
                .spot_price_history(sp("c5.large", "0.0100"))
                .build()
        });
        let client = mock_client!(aws_sdk_ec2, RuleMode::MatchAny, &[&history]);

        let mut cells: HashMap<Cell, Vec<InstanceType>> = HashMap::new();
        cells.insert(
            ("intel-8".into(), CapacityType::Spot),
            vec![it("c8g.large", 2, 4, 0.0)],
        );
        cells.insert(
            ("intel-7".into(), CapacityType::Spot),
            vec![it("c7a.large", 2, 4, 0.0), it("m7a.large", 4, 16, 0.0)],
        );
        cells.insert(
            ("intel-6".into(), CapacityType::Spot),
            vec![it("c6a.large", 2, 4, 0.0)],
        );

        let obs = poll_spot_once(&client, &cells).await.unwrap();
        assert_eq!(obs.len(), 2, "intel-6 had no rows → absent");
        assert!((obs[&("intel-8".into(), CapacityType::Spot)] - 0.02).abs() < 1e-9);
        assert!((obs[&("intel-7".into(), CapacityType::Spot)] - 0.025).abs() < 1e-9);
        // Empty menu → no-op.
        assert!(
            poll_spot_once(&client, &HashMap::new())
                .await
                .unwrap()
                .is_empty()
        );
    }

    /// `persist`/`load`/`refresh_lambda` are cluster-scoped: two
    /// schedulers writing to the same global DB with different
    /// `cluster` keys don't read each other's EMAs or interrupt rows.
    /// ADR-023 §2.13 regression — pre-043 the `key` PK collided.
    #[tokio::test]
    async fn persist_load_cluster_scoped() {
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());
        let cell = ("intel-8".into(), CapacityType::Spot);

        // Cluster A writes price=0.5 for (intel-8, Spot) and an
        // interrupt row.
        let mut a = CostTable::seeded("us-east-1");
        a.set_price("intel-8", CapacityType::Spot, 0.5, 1000.0);
        a.persist(&sdb).await.unwrap();
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value) \
             VALUES ('us-east-1', 'intel-8', 'interrupt', 5), \
                    ('us-east-1', 'intel-8', 'exposure', 100)",
        )
        .execute(&db.pool)
        .await
        .unwrap();

        // Cluster B loads → sees seeds (NOT A's 0.5), and refresh_lambda
        // sees no rows.
        let mut b = CostTable::load(&sdb, "eu-west-2").await.unwrap();
        assert!((b.price(&cell) - 0.5).abs() > 1e-3, "B leaked A's price");
        b.refresh_lambda(&sdb).await.unwrap();
        assert!(b.lambda.is_empty(), "B leaked A's interrupt rows");

        // Cluster A reload roundtrips its own price.
        let a2 = CostTable::load(&sdb, "us-east-1").await.unwrap();
        assert!((a2.price(&cell) - 0.5).abs() < 1e-9);
        // And sees its own interrupt rows.
        let mut a3 = CostTable::seeded("us-east-1");
        a3.refresh_lambda(&sdb).await.unwrap();
        assert!(a3.lambda.contains_key("intel-8"));

        // B persists then A reloads: A's price unchanged (PK is
        // (cluster, key) — no overwrite).
        b.set_price("intel-8", CapacityType::Spot, 0.01, 2000.0);
        b.persist(&sdb).await.unwrap();
        let a4 = CostTable::load(&sdb, "us-east-1").await.unwrap();
        assert!((a4.price(&cell) - 0.5).abs() < 1e-9);
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
        t.set_price("h", CapacityType::Spot, 0.5, 1000.0);
        t.persist(&sdb).await.unwrap();
        let r = CostTable::load(&sdb, "c").await.unwrap();
        let at = r
            .price
            .get(&("h".into(), CapacityType::Spot))
            .unwrap()
            .updated_at;
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
        let cell = ("intel-7".into(), CapacityType::Spot);
        t.set_price("intel-7", CapacityType::Spot, 0.0123, 7000.0);
        t.lambda.insert(
            "intel-7".into(),
            RatioEma {
                numerator: 3.0,
                denominator: 9000.0,
                updated_at: 7100.0,
            },
        );
        t.set_node_count("intel-7", 12.5, 7100.0);
        t.persist(&sdb).await.unwrap();

        let r = CostTable::load(&sdb, "c").await.unwrap();
        assert!((r.price(&cell) - 0.0123).abs() < 1e-9);
        let l = r.lambda.get("intel-7").unwrap();
        assert!((l.numerator - 3.0).abs() < 1e-9);
        assert!((l.denominator - 9000.0).abs() < 1e-9);
        assert!((l.updated_at - 7100.0).abs() < 1.0);
        let nc = r.node_count.get("intel-7").unwrap();
        assert!((nc.value - 12.5).abs() < 1e-9);
        assert!((nc.updated_at - 7100.0).abs() < 1.0);
        // λ̂ recomputed identically from the round-tripped state.
        assert!((r.lambda_for("intel-7") - t.lambda_for("intel-7")).abs() < 1e-12);
    }

    /// `> 6 × pollInterval` stale → `price()` clamps to the static seed
    /// and `_hw_cost_fallback_total{reason="stale"}` fires. Fresh →
    /// clamp clears and `price()` reads through.
    #[test]
    fn stale_price_clamps_to_seed_and_emits_fallback() {
        let mut t = CostTable::seeded("c");
        let cell = ("h".into(), CapacityType::Spot);
        t.set_price("h", CapacityType::Spot, 0.5, 1000.0);
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _g = metrics::set_default_local_recorder(&rec);

        // Stale: now − updated_at = 7200 > 3600.
        assert!(t.apply_stale_clamp(1000.0 + STALE_CLAMP_AFTER_SECS + 1.0));
        assert!(
            (t.price(&cell) - seed_price(CapacityType::Spot)).abs() < 1e-9,
            "clamped → seed, not 0.5"
        );
        let fired = snap.snapshot().into_vec().iter().any(|(ck, _, _, _)| {
            ck.key().name() == "rio_scheduler_sla_hw_cost_fallback_total"
                && ck.key().labels().any(|l| l.value() == "stale")
        });
        assert!(fired, "fallback_total{{reason=stale}} must increment");

        // Fresh: clamp clears; price() reads through.
        t.set_price("h", CapacityType::Spot, 0.5, 9000.0);
        assert!(!t.apply_stale_clamp(9000.0 + 60.0));
        assert!((t.price(&cell) - 0.5).abs() < 1e-9);
    }

    /// `fold_spot_poll` emits `_hw_cost_fallback_total{reason=…}` on
    /// the two non-success arms and folds prices on success. Wires the
    /// previously-dead `api_error` / `empty_history` label values
    /// (observability.md:164).
    #[test]
    fn fold_spot_poll_emits_fallback_reasons() {
        use metrics_util::debugging::DebugValue;
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _g = metrics::set_default_local_recorder(&rec);
        let mut t = CostTable::seeded("c");
        let cell = ("h".to_owned(), CapacityType::Spot);

        // One call per arm. Ok(non-empty) folds and emits nothing;
        // Err / Ok(empty) emit one reason each.
        fold_spot_poll(&mut t, Err(anyhow::anyhow!("boom")), 1000.0);
        fold_spot_poll(&mut t, Ok(HashMap::new()), 1000.0);
        fold_spot_poll(&mut t, Ok(HashMap::from([(cell.clone(), 0.07)])), 1000.0);
        assert!((t.price(&cell) - 0.07).abs() < 1e-9, "folded into EMA");

        // Snapshot once (Snapshotter::snapshot drains): exactly the two
        // failure reasons fired, once each.
        let counts: HashMap<String, u64> = snap
            .snapshot()
            .into_vec()
            .into_iter()
            .filter_map(|(ck, _, _, v)| {
                let k = ck.key();
                (k.name() == "rio_scheduler_sla_hw_cost_fallback_total").then(|| {
                    let r = k
                        .labels()
                        .find(|l| l.key() == "reason")
                        .map(|l| l.value().to_owned())
                        .unwrap_or_default();
                    let DebugValue::Counter(c) = v else {
                        return (r, 0);
                    };
                    (r, c)
                })
            })
            .collect();
        assert_eq!(counts.get("api_error"), Some(&1));
        assert_eq!(counts.get("empty_history"), Some(&1));
        assert_eq!(counts.len(), 2, "Ok(non-empty) emits no fallback reason");
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
        let nc = t.node_count.get("aws-8-nvme-hi").unwrap().value;
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
        let mut t = CostTable::default();
        let h1: Cell = ("h1".into(), CapacityType::Spot);
        let h2: Cell = ("h2".into(), CapacityType::Spot);
        let mut obs = HashMap::new();
        obs.insert(h1.clone(), 0.02);
        obs.insert(h2.clone(), 0.01);
        t.fold_prices(&obs, 1000.0);
        // t=1600: h2 only (h1 absent).
        let mut obs2 = HashMap::new();
        obs2.insert(h2.clone(), 0.015);
        t.fold_prices(&obs2, 1600.0);
        // h1's updated_at must NOT have moved.
        assert_eq!(t.price[&h1].updated_at, 1000.0);
        // t=2200: h1 reappears. dt=1200 (vs old global-stamp dt=600).
        let mut obs3 = HashMap::new();
        obs3.insert(h1.clone(), 0.03);
        t.fold_prices(&obs3, 2200.0);
        // decay = 0.5^(1200/SPOT_HALFLIFE_SECS); SPOT_HALFLIFE_SECS=3h.
        let decay = 0.5f64.powf(1200.0 / SPOT_HALFLIFE_SECS);
        let want = 0.02 * decay + 0.03 * (1.0 - decay);
        assert!(
            (t.price(&h1) - want).abs() < 1e-9,
            "want {want}, got {}",
            t.price(&h1)
        );
    }

    /// merged_bug_006b: under `hw_cost_source ∈ {Static, None}` there is
    /// no live source, so `_hw_cost_stale_seconds` reads as 56 years
    /// (epoch) and `apply_stale_clamp` trips the false-positive
    /// `_hw_cost_fallback_total{reason="stale"}`. Both are now gated on
    /// `source == Some(Spot)` in `poller_tick_prelude`.
    #[tokio::test]
    async fn stale_metric_silent_under_static() {
        use std::sync::Arc;
        let db = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        let sdb = SchedulerDb::new(db.pool.clone());
        // No price keys → price_updated_at()=0 → "56 years stale".
        let cost = Arc::new(parking_lot::RwLock::new(CostTable::seeded("c")));

        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = rec.snapshotter();
        let mut was_leader = false;
        for source in [None, Some(HwCostSource::Static)] {
            let _g = metrics::set_default_local_recorder(&rec);
            // Standby + leader-edge ticks: neither emits gauge / counter.
            poller_tick_prelude(&mut was_leader, false, &cost, &sdb, 6100.0, source).await;
            poller_tick_prelude(&mut was_leader, true, &cost, &sdb, 6100.0, source).await;
        }
        let metrics: Vec<_> = snapshotter
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(ck, _, _, _)| ck.key().name().to_owned())
            .collect();
        assert!(
            !metrics
                .iter()
                .any(|n| n == "rio_scheduler_sla_hw_cost_stale_seconds"),
            "gauge unset under non-Spot source: {metrics:?}"
        );
        assert!(
            !metrics
                .iter()
                .any(|n| n == "rio_scheduler_sla_hw_cost_fallback_total"),
            "stale-clamp counter zero under non-Spot source: {metrics:?}"
        );
        // stale_clamp not latched (would be `true` after 6100s if the
        // leader-edge `apply_stale_clamp` ran un-gated).
        assert!(
            (cost.read().price(&("h".into(), CapacityType::Spot)) - seed_price(CapacityType::Spot))
                .abs()
                < 1e-9,
            "non-Spot leader-edge does not engage stale_clamp"
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

        let cell = ("h".into(), CapacityType::Spot);
        // Seed PG with the previous leader's evolved state.
        let mut prev = CostTable::seeded("c");
        prev.set_price("h", CapacityType::Spot, 0.08, 5000.0);
        prev.persist(&sdb).await.unwrap();

        // This replica's stale in-mem startup snapshot.
        let mut mine = CostTable::seeded("c");
        mine.set_price("h", CapacityType::Spot, 0.02, 100.0);
        let cost = Arc::new(parking_lot::RwLock::new(mine));

        // (a) standby: emits gauge, returns false. Captured via local
        // recorder so parallel tests can't interfere.
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snapshotter = rec.snapshotter();
        let mut was_leader = false;
        let spot = Some(HwCostSource::Spot);
        let proceed = {
            let _g = metrics::set_default_local_recorder(&rec);
            poller_tick_prelude(&mut was_leader, false, &cost, &sdb, 6100.0, spot).await
        };
        assert!(!proceed);
        let saw_gauge = snapshotter
            .snapshot()
            .into_vec()
            .iter()
            .any(|(ck, _, _, _)| ck.key().name() == "rio_scheduler_sla_hw_cost_stale_seconds");
        assert!(saw_gauge, "standby must emit the staleness gauge");
        // Standby did NOT reload (still 0.02).
        assert!((cost.read().price(&cell) - 0.02).abs() < 1e-9);

        // (b) false→true edge: reloads from PG, returns true.
        let proceed = poller_tick_prelude(&mut was_leader, true, &cost, &sdb, 6100.0, spot).await;
        assert!(proceed);
        assert!(was_leader);
        assert!(
            (cost.read().price(&cell) - 0.08).abs() < 1e-9,
            "leader-edge must reload PG state, not keep startup snapshot"
        );

        // Subsequent leader tick: no reload (would clobber in-flight
        // mutation if it did).
        cost.write()
            .set_price("h", CapacityType::Spot, 0.09, 6000.0);
        let proceed = poller_tick_prelude(&mut was_leader, true, &cost, &sdb, 6100.0, spot).await;
        assert!(proceed);
        assert!((cost.read().price(&cell) - 0.09).abs() < 1e-9);
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

        let cell = ("h".into(), CapacityType::Spot);
        // Seed PG with the previous leader's evolved state.
        let mut prev = CostTable::seeded("c");
        prev.set_price("h", CapacityType::Spot, 0.08, 5000.0);
        prev.persist(&sdb).await.unwrap();

        // This replica's stale in-mem startup snapshot.
        let mut mine = CostTable::seeded("c");
        mine.set_price("h", CapacityType::Spot, 0.02, 100.0);
        let cost = Arc::new(parking_lot::RwLock::new(mine));

        // Broken DB: a separate pool closed before use → load() Errs.
        // (PgPool is Arc-backed; closing a clone of `db.pool` would
        // also break `sdb`.)
        let bad = rio_test_support::TestDb::new(&crate::MIGRATOR).await;
        bad.pool.close().await;
        let bad_db = SchedulerDb::new(bad.pool.clone());

        let mut was_leader = false;
        let spot = Some(HwCostSource::Spot);
        let proceed =
            poller_tick_prelude(&mut was_leader, true, &cost, &bad_db, 6100.0, spot).await;
        assert!(
            !proceed,
            "load() Err → tick body skipped (no persist of stale snapshot)"
        );
        assert!(
            !was_leader,
            "load() Err → was_leader stays false so next tick retries"
        );
        assert!(
            (cost.read().price(&cell) - 0.02).abs() < 1e-9,
            "in-mem unchanged on Err"
        );

        // Retry with a working DB: reload succeeds, latches, proceeds.
        let proceed = poller_tick_prelude(&mut was_leader, true, &cost, &sdb, 6100.0, spot).await;
        assert!(proceed, "retry with working DB → proceed");
        assert!(was_leader, "retry success → latched");
        assert!(
            (cost.read().price(&cell) - 0.08).abs() < 1e-9,
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
        let hwm = t.lambda["aws-8-nvme-hi"].updated_at;
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
        assert_eq!(t.lambda["aws-8-nvme-hi"].updated_at, 1500.0);
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
        // Seeds: spot < on-demand for any unknown h.
        let h = "any".to_string();
        assert!(t.price(&(h.clone(), CapacityType::Spot)) < t.price(&(h, CapacityType::Od)));
    }

    // r[verify sched.sla.hw-class.ice-mask]
    #[test]
    fn ice_mark_exponential_then_clear_resets() {
        let ice = IceBackoff::new(600.0);
        let cell: Cell = ("h".into(), CapacityType::Spot);
        assert!(!ice.is_masked(&cell));
        ice.mark(&cell);
        assert!(ice.is_masked(&cell));
        assert!(!ice.is_masked(&("h".into(), CapacityType::Od)));
        assert_eq!(ice.live(), 1);
        // Backoff doubles per consecutive mark, capped at max_lead_time.
        // step=0 → 60s. After another mark: step=1 → 120s.
        let u0 = ice.cells.get(&cell).unwrap().until;
        ice.mark(&cell);
        let s1 = *ice.cells.get(&cell).unwrap();
        assert_eq!(s1.step, 1);
        assert!(
            s1.until > u0 + Duration::from_secs(50),
            "step=1 TTL ~2× step=0: {:?}",
            s1.until.duration_since(u0)
        );
        // step=10 would be 60·1024=61440s; clamped to 600s.
        for _ in 0..10 {
            ice.mark(&cell);
        }
        let until = ice.cells.get(&cell).unwrap().until;
        assert!(
            until <= Instant::now() + Duration::from_secs(601),
            "TTL capped at max_lead_time"
        );
        // clear() resets — next mark is step=0 again.
        ice.clear(&cell);
        assert!(!ice.is_masked(&cell));
        ice.mark(&cell);
        assert_eq!(ice.cells.get(&cell).unwrap().step, 0);
    }

    /// Regression: `mark()` reads the prior step then inserts; holding
    /// the `Ref` guard across the insert deadlocks (DashMap shard
    /// RwLock is non-reentrant). If reintroduced, this hangs and
    /// nextest's per-test timeout catches it.
    #[test]
    fn ice_mark_re_mark_no_deadlock() {
        let ice = IceBackoff::default();
        let cell: Cell = ("h".into(), CapacityType::Spot);
        for _ in 0..5 {
            ice.mark(&cell);
        }
        assert_eq!(ice.cells.get(&cell).unwrap().step, 4);
    }

    #[test]
    fn ice_masked_cells_and_exhausted() {
        let ice = IceBackoff::default();
        let hs: Vec<HwClassName> = vec!["h1".into(), "h2".into()];
        assert!(!ice.exhausted(&hs), "no cells masked");
        for h in &hs {
            for c in CapacityType::ALL {
                ice.mark(&(h.clone(), c));
            }
        }
        assert_eq!(ice.masked_cells().len(), 4);
        assert!(ice.exhausted(&hs));
        ice.clear(&("h1".into(), CapacityType::Od));
        assert!(!ice.exhausted(&hs), "one cell clear → not exhausted");
        // Empty H → not exhausted (vacuously not "all of H masked").
        assert!(!ice.exhausted(std::iter::empty::<&HwClassName>()));
    }

    #[test]
    fn ladder_cap_bounds() {
        // 1h tier, budget 600s, lead 120s → ceil(3600/120/4)=8 → clamp 8.
        assert_eq!(IceBackoff::ladder_cap(3600.0, 600.0, 120.0), 8);
        // 5min tier, budget 300s, lead 120s → ceil(300/120/4)=1.
        assert_eq!(IceBackoff::ladder_cap(300.0, 300.0, 120.0), 1);
        // tier < budget → budget binds: max(60, 600)/45/4 = 3.33 → 4.
        assert_eq!(IceBackoff::ladder_cap(60.0, 600.0, 45.0), 4);
        // Huge tier → still 8.
        assert_eq!(IceBackoff::ladder_cap(86400.0, 600.0, 120.0), 8);
    }
}
