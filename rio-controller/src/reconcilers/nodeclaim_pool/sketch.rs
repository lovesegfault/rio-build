//! Per-cell DDSketch state + PG persist (migration 059
//! `nodeclaim_cell_state`).
//!
//! A "cell" is `(hw_class, capacity_type)` — the unit at which lead-time
//! and boot-time distributions are tracked. Each cell carries an
//! active/shadow DDSketch pair for `z = boot − eta_error` (the lead-time
//! the reconciler should provision ahead by) and for raw `boot` (the
//! Karpenter+kubelet overhead floor). Active/shadow rotation gives a
//! sliding window without losing the warm quantile during cold-start.
//!
//! Persisted as version-tagged postcard `bytea` (DDSketch's bucket array
//! is dense-packed integers; ~1KiB binary vs ~8KiB JSON) per
//! `r[ctrl.nodeclaim.lead-time-ddsketch]`. The reconciler loads on
//! construct, persists every tick.

use std::collections::{HashMap, HashSet};
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sketches_ddsketch::{Config, DDSketch};
use tracing::{debug, warn};

use super::consolidate::IdleGapEvent;
use super::ffd::LiveNode;

/// postcard encoding version tag (4 LE bytes prefix). Bump on any
/// `sketches-ddsketch` serde-shape change; [`decode_versioned`] returns
/// `None` on mismatch and the caller falls back to seed.
const SKETCH_VERSION: u32 = 1;

/// Synthetic seed sample count. `r[ctrl.nodeclaim.lead-time-ddsketch]`:
/// `n_seed = 1/(1−q) = 10` at `q=0.9` — enough that
/// `quantile(0.9)` returns the seed value, few enough that real
/// observations dominate quickly.
const N_SEED: usize = 10;

/// `boot_active.count()` threshold below which [`CellState::ice_timeout`]
/// uses `2×seed` instead of `q_0.99(boot)`. A 0.99-quantile on <100
/// samples is noise; the seed floor is the conservative bound.
const ICE_REAL_THRESHOLD: usize = 100;

/// Spot vs on-demand. The migration's CHECK constraint pins the wire
/// strings; [`as_str`](Self::as_str) is the single mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum CapacityType {
    Spot,
    OnDemand,
}

impl CapacityType {
    /// PG/helm wire string. Matches migration 059's
    /// `CHECK (capacity_type IN ('spot','od'))`.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Spot => "spot",
            Self::OnDemand => "od",
        }
    }

    /// Inverse of [`as_str`](Self::as_str).
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "spot" => Some(Self::Spot),
            "od" => Some(Self::OnDemand),
            _ => None,
        }
    }
}

/// `(hw_class, capacity_type)` cell key. `hw_class` is the operator's
/// `[sla.hw_classes.$h]` key (e.g. `"mid-ebs-x86"`). `Ord` for
/// deterministic round-robin iteration in `cover_deficit`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Cell(pub String, pub CapacityType);

impl Cell {
    /// Parse the `"h:cap"` wire form used by helm `sla.leadTimeSeed`
    /// keys and `GetSpawnIntentsResponse.ice_masked_cells`.
    pub fn parse(s: &str) -> Option<Self> {
        let (h, cap) = s.rsplit_once(':')?;
        Some(Self(h.to_string(), CapacityType::parse(cap)?))
    }
}

impl std::fmt::Display for Cell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}:{}", self.0, self.1.as_str())
    }
}

/// Per-cell state. Mirrors migration 059's columns 1:1 (modulo
/// `sketch_epoch` which is a `SystemTime` here, `timestamptz` in PG).
///
/// No `Debug`: `sketches_ddsketch::DDSketch` doesn't derive it.
#[derive(Clone)]
pub struct CellState {
    /// `z = boot − eta_error` active sketch. `lead_time_q`-quantile of
    /// this is what `cover_deficit` provisions ahead by.
    pub z_active: DDSketch,
    /// Previous-epoch `z` sketch (warm fallback during rotation).
    pub z_shadow: DDSketch,
    /// Raw Launch→Registered seconds, active.
    pub boot_active: DDSketch,
    /// Previous-epoch boot sketch.
    pub boot_shadow: DDSketch,
    /// Wall-clock of the last active→shadow rotation. PG round-trips
    /// this so a controller restart doesn't reset the rotation clock.
    pub epoch: SystemTime,
    /// Current `z` quantile (Schmitt-adjusted in B9).
    pub lead_time_q: f64,
    /// EWMA(α=0.2) of the per-cell `on_registered/(on_registered+
    /// on_inflight)` placement split — the warm-hit proxy
    /// [`Self::schmitt_adjust`] reads. In-memory only (NOT a PG
    /// column): restart-reset to 0.9 (= the Schmitt target, mid-zone
    /// `[0.765,0.945]`) so cold-start is a no-op until real
    /// observations move it.
    pub forecast_hit_ewma: f64,
    /// Consolidator's recent idle-gap log (jsonb in PG; capped ring).
    pub idle_gap_events: Vec<IdleGapEvent>,
}

