//! ADR-023 phase-13 hw-band + capacity-type cost model.
//!
//! Two halves, both feeding [`super::solve::solve_full`]:
//!
//! - [`CostTable`]: per-`(Band, Cap)` `$/vCPU·hr` snapshot. Populated by
//!   [`spot_price_poller`] (lease-gated, 10min tick, 3h-halflife EMA over
//!   `DescribeSpotPriceHistory`) and persisted to `sla_ema_state` so a
//!   restart doesn't re-warm. `expected_cost` turns a candidate
//!   `(band, cap, c*, T(c*))` into a comparable scalar for the softmax.
//! - λ[h]: per-hw-band Poisson interrupt rate. Computed from
//!   `interrupt_samples` (controller-appended) as
//!   `EMA(Σinterrupts) / EMA(Σnode-seconds)` with 24h halflife;
//!   decays toward [`LAMBDA_SEED`] when exposure dries up.
//!
//! [`IceBackoff`] is the in-process insufficient-capacity ladder: a
//! `(band, cap)` that left a pod Pending past `hw_fallback_after_secs`
//! is marked infeasible fleet-wide for 60s so the next solve excludes
//! it.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use dashmap::DashMap;
use serde::{Deserialize, Serialize};

use crate::db::SchedulerDb;
use crate::lease::LeaderState;

use super::hw::HwTable;
use super::types::{FittedParams, RawCores};

