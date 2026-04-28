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
//! Persisted as version-tagged bincode `bytea` (DDSketch's bucket array
//! is dense-packed integers; ~1KiB binary vs ~8KiB JSON) per
//! `r[ctrl.nodeclaim.lead-time-ddsketch]`. The reconciler loads on
//! construct, persists every tick.

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use serde::{Deserialize, Serialize};
use sketches_ddsketch::{Config, DDSketch};
use tracing::warn;

use super::consolidate::IdleGapEvent;

/// bincode encoding version tag (4 LE bytes prefix). Bump on any
/// `sketches-ddsketch` serde-shape change; [`decode_versioned`] returns
/// `None` on mismatch and the caller falls back to seed.
const SKETCH_VERSION: u32 = 1;

/// Spot vs on-demand. The migration's CHECK constraint pins the wire
/// strings; [`as_str`](Self::as_str) is the single mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
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
/// `[sla.hw_classes.$h]` key (e.g. `"mid-ebs-x86"`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
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
            idle_gap_events: Vec::new(),
        }
    }
}

impl CellState {
    /// `q`-quantile of the active `z` sketch — the provisioning
    /// lead-time. Empty sketch → 0 (cold start; B9's seed overlays).
    // TODO(B9): Schmitt-adjust `lead_time_q`, `record()`, `maybe_rotate()`.
    pub fn lead_time(&self) -> f64 {
        self.z_active
            .quantile(self.lead_time_q)
            .ok()
            .flatten()
            .unwrap_or(0.0)
    }
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

/// `[SKETCH_VERSION le-u32][bincode(DDSketch)]`. `unwrap`: DDSketch's
/// derived `Serialize` is infallible into `Vec<u8>`.
pub fn encode_versioned(s: &DDSketch) -> Vec<u8> {
    let mut buf = SKETCH_VERSION.to_le_bytes().to_vec();
    buf.extend(
        bincode::serde::encode_to_vec(s, bincode::config::standard())
            .expect("DDSketch serialize is infallible"),
    );
    buf
}

/// Inverse of [`encode_versioned`]. `None` on short input, version
/// mismatch, or bincode error — caller falls back to seed/empty.
pub fn decode_versioned(bytes: &[u8]) -> Option<DDSketch> {
    let tag: [u8; 4] = bytes.get(..4)?.try_into().ok()?;
    if u32::from_le_bytes(tag) != SKETCH_VERSION {
        return None;
    }
    bincode::serde::decode_from_slice(&bytes[4..], bincode::config::standard())
        .ok()
        .map(|(s, _)| s)
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
            "bincode round-trip should be exact (got {orig} vs {rt})"
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