impl Default for CellState {
    fn default() -> Self {
        Self {
            z_active: DDSketch::new(Config::defaults()),
            z_shadow: DDSketch::new(Config::defaults()),
            boot_active: DDSketch::new(Config::defaults()),
            boot_shadow: DDSketch::new(Config::defaults()),
            epoch: SystemTime::now(),
            lead_time_q: 0.9,
            forecast_hit_ewma: 0.9,
            idle_gap_events: Vec::new(),
        }
    }
}

impl CellState {
    /// `lead_time_q`-quantile of the `z` sketch — the provisioning
    /// lead-time. Reads `z_active` first; falls back to `z_shadow` when
    /// active is empty (immediately post-rotation, before the first new
    /// sample lands) so the warm quantile isn't lost. Both empty → 0
    /// (cold start; [`CellSketches::seed`] overlays).
    pub fn lead_time(&self) -> f64 {
        quantile_or(&self.z_active, self.lead_time_q)
            .or_else(|| quantile_or(&self.z_shadow, self.lead_time_q))
            .unwrap_or(0.0)
    }

    /// Median raw boot time (`q_0.5(boot)`). B10's `consolidate_after`
    /// floor (`q_0.5/2`) and break-even RHS (`cores/q_0.5`) both read
    /// this. Shadow fallback as for [`lead_time`](Self::lead_time);
    /// empty → `None` (caller supplies seed).
    pub fn boot_median(&self) -> Option<f64> {
        quantile_or(&self.boot_active, 0.5).or_else(|| quantile_or(&self.boot_shadow, 0.5))
    }

    /// ICE/boot-failure timeout for this cell. Below
    /// `ICE_REAL_THRESHOLD` real samples, `2×seed` (the seed is the
    /// operator's `xtask k8s probe-boot` measurement; doubling it is the
    /// conservative wait). At ≥100 samples, `q_0.99(boot)` — the
    /// observed tail. Floored at `2×seed` either way so a lucky
    /// fast-boot streak can't shrink the timeout below the operator's
    /// floor.
    pub fn ice_timeout(&self, seed: f64) -> f64 {
        let floor = 2.0 * seed;
        if self.boot_active.count() < ICE_REAL_THRESHOLD {
            return floor;
        }
        quantile_or(&self.boot_active, 0.99).map_or(floor, |q| q.max(floor))
    }

    /// Record one observed `boot` (Registered.transition − created)
    /// with the forecast `eta_error` the claim was provisioned for
    /// (`FORECAST_ETA_ANNOTATION`, per-cell `min(eta_seconds)` at
    /// create time). `z = boot − eta_error` per
    /// `r[ctrl.nodeclaim.lead-time-ddsketch]`.
    pub fn record(&mut self, boot: f64, eta_error: f64) {
        self.z_active.add(boot - eta_error);
        self.boot_active.add(boot);
    }

    /// Active→shadow rotation when `now − epoch > halflife`. Gives a
    /// sliding window: stale samples age out (after 2×halflife they're
    /// gone entirely) without dropping the warm quantile (shadow holds
    /// it while active refills).
    pub fn maybe_rotate(&mut self, now: SystemTime, halflife: Duration) {
        if now.duration_since(self.epoch).unwrap_or(Duration::ZERO) > halflife {
            self.z_shadow =
                std::mem::replace(&mut self.z_active, DDSketch::new(Config::defaults()));
            self.boot_shadow =
                std::mem::replace(&mut self.boot_active, DDSketch::new(Config::defaults()));
            self.epoch = now;
        }
    }

    /// Schmitt trigger on `forecast_warm_hit_ratio` per
    /// `r[ctrl.nodeclaim.lead-time-ddsketch]`: widen `lead_time_q` by
    /// `Δq=0.02` when `hit_ratio < 0.85·target` (under-provisioning;
    /// gated on `lead_time < max_lead_time` so a permanently-slow cell
    /// doesn't ratchet to 0.99 forever); narrow when
    /// `hit_ratio > 1.05·target` (over-provisioning). The `[0.85,1.05]`
    /// dead zone prevents oscillation.
    /// Fold one tick's per-cell `on_reg/(on_reg+on_inf)` into
    /// [`Self::forecast_hit_ewma`] (α=0.2). `hit` is clamped to `[0,1]`
    /// — a `0/0` cell is skipped by the caller.
    pub fn observe_hit_ratio(&mut self, hit: f64) {
        self.forecast_hit_ewma = 0.2 * hit.clamp(0.0, 1.0) + 0.8 * self.forecast_hit_ewma;
    }

    pub fn schmitt_adjust(&mut self, hit_ratio: f64, target: f64, max_lead_time: f64) {
        if hit_ratio < 0.85 * target && self.lead_time() < max_lead_time {
            self.lead_time_q = (self.lead_time_q + 0.02).min(0.99);
        } else if hit_ratio > 1.05 * target {
            self.lead_time_q = (self.lead_time_q - 0.02).max(0.5);
        }
    }
}