/// Hardware-generation band. Maps to the `rio.build/hw-band` Node label
/// (12-NodePool topology) and the `karpenter.k8s.aws/instance-generation`
/// requirement: `Hi`=gen8, `Mid`=gen7, `Lo`=gen6.
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

    /// `instance-generation` strings this band admits. Feeds
    /// [`HwTable::h_dagger`]'s "is hw_class `h` in band `b`?" check
    /// (hw_class is `"{mfr}-{gen}-{storage}"`).
    pub fn generations(self) -> &'static [&'static str] {
        match self {
            Band::Hi => &["8"],
            Band::Mid => &["7"],
            Band::Lo => &["6"],
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
/// halves are independently EMA-decayed. Used for λ[h] (interrupts ÷
/// node-seconds) so a burst of node churn doesn't spike λ — the
/// denominator absorbs it.
#[derive(Debug, Clone, Copy, Default)]
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

/// Seed λ (interrupts/sec) when no exposure has been observed. ~1/3h —
/// AWS's published spot-interruption frequency floor for the deepest
/// pools is "<5%/hr"; 1/3h is a conservative middle until self-
/// calibration kicks in.
pub const LAMBDA_SEED: f64 = 1.0 / (3.0 * 3600.0);

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

/// λ[h] EMA halflife. 24h: spot interruption rates move on a daily
/// cadence (capacity rebalancing); a 3h halflife would chase noise.
const LAMBDA_HALFLIFE_SECS: f64 = 24.0 * 3600.0;

/// After this many seconds of zero exposure, λ[h] linearly blends back
/// toward [`LAMBDA_SEED`]. ADR-023: a band that hasn't been scheduled
/// in two days shouldn't carry a stale λ from a since-resolved
/// capacity crunch.
const LAMBDA_DECAY_TO_SEED_AFTER_SECS: f64 = 48.0 * 3600.0;

/// ICE-backoff TTL. A `(band, cap)` that left a pod Pending past
/// `hw_fallback_after_secs` is fleet-wide infeasible for this long.
/// Short — capacity recovers in minutes; the ladder re-probes.
const ICE_TTL: Duration = Duration::from_secs(60);

/// Per-`(band, cap)` `$/vCPU·hr` + per-band λ. Cheap to clone (two
/// small maps); the solve takes a snapshot by value.
#[derive(Debug, Clone)]
pub struct CostTable {
    /// EMA-smoothed `$/vCPU·hr`. Missing key → seed.
    price: HashMap<(Band, Cap), f64>,
    /// Per-band interrupt-rate EMA.
    lambda: HashMap<Band, RatioEma>,
    /// Unix-epoch seconds of the last successful price refresh. Feeds
    /// `rio_scheduler_sla_hw_cost_stale_seconds`.
    pub price_updated_at: f64,
}

impl Default for CostTable {
    fn default() -> Self {
        let mut price = HashMap::new();
        for &(b, od) in &ON_DEMAND_SEED {
            price.insert((b, Cap::OnDemand), od);
            price.insert((b, Cap::Spot), od * SPOT_SEED_DISCOUNT);
        }
        Self {
            price,
            lambda: HashMap::new(),
            price_updated_at: 0.0,
        }
    }
}

impl CostTable {
    /// `$/vCPU·hr` for `(band, cap)`. Seed-backed — never `None`.
    pub fn price(&self, band: Band, cap: Cap) -> f64 {
        self.price.get(&(band, cap)).copied().unwrap_or_else(|| {
            let od = ON_DEMAND_SEED
                .iter()
                .find(|(b, _)| *b == band)
                .map(|(_, p)| *p)
                .unwrap_or(0.043);
            match cap {
                Cap::OnDemand => od,
                Cap::Spot => od * SPOT_SEED_DISCOUNT,
            }
        })
    }

    /// Per-band Poisson interrupt rate (events/sec). Decays toward
    /// [`LAMBDA_SEED`] after [`LAMBDA_DECAY_TO_SEED_AFTER_SECS`] of no
    /// exposure: `λ = (1-α)·λ_ema + α·seed` where `α = min(1,
    /// (now - updated_at) / 48h)`.
    pub fn lambda_band(&self, band: Band) -> f64 {
        self.lambda_band_at(band, now_epoch())
    }

    fn lambda_band_at(&self, band: Band, now: f64) -> f64 {
        let Some(ema) = self.lambda.get(&band) else {
            return LAMBDA_SEED;
        };
        let raw = ema.value_or(LAMBDA_SEED);
        let stale = (now - ema.updated_at).max(0.0);
        let alpha = (stale / LAMBDA_DECAY_TO_SEED_AFTER_SECS).min(1.0);
        (1.0 - alpha) * raw + alpha * LAMBDA_SEED
    }

    /// `E[cost]` for a candidate: `price · c* · E[wall] / 3600` where
    /// `E[wall] = (T(c*)/factor) / (1-p)` accounts for geometric
    /// retries under preemption probability `p = 1 - e^{-λT}`.
    /// Memory contributes via the same per-vCPU-hr proxy (memory
    /// scales with cores in the c/m/r families the NodePools admit).
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

    /// Load persisted EMAs from `sla_ema_state`. Called once at
    /// startup so a scheduler restart doesn't re-warm.
    pub async fn load(db: &SchedulerDb) -> anyhow::Result<Self> {
        let mut t = Self::default();
        let rows: Vec<(String, f64, Option<f64>, Option<f64>, f64)> = sqlx::query_as(
            "SELECT key, value, numerator, denominator, \
             EXTRACT(EPOCH FROM updated_at) FROM sla_ema_state",
        )
        .fetch_all(db.pool())
        .await?;
        for (key, value, num, den, at) in rows {
            if let Some(rest) = key.strip_prefix("price:")
                && let Some((b, c)) = parse_band_cap(rest)
            {
                t.price.insert((b, c), value);
                t.price_updated_at = t.price_updated_at.max(at);
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
            }
        }
        Ok(t)
    }

    /// Persist all EMAs to `sla_ema_state` (upsert). One row per key;
    /// small (≤9 rows), so no batching.
    pub async fn persist(&self, db: &SchedulerDb) -> anyhow::Result<()> {
        for (&(b, c), &v) in &self.price {
            sqlx::query(
                "INSERT INTO sla_ema_state (key, value, updated_at) \
                 VALUES ($1, $2, now()) \
                 ON CONFLICT (key) DO UPDATE SET value = $2, updated_at = now()",
            )
            .bind(format!("price:{}:{}", b.label(), c.label()))
            .bind(v)
            .execute(db.pool())
            .await?;
        }
        for (&b, ema) in &self.lambda {
            sqlx::query(
                "INSERT INTO sla_ema_state (key, value, numerator, denominator, updated_at) \
                 VALUES ($1, $2, $3, $4, to_timestamp($5)) \
                 ON CONFLICT (key) DO UPDATE SET \
                   value = $2, numerator = $3, denominator = $4, updated_at = to_timestamp($5)",
            )
            .bind(format!("lambda:{}", b.label()))
            .bind(ema.value_or(LAMBDA_SEED))
            .bind(ema.numerator)
            .bind(ema.denominator)
            .bind(ema.updated_at)
            .execute(db.pool())
            .await?;
        }
        Ok(())
    }

    /// Recompute λ[h] from `interrupt_samples` rows newer than each
    /// band's `updated_at`. Called from the poller tick (lease-gated)
    /// — controller appends, scheduler aggregates.
    pub async fn refresh_lambda(&mut self, db: &SchedulerDb) -> anyhow::Result<()> {
        // Aggregate per-hw_class since the last per-band update; map
        // hw_class → band via the generation segment. Unknown
        // hw_classes (no generation match) are dropped — they
        // contribute to neither numerator nor denominator.
        let rows: Vec<(String, String, f64, f64)> = sqlx::query_as(
            "SELECT hw_class, kind, COALESCE(SUM(value), 0), \
                    EXTRACT(EPOCH FROM MAX(at)) \
             FROM interrupt_samples WHERE at > to_timestamp($1) \
             GROUP BY hw_class, kind",
        )
        .bind(
            self.lambda
                .values()
                .map(|e| e.updated_at)
                .fold(0.0, f64::max),
        )
        .fetch_all(db.pool())
        .await?;
        let now = now_epoch();
        let mut per_band: HashMap<Band, (f64, f64)> = HashMap::new();
        for (hw_class, kind, sum, _) in rows {
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
        for (band, (n, d)) in per_band {
            self.lambda
                .entry(band)
                .or_default()
                .update(n, d, now, LAMBDA_HALFLIFE_SECS);
        }
        Ok(())
    }

    /// Fold one round of spot-price observations into the price EMA.
    /// `obs` is `$/vCPU·hr` keyed by `(band, cap)` — already
    /// vCPU-normalized by the caller.
    pub fn fold_prices(&mut self, obs: &HashMap<(Band, Cap), f64>, now: f64) {
        for (&k, &v) in obs {
            let prev = self.price.get(&k).copied().unwrap_or(v);
            let dt = (now - self.price_updated_at).max(0.0);
            let decay = if self.price_updated_at == 0.0 {
                0.0
            } else {
                0.5f64.powf(dt / SPOT_HALFLIFE_SECS)
            };
            self.price.insert(k, prev * decay + v * (1.0 - decay));
        }
        self.price_updated_at = now;
    }

    /// Test constructor.
    #[cfg(test)]
    pub fn from_parts(price: HashMap<(Band, Cap), f64>, lambda: HashMap<Band, RatioEma>) -> Self {
        Self {
            price,
            lambda,
            price_updated_at: now_epoch(),
        }
    }
}

/// Map `"{mfr}-{gen}-{storage}"` → `Band` via the generation segment.
/// `None` for hw_classes outside gen 6-8 (e.g. metal `unknown` or gen5
/// fallback nodes — the 12-NodePool topology doesn't admit them).
pub fn band_of_hw_class(hw_class: &str) -> Option<Band> {
    let g = hw_class.split('-').nth(1)?;
    Band::ALL.into_iter().find(|b| b.generations().contains(&g))
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

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// In-process insufficient-capacity backoff. A `(band, cap)` that left
/// a pod Pending past `hw_fallback_after_secs` is marked here for
/// [`ICE_TTL`]; [`super::solve::solve_full`] skips marked cells. Shared
/// across all dispatch threads (DashMap).
#[derive(Debug, Default)]
pub struct IceBackoff(DashMap<(Band, Cap), Instant>);

impl IceBackoff {
    /// Mark `(band, cap)` infeasible for [`ICE_TTL`].
    pub fn mark(&self, band: Band, cap: Cap) {
        self.0.insert((band, cap), Instant::now() + ICE_TTL);
    }

    /// Whether `(band, cap)` is currently backed off. Expired entries
    /// are reaped lazily on read.
    pub fn is_infeasible(&self, band: Band, cap: Cap) -> bool {
        match self.0.get(&(band, cap)) {
            Some(until) if *until > Instant::now() => true,
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

    /// Count of currently-live backoff entries. For
    /// `infeasible_total{reason=capacity_exhausted}` and tests.
    pub fn live(&self) -> usize {
        let now = Instant::now();
        self.0.iter().filter(|e| *e.value() > now).count()
    }
}

/// Lease-gated spot-price poller: every 10min, the leader pulls
/// `DescribeSpotPriceHistory` for each band's representative instance
/// type, EMA-smooths into `cost`, refreshes λ from `interrupt_samples`,
/// persists to PG, and exports the staleness gauge.
///
/// `source = None | Some(Static)` → AWS calls skipped; only the λ
/// refresh + gauge run. Standby replicas skip the tick body entirely
/// (price/λ are PG-backed; the next leader picks up where the last
/// left off).
pub async fn spot_price_poller(
    db: SchedulerDb,
    leader: LeaderState,
    cost: std::sync::Arc<parking_lot::RwLock<CostTable>>,
    source: Option<HwCostSource>,
    shutdown: rio_common::signal::Token,
) {
    let mut tick = tokio::time::interval(Duration::from_secs(600));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {},
        }
        if !leader.is_leader() {
            continue;
        }
        // Snapshot → mutate → swap: parking_lot guards aren't Send, so
        // no await while holding one. CostTable is two small maps;
        // clone is cheap.
        let mut snap = cost.read().clone();
        if matches!(source, Some(HwCostSource::Spot)) {
            match poll_spot_once().await {
                Ok(obs) if !obs.is_empty() => snap.fold_prices(&obs, now_epoch()),
                Ok(_) => {}
                Err(e) => tracing::warn!(error = %e, "spot-price poll failed; keeping previous"),
            }
        }
        if let Err(e) = snap.refresh_lambda(&db).await {
            tracing::warn!(error = %e, "λ refresh failed; keeping previous");
        }
        if let Err(e) = snap.persist(&db).await {
            tracing::warn!(error = %e, "cost-table persist failed");
        }
        let stale = now_epoch() - snap.price_updated_at;
        *cost.write() = snap;
        metrics::gauge!("rio_scheduler_sla_hw_cost_stale_seconds").set(stale);
    }
}

/// One `DescribeSpotPriceHistory` round. Returns vCPU-normalized
/// `$/vCPU·hr` per `(band, Cap::Spot)`.
///
// TODO(ADR-023): live aws-sdk-ec2 wiring. The IRSA role + sdk client
// builder mirror `rio_common::s3::default_client`; the call is
// `describe_spot_price_history().instance_types([...]).product_
// descriptions("Linux/UNIX")` filtered to the last hour, then
// `price / vcpu_count` per type → median per band. Stubbed empty so
// `source = Some(Spot)` degrades to seed prices until wired (the
// solve still ranks; only the absolute $ is off).
async fn poll_spot_once() -> anyhow::Result<HashMap<(Band, Cap), f64>> {
    Ok(HashMap::new())
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

    #[test]
    fn lambda_decays_to_seed_after_48h() {
        let mut t = CostTable::default();
        t.lambda.insert(
            Band::Mid,
            RatioEma {
                numerator: 100.0,
                denominator: 100.0, // λ=1.0 — absurdly high
                updated_at: 1000.0,
            },
        );
        // At updated_at: raw λ.
        assert!((t.lambda_band_at(Band::Mid, 1000.0) - 1.0).abs() < 1e-9);
        // 48h later: fully blended to seed.
        let later = 1000.0 + LAMBDA_DECAY_TO_SEED_AFTER_SECS;
        assert!((t.lambda_band_at(Band::Mid, later) - LAMBDA_SEED).abs() < 1e-9);
        // Halfway: midpoint.
        let mid = 1000.0 + LAMBDA_DECAY_TO_SEED_AFTER_SECS / 2.0;
        let v = t.lambda_band_at(Band::Mid, mid);
        assert!(v > LAMBDA_SEED && v < 1.0);
    }

    #[test]
    fn band_of_hw_class_parses_generation() {
        assert_eq!(band_of_hw_class("aws-8-nvme"), Some(Band::Hi));
        assert_eq!(band_of_hw_class("intel-7-ebs"), Some(Band::Mid));
        assert_eq!(band_of_hw_class("amd-6-ebs"), Some(Band::Lo));
        assert_eq!(band_of_hw_class("aws-5-ebs"), None);
        assert_eq!(band_of_hw_class("unknown-unknown-ebs"), None);
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