/// `s.quantile(q)` flattened. `None` ⇔ empty sketch (or `q∉[0,1]`,
/// which callers don't emit).
fn quantile_or(s: &DDSketch, q: f64) -> Option<f64> {
    s.quantile(q).ok().flatten()
}

/// All cells. `HashMap` keyed by [`Cell`]; `len()` ≈ |hw_classes| × 2
/// (spot+od) — small.
#[derive(Default)]
pub struct CellSketches {
    cells: HashMap<Cell, CellState>,
}

impl CellSketches {
    /// Number of cells with state. Logged at startup.
    pub fn len(&self) -> usize {
        self.cells.len()
    }

    /// `len() == 0` (cold start, or load failed and B9's seed hasn't
    /// run yet).
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// Mutable cell entry, inserting [`CellState::default`] if absent.
    pub fn cell_mut(&mut self, cell: &Cell) -> &mut CellState {
        self.cells.entry(cell.clone()).or_default()
    }

    /// Read-only cell lookup.
    pub fn get(&self, cell: &Cell) -> Option<&CellState> {
        self.cells.get(cell)
    }

    /// Convenience: `cell`'s current provisioning lead-time (seconds).
    /// Unknown cell → 0 (cold start; [`seed`](Self::seed) overlays).
    pub fn lead_time(&self, cell: &Cell) -> f64 {
        self.get(cell).map_or(0.0, CellState::lead_time)
    }

    /// Overlay `sla.leadTimeSeed` onto cells whose active sketch is
    /// empty: inject `N_SEED = 1/(1−q) = 10` copies of the seed into
    /// `z_active` and `boot_active`. Idempotent (skips cells with
    /// `count > 0`). Called once after [`load`](Self::load) so cold-start
    /// `lead_time()` returns the operator's probe-boot measurement
    /// instead of 0. Malformed seed keys (not `"h:cap"`) are skipped +
    /// warned.
    // r[impl ctrl.nodeclaim.lead-time-ddsketch]
    pub fn seed(&mut self, lead_time_seed: &HashMap<String, f64>) {
        for (key, &seed) in lead_time_seed {
            let Some(cell) = Cell::parse(key) else {
                warn!(%key, "lead_time_seed key not parseable as h:cap; skipping");
                continue;
            };
            let s = self.cell_mut(&cell);
            if s.z_active.count() > 0 {
                continue;
            }
            for _ in 0..N_SEED {
                s.z_active.add(seed);
                s.boot_active.add(seed);
            }
            debug!(%cell, seed, "seeded empty cell sketch");
            metrics::counter!(
                "rio_controller_ddsketch_seed_fallback_total",
                "cell" => cell.to_string(),
            )
            .increment(1);
        }
    }

    /// Edge-detect `Registered=True` transitions and record their
    /// `boot_secs()` into the cell's sketches. `recorded` is the
    /// reconciler's running set of NodeClaim names already counted —
    /// each claim contributes exactly one sample over its lifetime.
    /// Names no longer in `live` are pruned from `recorded` so the set
    /// doesn't grow unbounded across NodeClaim churn. Returns the cells
    /// that saw a registration this call (B11's `registered_cells`
    /// ICE-clear signal).
    pub fn observe_registered(
        &mut self,
        live: &[LiveNode],
        recorded: &mut HashSet<String>,
        now_secs: f64,
    ) -> Vec<Cell> {
        let live_names: HashSet<&str> = live.iter().map(|n| n.name.as_str()).collect();
        recorded.retain(|n| live_names.contains(n.as_str()));
        let mut registered_cells = Vec::new();
        for n in live {
            if !n.registered || recorded.contains(&n.name) {
                continue;
            }
            let (Some(cell), Some(boot)) = (n.cell.as_ref(), n.boot_secs()) else {
                continue;
            };
            // Recency-gate: only RECENT registrations are reported as
            // ICE-clear evidence. With `recorded_boot` empty after
            // restart/lease-acquire, days-old nodes would otherwise
            // mass-clear the scheduler's accumulated IceBackoff.
            // `registered_at_secs` (NOT `boot_secs` — that's the
            // DURATION; a 5-day-old node with 18s boot would pass).
            // Stale → record-only (so it doesn't re-edge later) +
            // skip the cell push.
            if n.registered_at_secs()
                .is_some_and(|t| now_secs - t > 3.0 * super::TICK.as_secs_f64())
            {
                recorded.insert(n.name.clone());
                continue;
            }
            // `z = boot − eta` per `r[ctrl.nodeclaim.lead-time-ddsketch]`.
            // `eta` is the per-cell `min(eta_seconds)` `cover_deficit`
            // stamped at create time; absent (Ready-only cell, or
            // pre-F7 claim) → 0 → `z = boot`.
            let eta = n
                .annotation(super::cover::FORECAST_ETA_ANNOTATION)
                .and_then(|s| s.parse().ok())
                .unwrap_or(0.0);
            self.cell_mut(cell).record(boot, eta);
            recorded.insert(n.name.clone());
            registered_cells.push(cell.clone());
        }
        registered_cells
    }

    /// Rotate every cell whose epoch has aged past `halflife`.
    pub fn maybe_rotate_all(&mut self, now: SystemTime, halflife: Duration) {
        for s in self.cells.values_mut() {
            s.maybe_rotate(now, halflife);
        }
    }

    /// Load all rows from `nodeclaim_cell_state`. Unknown sketch
    /// versions decode to an empty sketch (B9's seed overlays).
    /// Unknown `capacity_type` rows are skipped + warned (would
    /// indicate migration drift past the CHECK constraint).
    ///
    /// `sketch_epoch` is read as `EXTRACT(EPOCH ...)::float8` and
    /// rebuilt as `UNIX_EPOCH + secs`: workspace `sqlx` has no
    /// chrono/time feature, and the codebase pattern is to keep
    /// timestamptz arithmetic PG-side (see `rio-scheduler/src/db/
    /// recovery.rs`).
    // r[impl ctrl.nodeclaim.lead-time-ddsketch]
    pub async fn load(pg: &sqlx::PgPool) -> sqlx::Result<Self> {
        let rows = sqlx::query!(
            r#"SELECT hw_class, capacity_type,
                      z_sketch_active, z_sketch_shadow,
                      boot_sketch_active, boot_sketch_shadow,
                      EXTRACT(EPOCH FROM sketch_epoch)::float8 AS "sketch_epoch_secs!",
                      lead_time_q, idle_gap_events
               FROM nodeclaim_cell_state"#
        )
        .fetch_all(pg)
        .await?;

        let mut cells = HashMap::with_capacity(rows.len());
        for r in rows {
            let Some(cap) = CapacityType::parse(&r.capacity_type) else {
                warn!(capacity_type = %r.capacity_type, "unknown capacity_type; skipping row");
                continue;
            };
            let cell = Cell(r.hw_class, cap);
            let state = CellState {
                z_active: decode_or_empty(r.z_sketch_active.as_deref()),
                z_shadow: decode_or_empty(r.z_sketch_shadow.as_deref()),
                boot_active: decode_or_empty(r.boot_sketch_active.as_deref()),
                boot_shadow: decode_or_empty(r.boot_sketch_shadow.as_deref()),
                epoch: SystemTime::UNIX_EPOCH + Duration::from_secs_f64(r.sketch_epoch_secs),
                lead_time_q: r.lead_time_q,
                forecast_hit_ewma: 0.9,
                idle_gap_events: serde_json::from_value(r.idle_gap_events).unwrap_or_else(|e| {
                    warn!(error = %e, "idle_gap_events deserialize failed; resetting");
                    Vec::new()
                }),
            };
            cells.insert(cell, state);
        }
        Ok(Self { cells })
    }

    /// Upsert every cell into `nodeclaim_cell_state`. One round-trip
    /// per cell — |cells| ≈ 24, tick = 10s, so ~2.4 qps; not worth a
    /// batch UNNEST yet. `sketch_epoch` written via `to_timestamp(f64)`
    /// per the no-time-crate pattern (see [`load`](Self::load)).
    pub async fn persist(&self, pg: &sqlx::PgPool) -> sqlx::Result<()> {
        for (cell, s) in &self.cells {
            let idle = serde_json::to_value(&s.idle_gap_events)
                .expect("Vec<IdleGapEvent> is infallibly JSON-serializable");
            let epoch_secs = s
                .epoch
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or(Duration::ZERO)
                .as_secs_f64();
            sqlx::query!(
                r#"INSERT INTO nodeclaim_cell_state
                       (hw_class, capacity_type,
                        z_sketch_active, z_sketch_shadow,
                        boot_sketch_active, boot_sketch_shadow,
                        sketch_epoch, lead_time_q, idle_gap_events)
                   VALUES ($1, $2, $3, $4, $5, $6, to_timestamp($7), $8, $9)
                   ON CONFLICT (hw_class, capacity_type) DO UPDATE SET
                       z_sketch_active   = EXCLUDED.z_sketch_active,
                       z_sketch_shadow   = EXCLUDED.z_sketch_shadow,
                       boot_sketch_active = EXCLUDED.boot_sketch_active,
                       boot_sketch_shadow = EXCLUDED.boot_sketch_shadow,
                       sketch_epoch      = EXCLUDED.sketch_epoch,
                       lead_time_q       = EXCLUDED.lead_time_q,
                       idle_gap_events   = EXCLUDED.idle_gap_events"#,
                cell.0,
                cell.1.as_str(),
                encode_versioned(&s.z_active),
                encode_versioned(&s.z_shadow),
                encode_versioned(&s.boot_active),
                encode_versioned(&s.boot_shadow),
                epoch_secs,
                s.lead_time_q,
                idle,
            )
            .execute(pg)
            .await?;
        }
        Ok(())
    }
}

/// `[SKETCH_VERSION le-u32][postcard(DDSketch)]`. `unwrap`: DDSketch's
/// derived `Serialize` is infallible into `Vec<u8>`.
pub fn encode_versioned(s: &DDSketch) -> Vec<u8> {
    let mut buf = SKETCH_VERSION.to_le_bytes().to_vec();
    buf.extend(postcard::to_stdvec(s).expect("DDSketch serialize is infallible"));
    buf
}

/// Inverse of [`encode_versioned`]. `None` on short input, version
/// mismatch, or postcard error — caller falls back to seed/empty.
pub fn decode_versioned(bytes: &[u8]) -> Option<DDSketch> {
    let tag: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    if u32::from_le_bytes(tag) != SKETCH_VERSION {
        return None;
    }
    postcard::from_bytes(&bytes[4..]).ok()
}

/// `decode_versioned` with `None`-column / decode-failure both mapping
/// to an empty sketch.
fn decode_or_empty(bytes: Option<&[u8]>) -> DDSketch {
    bytes
        .and_then(decode_versioned)
        .unwrap_or_else(|| DDSketch::new(Config::defaults()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_type_round_trip() {
        for ct in [CapacityType::Spot, CapacityType::OnDemand] {
            assert_eq!(CapacityType::parse(ct.as_str()), Some(ct));
        }
        assert_eq!(CapacityType::parse("on-demand"), None);
        assert_eq!(CapacityType::parse(""), None);
    }

    #[test]
    fn cell_parse_display_round_trip() {
        let c = Cell::parse("mid-ebs-x86:spot").unwrap();
        assert_eq!(c.0, "mid-ebs-x86");
        assert_eq!(c.1, CapacityType::Spot);
        assert_eq!(c.to_string(), "mid-ebs-x86:spot");
        // rsplit_once: hw_class names containing ':' are tolerated
        // (none currently do, but defensive).
        let c2 = Cell::parse("weird:name:od").unwrap();
        assert_eq!(c2.0, "weird:name");
        assert_eq!(c2.1, CapacityType::OnDemand);
        assert!(Cell::parse("no-colon").is_none());
        assert!(Cell::parse("h:bad-cap").is_none());
    }

    // r[verify ctrl.nodeclaim.lead-time-ddsketch]
    /// Version-tagged encode/decode round-trips quantiles within
    /// DDSketch's relative-error bound, and the version tag is the
    /// 4-byte LE prefix.
    #[test]
    fn sketch_versioned_round_trip() {
        let mut s = DDSketch::new(Config::defaults());
        for v in [10.0, 12.0, 15.0, 18.0, 20.0] {
            s.add(v);
        }
        let bytes = encode_versioned(&s);
        assert_eq!(&bytes[..4], &SKETCH_VERSION.to_le_bytes());
        let back = decode_versioned(&bytes).expect("decodes");
        let orig = s.quantile(0.9).unwrap().unwrap();
        let rt = back.quantile(0.9).unwrap().unwrap();
        assert!(
            (orig - rt).abs() < 1e-9,
            "postcard round-trip should be exact (got {orig} vs {rt})"
        );
    }

    /// Version mismatch / short / garbage → `None` (caller seeds).
    #[test]
    fn sketch_decode_rejects_mismatch() {
        assert!(decode_versioned(&[]).is_none(), "empty");
        assert!(decode_versioned(&[1, 0, 0]).is_none(), "short tag");
        let bad_ver = [99u8, 0, 0, 0, 1, 2, 3];
        assert!(decode_versioned(&bad_ver).is_none(), "wrong version");
        let bad_body = [1u8, 0, 0, 0, 0xff, 0xff];
        assert!(decode_versioned(&bad_body).is_none(), "garbage body");
    }

    #[test]
    fn decode_or_empty_handles_null_column() {
        let s = decode_or_empty(None);
        assert_eq!(s.count(), 0);
        let s2 = decode_or_empty(Some(&[99, 0, 0, 0]));
        assert_eq!(s2.count(), 0, "bad version → empty");
    }

    #[test]
    fn cell_state_default_q_is_0_9() {
        let s = CellState::default();
        assert!((s.lead_time_q - 0.9).abs() < 1e-12);
        assert_eq!(s.lead_time(), 0.0, "empty sketch → 0 lead time");
        assert!(s.idle_gap_events.is_empty());
    }

    /// `record(boot, eta_error)` feeds `z=boot−eta_error` and `boot`.
    /// `lead_time()` reads `z_active`'s `lead_time_q`-quantile.
    #[test]
    fn record_and_lead_time_quantile() {
        let mut s = CellState::default();
        for boot in [40.0, 42.0, 44.0, 46.0, 48.0, 50.0, 52.0, 54.0, 56.0, 58.0] {
            s.record(boot, 0.0);
        }
        assert_eq!(s.z_active.count(), 10);
        assert_eq!(s.boot_active.count(), 10);
        // q=0.9 over 40..58 step 2 ≈ 56 (within DDSketch rel-error).
        let lt = s.lead_time();
        assert!((54.0..=58.5).contains(&lt), "lead_time={lt}");
        // boot_median ≈ 48.
        let bm = s.boot_median().unwrap();
        assert!((46.0..=50.0).contains(&bm), "boot_median={bm}");
        // Non-zero eta_error: z = boot − eta_error.
        let mut s2 = CellState::default();
        s2.record(50.0, 10.0);
        let z = s2.z_active.quantile(0.5).unwrap().unwrap();
        assert!((z - 40.0).abs() < 1.0, "z=boot−eta_error, got {z}");
    }

    /// r[ctrl.nodeclaim.lead-time-ddsketch]: seed injects N_SEED copies
    /// into empty cells only; `lead_time()` then returns ≈ seed value.
    // r[verify ctrl.nodeclaim.lead-time-ddsketch]
    #[test]
    fn seed_fills_empty_cells_only() {
        let mut sk = CellSketches::default();
        let warm = Cell("warm".into(), CapacityType::Spot);
        sk.cell_mut(&warm).record(100.0, 0.0);
        let seeds: HashMap<String, f64> = [
            ("cold:spot".into(), 45.0),
            ("warm:spot".into(), 999.0),
            ("malformed".into(), 1.0),
        ]
        .into();
        sk.seed(&seeds);
        let cold = Cell("cold".into(), CapacityType::Spot);
        assert_eq!(sk.get(&cold).unwrap().z_active.count(), N_SEED);
        let lt = sk.lead_time(&cold);
        assert!((lt - 45.0).abs() / 45.0 < 0.02, "seed lead_time={lt}");
        // Warm cell untouched (count still 1).
        assert_eq!(sk.get(&warm).unwrap().z_active.count(), 1);
        // Malformed key skipped.
        assert!(
            sk.get(&Cell("malformed".into(), CapacityType::Spot))
                .is_none()
        );
        // Idempotent: second call doesn't double-seed.
        sk.seed(&seeds);
        assert_eq!(sk.get(&cold).unwrap().z_active.count(), N_SEED);
    }

    /// Active→shadow rotation on halflife expiry; `lead_time()` falls
    /// back to shadow when active is empty post-rotation.
    #[test]
    fn sliding_pair_rotates_on_halflife() {
        let t0 = SystemTime::UNIX_EPOCH;
        let mut s = CellState {
            epoch: t0,
            ..Default::default()
        };
        for _ in 0..50 {
            s.record(40.0, 0.0);
        }
        let halflife = Duration::from_secs(6 * 3600);
        // Before halflife: no rotation.
        s.maybe_rotate(t0 + Duration::from_secs(3600), halflife);
        assert_eq!(s.z_active.count(), 50);
        assert_eq!(s.z_shadow.count(), 0);
        // After halflife: active → shadow, active fresh.
        let t1 = t0 + Duration::from_secs(7 * 3600);
        s.maybe_rotate(t1, halflife);
        assert_eq!(s.z_active.count(), 0, "active reset");
        assert_eq!(s.z_shadow.count(), 50, "old active → shadow");
        assert_eq!(s.boot_active.count(), 0);
        assert_eq!(s.boot_shadow.count(), 50);
        assert_eq!(s.epoch, t1, "epoch advances");
        // lead_time() falls back to shadow → still ≈40.
        let lt = s.lead_time();
        assert!((lt - 40.0).abs() / 40.0 < 0.02, "shadow fallback lt={lt}");
        // Second rotate before next halflife: no-op.
        s.maybe_rotate(t1 + Duration::from_secs(60), halflife);
        assert_eq!(s.z_shadow.count(), 50);
    }

    /// Schmitt: `hit_ratio < 0.85·target` widens by 0.02 (capped 0.99,
    /// gated on lead_time < max); `> 1.05·target` narrows (floor 0.5);
    /// dead zone holds.
    #[test]
    fn lead_time_q_widens_on_low_hit_ratio() {
        let mut s = CellState::default();
        s.record(30.0, 0.0);
        // 0.7 < 0.85·0.9=0.765 → widen 0.90→0.92.
        s.schmitt_adjust(0.7, 0.9, 600.0);
        assert!((s.lead_time_q - 0.92).abs() < 1e-9);
        // 0.96 > 1.05·0.9=0.945 → narrow 0.92→0.90.
        s.schmitt_adjust(0.96, 0.9, 600.0);
        assert!((s.lead_time_q - 0.90).abs() < 1e-9);
        // Dead zone: 0.88 ∈ [0.765, 0.945] → hold.
        s.schmitt_adjust(0.88, 0.9, 600.0);
        assert!((s.lead_time_q - 0.90).abs() < 1e-9);
        // Widen capped at 0.99.
        s.lead_time_q = 0.98;
        s.schmitt_adjust(0.1, 0.9, 600.0);
        assert!((s.lead_time_q - 0.99).abs() < 1e-9);
        s.schmitt_adjust(0.1, 0.9, 600.0);
        assert!((s.lead_time_q - 0.99).abs() < 1e-9, "stays at cap");
        // Widen gated on lead_time < max: max=10 < lt≈30 → no widen.
        let mut s2 = CellState::default();
        s2.record(30.0, 0.0);
        s2.schmitt_adjust(0.1, 0.9, 10.0);
        assert!((s2.lead_time_q - 0.90).abs() < 1e-9, "max gate holds q");
        // Narrow floored at 0.5.
        s2.lead_time_q = 0.51;
        s2.schmitt_adjust(0.99, 0.9, 600.0);
        assert!((s2.lead_time_q - 0.50).abs() < 1e-9);
    }

    /// `ice_timeout`: <100 samples → 2×seed; ≥100 → q_0.99(boot)
    /// floored at 2×seed.
    #[test]
    fn ice_timeout_uses_seed_floor_below_threshold() {
        let mut s = CellState::default();
        for _ in 0..50 {
            s.record(40.0, 0.0);
        }
        assert_eq!(s.ice_timeout(45.0), 90.0, "n<100 → 2×seed");
        for _ in 0..60 {
            s.record(40.0, 0.0);
        }
        // n=110 ≥ 100; q_0.99 ≈ 40 < 2×45 → floor at 90.
        assert_eq!(s.ice_timeout(45.0), 90.0, "floored at 2×seed");
        // Seed=10 → floor=20 < q_0.99≈40 → returns ≈40.
        let t = s.ice_timeout(10.0);
        assert!((38.0..=42.0).contains(&t), "q_0.99 path t={t}");
    }

    /// Edge detection: each `Registered=True` LiveNode records once;
    /// re-seeing it next tick doesn't re-record; pruned from `recorded`
    /// when gone from `live`.
    #[test]
    fn observe_registered_edge_detection() {
        use super::super::ffd::tests::{node, with_conds};
        let mut sk = CellSketches::default();
        let mut recorded = HashSet::new();
        let cell = Cell("h".into(), CapacityType::Spot);

        // now=1060: Registered.transition at 1042..1050 → recency
        // (now − reg_at < 30s) passes for all.
        let now = 1060.0;
        let reg = |name: &str, boot: f64| {
            with_conds(
                node(name, "h", CapacityType::Spot, 8, 0, 0),
                &[("Registered", "True", 1000.0 + boot)],
            )
        };
        let mut inflight = node("c", "h", CapacityType::Spot, 8, 0, 0);
        inflight.registered = false;

        // Tick 1: a (boot=42) + c (in-flight). a recorded; c not.
        let cells = sk.observe_registered(&[reg("a", 42.0), inflight.clone()], &mut recorded, now);
        assert_eq!(cells, vec![cell.clone()]);
        assert_eq!(sk.get(&cell).unwrap().boot_active.count(), 1);
        assert!(recorded.contains("a"));
        assert!(!recorded.contains("c"));

        // Tick 2: a again + b (boot=50). Only b is a new edge.
        let cells = sk.observe_registered(&[reg("a", 42.0), reg("b", 50.0)], &mut recorded, now);
        assert_eq!(cells, vec![cell.clone()]);
        assert_eq!(sk.get(&cell).unwrap().boot_active.count(), 2);
        assert_eq!(recorded.len(), 2);

        // Tick 3: only b. a pruned from `recorded`.
        sk.observe_registered(&[reg("b", 50.0)], &mut recorded, now);
        assert_eq!(sk.get(&cell).unwrap().boot_active.count(), 2, "no new edge");
        assert!(!recorded.contains("a"), "pruned");
        assert_eq!(recorded.len(), 1);
    }

    /// mb_076 recency-gate: a node registered hours ago (e.g.
    /// `recorded_boot` empty after restart/lease-acquire) is
    /// recorded-only — its cell is NOT pushed (else it would
    /// mass-clear the scheduler's IceBackoff). Uses `now −
    /// registered_at`, NOT `boot_secs()` (boot DURATION; a 5-day-old
    /// node with 18s boot would wrongly pass).
    #[test]
    fn observe_registered_recency_gates_stale() {
        use super::super::ffd::tests::{node, with_conds};
        let mut sk = CellSketches::default();
        let mut recorded = HashSet::new();
        // Registered at t=100; boot_secs() = 100 − 1000(created)? No —
        // created defaults to 1000.0 in `node()` so boot=−900, which
        // boot_secs would return as Some(−900). But registered_at=100.
        // Use plausible: created=1000 (node() default), Registered at
        // 1018 (boot=18s), now=10000 (hours later).
        let stale = with_conds(
            node("stale", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1018.0)],
        );
        let cells = sk.observe_registered(&[stale], &mut recorded, 10_000.0);
        assert!(
            cells.is_empty(),
            "stale (now−reg_at=8982s ≫ 30s) NOT pushed as ICE-clear"
        );
        assert!(recorded.contains("stale"), "still recorded (no re-edge)");
    }

    /// F7: NodeClaim annotated `rio.build/forecast-eta-secs=30` with
    /// boot=45 → `z = boot − eta = 15` recorded (not 45). Annotation
    /// absent/unparseable → eta=0 (z = boot).
    #[test]
    fn observe_registered_reads_forecast_eta_annotation() {
        use super::super::cover::FORECAST_ETA_ANNOTATION;
        use super::super::ffd::tests::{node, with_conds};
        let mut sk = CellSketches::default();
        let mut recorded = HashSet::new();
        let cell = Cell("h".into(), CapacityType::Spot);

        let mut a = with_conds(
            node("a", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1045.0)],
        );
        a.annotations
            .insert(FORECAST_ETA_ANNOTATION.into(), "30".into());
        // boot = 1045 − 1000 = 45; eta = 30; z = 15.
        sk.observe_registered(&[a], &mut recorded, 1060.0);
        let s = sk.get(&cell).unwrap();
        let z = s.z_active.quantile(0.5).unwrap().unwrap();
        assert!((z - 15.0).abs() < 1.0, "z = boot − eta = 15, got {z}");
        let b = s.boot_active.quantile(0.5).unwrap().unwrap();
        assert!((b - 45.0).abs() < 1.0, "raw boot = 45, got {b}");

        // No annotation → eta=0 → z=boot.
        let mut sk2 = CellSketches::default();
        let mut rec2 = HashSet::new();
        let n = with_conds(
            node("n", "h", CapacityType::Spot, 8, 0, 0),
            &[("Registered", "True", 1045.0)],
        );
        sk2.observe_registered(&[n], &mut rec2, 1060.0);
        let z2 = sk2
            .get(&cell)
            .unwrap()
            .z_active
            .quantile(0.5)
            .unwrap()
            .unwrap();
        assert!((z2 - 45.0).abs() < 1.0, "no annotation → z=boot, got {z2}");
    }

    /// PG round-trip: persist two cells, load, compare quantiles +
    /// idle-gap events. Uses `rio-test-support`'s ephemeral postgres.
    #[tokio::test]
    async fn persist_load_round_trip() {
        let db = rio_test_support::pg::TestDb::new(&crate::MIGRATOR).await;

        let mut sketches = CellSketches::default();
        let cell_a = Cell("hi-nvme-x86".into(), CapacityType::Spot);
        let cell_b = Cell("lo-ebs-arm".into(), CapacityType::OnDemand);
        {
            let a = sketches.cell_mut(&cell_a);
            for v in [5.0, 6.0, 7.0, 8.0, 9.0] {
                a.z_active.add(v);
                a.boot_active.add(v + 10.0);
            }
            a.lead_time_q = 0.92;
            a.idle_gap_events.push(IdleGapEvent {
                gap_secs: 12.5,
                censored: false,
            });
        }
        sketches.cell_mut(&cell_b).lead_time_q = 0.5;

        sketches.persist(&db.pool).await.expect("persist");

        let loaded = CellSketches::load(&db.pool).await.expect("load");
        assert_eq!(loaded.len(), 2);

        let la = loaded.get(&cell_a).expect("cell_a");
        assert!((la.lead_time_q - 0.92).abs() < 1e-12);
        assert_eq!(la.z_active.count(), 5);
        assert_eq!(la.boot_active.count(), 5);
        assert_eq!(la.idle_gap_events.len(), 1);
        assert!((la.idle_gap_events[0].gap_secs - 12.5).abs() < 1e-12);
        assert!(!la.idle_gap_events[0].censored);

        let lb = loaded.get(&cell_b).expect("cell_b");
        assert!((lb.lead_time_q - 0.5).abs() < 1e-12);
        assert_eq!(lb.z_active.count(), 0, "default sketch persists empty");

        // Second persist on same cells = upsert (no PK violation).
        sketches.persist(&db.pool).await.expect("upsert");
        assert_eq!(CellSketches::load(&db.pool).await.unwrap().len(), 2);
    }
}

/// `IdleGapEvent` serde shape — re-exported from `consolidate` but the
/// jsonb wire form is asserted here next to the PG round-trip.
#[cfg(test)]
#[test]
fn idle_gap_event_jsonb_shape() {
    let e = IdleGapEvent {
        gap_secs: 3.5,
        censored: true,
    };
    let json = serde_json::to_value([&e]).unwrap();
    assert_eq!(json[0]["gap_secs"], 3.5);
    assert_eq!(json[0]["censored"], true);
    let back: Vec<IdleGapEvent> = serde_json::from_value(json).unwrap();
    assert_eq!(back.len(), 1);
}

// Ensure `Serialize`/`Deserialize` are derived on IdleGapEvent for the
// jsonb column. The derive lives in consolidate.rs; this `use` keeps the
// trait bounds checked in the persist module that depends on them.
const _: fn() = || {
    fn assert_serde<T: Serialize + for<'de> Deserialize<'de>>() {}
    assert_serde::<IdleGapEvent>();
};
