//! Algorithm 2 (ADR-023 §3.2): saturation-gated exploration.
//!
//! Drives the cold-start probe ladder before a key has enough span/n_eff
//! for `solve_tier`. Walks `c` away from `cfg.probe.cpu` — ×4 up while
//! the last build saturated AND missed its tier's SLA target
//! ([`Tier::binding_bound`]), ÷2 down otherwise — until the observed
//! `[min_c, max_c]` span reaches 4× (the fit gate), or hits a wall
//! (`max_cores` / `1`). At that point the ladder freezes and
//! `intent_for` switches to the solve path.
//!
//! [`Tier::binding_bound`]: super::solve::Tier::binding_bound

use super::config::{Cell, HwClassName, SlaConfig};
use super::fit::headroom;
use super::solve::{DrvHints, SolveFullResult, SolveMemo};
use super::types::{DiskBytes, FittedParams, MemBytes, RawCores};
use std::collections::HashSet;

/// Per-tick `{pool, masked}` snapshot for ε_h exploration. Groups the
/// inputs that are **read-only borrows of the same poll's state** —
/// semantically distinct from the per-key `(prev, mkh, ovr)` and the
/// `try_solve` effect. Built in `solve_intent_for`'s ε_h block: `pool`
/// per memo-key (depends on the unrestricted memo's A and `cheapest`);
/// `masked` tick-wide. The prev-validity filter tests `∈ pool` only —
/// `h_all`/`in_a` are subsumed (`pool` = output of [`h_explore_pool`]).
pub struct HExploreCtx<'a> {
    pub pool: &'a [&'a HwClassName],
    pub masked: &'a HashSet<Cell>,
}

/// The `(pinned_explore, pinned_explore_a)` pair as one value
/// (§Eighth-strike, R17B0). [`resolve_h_explore`] is `HExplorePin →
/// HExplorePin`: it owns the FULL transition for both halves, so the
/// caller cannot construct an out-of-sync `(h, prev_a)` (the r17
/// bug_001 shape — `h` transition in explore.rs, `prev_a` transition
/// reconstructed at the caller from a lossy `Miss{next}`).
///
/// `prev_a` semantics by [`HExploreOutcome`] arm:
/// - `Hit` → fresh `m.a.cells` (the restricted solve's A').
/// - `Miss` via `Feasible(m)`-all-masked, singleton (`next==h_tried`)
///   → fresh `m.a.cells` — the solve PRODUCED an A'; the
///   [`super::solve::MemoEntry::pinned_explore_a`] contract says store
///   it. (r17 bug_001: the conflated `_ =>` arm dropped `m`.)
/// - `Miss` via `BestEffort`, singleton (`next==prev.h`) → preserve
///   `prev.prev_a` — the solve produced no A'.
/// - `Miss` with rotation (`next≠h_tried`) or empty pool → `∅` — new
///   `h`'s first solve uses τ_enter, correctly.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct HExplorePin {
    pub h: Option<HwClassName>,
    pub prev_a: HashSet<Cell>,
}

/// ε_h pin state-transition outcome. Sum type — NOT `(Option, Option)`
/// — so the "Feasible-but-committed-nothing" / "Miss-but-returned-a-
/// memo" states are unrepresentable. Both arms carry the full
/// [`HExplorePin`] — caller commits `outcome.pin()` unconditionally.
pub enum HExploreOutcome {
    /// `try_solve(h, prev_a)` was [`SolveFullResult::Feasible`] AND not
    /// every cell of its A' is in `masked`. Caller commits `pin` and
    /// emits `memo`'s A'.
    Hit { memo: SolveMemo, pin: HExplorePin },
    /// Pre-solve release (`prev.h ∉ pool`) re-drew, OR post-solve
    /// release (infeasible / all-masked) round-robin'd to the next
    /// candidate. Caller commits `pin` and falls through to the
    /// unrestricted memo. `pin.h` is NOT solved this tick — next
    /// tick's `prev = pin` gets its one solve. `pin.h = None` ⇔ pool
    /// empty. `pin.prev_a` per [`HExplorePin`] doc.
    Miss { pin: HExplorePin },
}

impl HExploreOutcome {
    /// The committed pin, regardless of arm. Caller writes
    /// `(pinned_explore, pinned_explore_a) = (pin.h, pin.prev_a)`.
    pub fn pin(&self) -> &HExplorePin {
        match self {
            Self::Hit { pin, .. } | Self::Miss { pin } => pin,
        }
    }
}

/// ε_h candidate pool (§Seventh-strike extraction). `0 < |A| < |H|`
/// (normal mode) → `H\A`: cheapest may be ∈ pool when `cheapest ∉ A` —
/// valid explore target since the unrestricted solve picks from A, not
/// from `cheapest`. `|A| ∈ {0, |H|}` (degenerate fallback) →
/// `H\{cheapest}`: cache-miss or A=H, where `H\A` would be H or ∅ and
/// ε_h would re-select the price-dominant cell (ADR-023 L748).
///
/// The **else-branch pool is cheapest-independent** (stable under
/// spot-price flap); the fallback-branch pool varies with `cheapest` —
/// [`resolve_h_explore`]'s `h ∈ pool` prev-filter absorbs that as
/// ≤1 redraw.
pub fn h_explore_pool<'a>(
    h_all: &'a [HwClassName],
    in_a: &HashSet<HwClassName>,
    cheapest: Option<&HwClassName>,
) -> Vec<&'a HwClassName> {
    if in_a.is_empty() || in_a.len() == h_all.len() {
        h_all.iter().filter(|h| Some(*h) != cheapest).collect()
    } else {
        h_all.iter().filter(|h| !in_a.contains(*h)).collect()
    }
}

/// ε_h pin state transition (§Fifth-strike extraction). Draw seeded
/// from `mkh ^ ovr` — `ovr = 0` common-case → seed = `mkh`;
/// well-distributed; XOR collisions across distinct `(mkh, ovr)` only
/// affect initial-draw coincidence, not storage. Iteration-order-
/// independent: `pool` slice order does not affect the chosen `h`
/// (`choose` indexes by `pin_rng`, and `pin_rng` is a function of the
/// storage key alone).
///
/// **Deterministic, not pure**: the function is deterministic in
/// `(prev, mkh, ovr, h_all, in_a, pool, masked, try_solve-result)`;
/// it is NOT pure in the first seven (the eighth — `try_solve`'s
/// return — is a load-bearing input). [`FnOnce`] is the type-level
/// proof of "at most one solve per ε_h hit": the post-solve release
/// path CANNOT retry this call (the closure is consumed).
///
/// Release semantics:
/// - **Pre-solve** (`prev ∉ pool`, or `prev = None`): re-draw from
///   `pool` via `pin_rng`, THEN the one `try_solve` on the redrawn h.
///   `pool` = output of [`h_explore_pool`], so this subsumes
///   graduated / config-removed (always) and became-cheapest
///   (fallback mode only).
/// - **Post-solve** (`try_solve` infeasible, OR Feasible-but-all-
///   masked): NO retry this call. Round-robin over `sorted(pool)`:
///   `next = pool[(idx_of(h_tried)+1) % |pool|]`. Covers every pool
///   element in `|pool|` consecutive misses while pool composition
///   stable; on pool change, prev re-validated `∈ pool` (≤1 re-draw
///   transition). `|pool|=1` → fixed point, `|pool|=0` → `None`.
///
/// Within one tick, multiple drvs sharing `(mkh, ovr)` may each call
/// `try_solve` on the same `h` if it never commits (all see `prev =
/// None` → same seed → same `h`). The first to reach `update_entry`
/// commits the rotated `next`; subsequent same-tick drvs see `prev =
/// next`. Bounded N× cost, deterministic, no correctness impact.
// r[impl sched.sla.hw-class.epsilon-explore+6]
pub fn resolve_h_explore(
    prev: HExplorePin,
    mkh: u64,
    ovr: u64,
    ctx: &HExploreCtx<'_>,
    try_solve: impl FnOnce(&HwClassName, &HashSet<Cell>) -> SolveFullResult,
) -> HExploreOutcome {
    use rand::SeedableRng;
    use rand::seq::IndexedRandom;
    let mut pin_rng = rand::rngs::StdRng::seed_from_u64(mkh ^ ovr);
    // `IndexedRandom::choose` is position-based; sort so slice order
    // (caller's `h_all.iter().filter(..)` order, or HashMap-derived
    // order in a future caller) does NOT leak into the pin value.
    // |pool| ≤ |H| (config-bounded, typically <10).
    let mut pool: Vec<&HwClassName> = ctx.pool.to_vec();
    pool.sort_unstable();
    // Pre-solve release: prev.h still valid iff Some(h) ∧ h ∈ pool.
    // `pool` = output of `h_explore_pool` so this subsumes the prior
    // `∈ h_all ∧ ∉ in_a` filter AND rejects became-cheapest in
    // fallback mode (normal mode `cheapest ∉ A` may be ∈ pool — see
    // `h_explore_pool` doc). With this, `h_to_try ∈ pool` by
    // construction (round-robin's .position() relies on it).
    let h_to_try = match prev.h.clone().filter(|h| pool.contains(&h)) {
        Some(h) => h,
        None => match pool.choose(&mut pin_rng) {
            Some(&h) => h.clone(),
            None => {
                return HExploreOutcome::Miss {
                    pin: HExplorePin::default(),
                };
            }
        },
    };
    // Post-solve release: round-robin over sorted(pool); covers pool
    // in |pool| consecutive misses while pool stable. Pool changes
    // (in_a/cheapest update): prev re-validated ∈ pool, ≤1 re-draw
    // transition. |pool|=1 → fixed point (next == h_to_try).
    let next = || {
        let idx = pool
            .iter()
            .position(|h| **h == h_to_try)
            .expect("h_to_try ∈ pool by filter above");
        (*pool[(idx + 1) % pool.len()]).clone()
    };
    match try_solve(&h_to_try, &prev.prev_a) {
        SolveFullResult::Feasible(m) if !m.a.cells.iter().all(|c| ctx.masked.contains(c)) => {
            let prev_a = m.a.cells.iter().cloned().collect();
            HExploreOutcome::Hit {
                memo: m,
                pin: HExplorePin {
                    h: Some(h_to_try),
                    prev_a,
                },
            }
        }
        // Feasible-but-all-masked: the solve PRODUCED a fresh A'
        // (`m.a.cells`); `pinned_explore_a`'s contract says store it.
        // Singleton (next==h_tried) → keep the fresh A' for the next
        // solve's Schmitt prev_a. Rotation → ∅ (new h, τ_enter only).
        SolveFullResult::Feasible(m) => {
            let next = next();
            HExploreOutcome::Miss {
                pin: HExplorePin {
                    prev_a: if next == h_to_try {
                        m.a.cells.iter().cloned().collect()
                    } else {
                        HashSet::new()
                    },
                    h: Some(next),
                },
            }
        }
        // BestEffort: the solve produced NO A'. Singleton
        // (next==prev.h) → preserve `prev.prev_a` (the last solve
        // that DID produce one). Rotation → ∅.
        SolveFullResult::BestEffort { .. } => {
            let next = next();
            HExploreOutcome::Miss {
                pin: HExplorePin {
                    prev_a: if Some(&next) == prev.h.as_ref() {
                        prev.prev_a
                    } else {
                        HashSet::new()
                    },
                    h: Some(next),
                },
            }
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ExploreDecision {
    pub c: RawCores,
    pub mem: MemBytes,
    pub disk: DiskBytes,
}

/// Next explore step. `fit=None` (never-seen key) → cold-start at the
/// probe shape (or a feature-specific override). `fit=Some` with the
/// ladder still walking → ×4 / ÷2 from the explore state. Ladder
/// frozen → re-emit `max_c` (the solve gate in `intent_for` will pick
/// `solve_tier` instead, but a frozen-yet-immature fit — e.g. n_eff<3
/// after a version bump — falls through here and should hold position).
// r[impl sched.sla.explore-saturation-gate]
// r[impl sched.sla.explore-x4-first-bump]
// r[impl sched.sla.explore-freeze]
pub fn next(fit: Option<&FittedParams>, cfg: &SlaConfig, hints: &DrvHints) -> ExploreDecision {
    let probe = hints
        .required_features
        .iter()
        .find_map(|f| cfg.feature_probes.get(f))
        .unwrap_or(&cfg.probe);
    let mem_for = |c: f64| MemBytes((c * probe.mem_per_core as f64 + probe.mem_base as f64) as u64);

    let Some(f) = fit else {
        return decision(probe.cpu, mem_for, DiskBytes(cfg.default_disk));
    };
    // `resource_floor` is per-drv_hash so a fresh version would
    // otherwise re-climb OOM/DiskPressure from probe defaults. Disk is
    // a core-independent scalar (r[sched.sla.disk-scalar]); mem
    // evaluates `MemFit::at(c)` at the explore-chosen c, scaled by
    // `headroom(n_eff)` (r[sched.sla.headroom-confidence-scaled]) —
    // `MemFit::Coupled.at()` is the regression line (~p50), not a
    // quantile. `.max(probe shape)` guards the Independent{p90:0}
    // sentinel (no prior sample).
    let h = headroom(f.n_eff_ring);
    let mem_for = move |c: f64| {
        MemBytes(
            mem_for(c)
                .0
                .max((f.mem.at(RawCores(c)).0 as f64 * h) as u64),
        )
    };
    let disk = DiskBytes(f.disk_p90.map(|d| d.0).unwrap_or(cfg.default_disk));
    let st = &f.explore;
    // First sample landed but min/max not yet diverse → treat as cold.
    if st.max_c.0 <= 0.0 {
        return decision(probe.cpu, mem_for, disk);
    }
    if frozen(st, cfg.max_cores) {
        return decision(st.max_c.0, mem_for, disk);
    }
    let target = tier_target(f, cfg);
    if st.saturated && st.last_wall.0 > target {
        let c_up = (st.max_c.0 * 4.0).min(cfg.max_cores);
        if st.distinct_c >= 3 && c_up >= cfg.max_cores {
            ::metrics::counter!(
                "rio_scheduler_sla_suspicious_scaling_total",
                "tenant" => f.key.tenant.clone()
            )
            .increment(1);
        }
        // Clamp ate the step (already at ceiling, gradient into wall)
        // → step the OPPOSITE direction once so `distinct_c` reaches 2
        // and `frozen()`'s wall clause can fire on the next sample.
        let c = if c_up > st.max_c.0 {
            c_up
        } else {
            (st.min_c.0 / 2.0).floor().max(1.0)
        };
        decision(c, mem_for, disk)
    } else {
        let c_down = (st.min_c.0 / 2.0).floor().max(1.0);
        let c = if c_down < st.min_c.0 {
            c_down
        } else {
            (st.max_c.0 * 4.0).min(cfg.max_cores)
        };
        decision(c, mem_for, disk)
    }
}

/// Freeze predicate, shared with [`super::solve::intent_for`]'s gate so
/// "explore done" and "solve takes over" agree on the same boundary.
///
/// The wall checks are gated on `distinct_c >= 2`: a probe configured
/// at the wall (`probe.cpu == max_cores` or `== 1.0`, both permitted by
/// `validate()`) lands its first sample with `min_c == max_c == wall`;
/// without the guard that's `frozen=true` → re-emit `wall` forever and
/// the ladder never walks. With the guard, the first sample is treated
/// as "started at the wall" (walk away from it), not "walked to the
/// wall" (freeze).
pub(crate) fn frozen(st: &super::types::ExploreState, max_cores: f64) -> bool {
    let span = if st.min_c.0 > 0.0 {
        st.max_c.0 / st.min_c.0
    } else {
        1.0
    };
    span >= 4.0 || (st.distinct_c >= 2 && (st.max_c.0 >= max_cores || st.min_c.0 <= 1.0))
}

fn decision(c: f64, mem_for: impl Fn(f64) -> MemBytes, disk: DiskBytes) -> ExploreDecision {
    ExploreDecision {
        c: RawCores(c),
        mem: mem_for(c),
        disk,
    }
}

/// [`Tier::binding_bound`] of `fit.tier` if assigned, else
/// `cfg.default_tier`, else 1200s. Uses the config-side tier list (not
/// `solve_tiers()`) since we only need a name lookup.
///
/// [`Tier::binding_bound`]: super::solve::Tier::binding_bound
fn tier_target(fit: &FittedParams, cfg: &SlaConfig) -> f64 {
    cfg.tiers
        .iter()
        .find(|t| Some(&t.name) == fit.tier.as_ref())
        .or_else(|| cfg.tiers.iter().find(|t| t.name == cfg.default_tier))
        .and_then(super::solve::Tier::binding_bound)
        .unwrap_or(1200.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sla::config::{CapacityType, ProbeShape};
    use crate::sla::solve::{AdmissibleSet, InfeasibleReason, Tier};
    use crate::sla::types::{
        DurationFit, ExploreState, FitDf, MemFit, ModelKey, RingNEff, WallSeconds,
    };

    // ─── resolve_h_explore unit tests ────────────────────────────────────

    fn h(s: &str) -> HwClassName {
        s.to_string()
    }

    fn pin(h: Option<HwClassName>) -> HExplorePin {
        HExplorePin {
            h,
            prev_a: HashSet::new(),
        }
    }

    fn ctx<'a>(pool: &'a [&'a HwClassName], masked: &'a HashSet<Cell>) -> HExploreCtx<'a> {
        HExploreCtx { pool, masked }
    }

    /// **bug_003 falsification (R8B0)** — `h_explore_pool` is the ONE
    /// constructor of `HExploreCtx.pool`; its semantics are the
    /// doc-contract. Normal mode (`0<|A|<|H|`) is `H\A` — cheapest may
    /// be ∈ pool when `cheapest ∉ A` (case 1). Fallback (`|A|∈{0,|H|}`)
    /// is `H\{cheapest}` (cases 2+3). Pre-R8B0 the doc-contract claimed
    /// pool was always a subset of `h_all\in_a\{cheapest}`; the caller's
    /// else-branch built `H\A` — case 1's `{A,C,D}` ⊄ `{C,D}`.
    #[test]
    fn h_explore_pool_matches_doc() {
        let h_all = [h("A"), h("B"), h("C"), h("D")];
        let set = |hs: &[&HwClassName]| hs.iter().map(|h| (**h).clone()).collect::<HashSet<_>>();
        // (1) normal mode, cheapest ∉ A: pool = H\A = {A,C,D}.
        // bug_003: cheapest=A IS in the pool — the else-branch is
        // cheapest-independent. Doc/spec must match this, not the
        // other way round (exploring `cheapest ∉ A` is valid).
        let in_a: HashSet<_> = [h("B")].into();
        let pool = h_explore_pool(&h_all, &in_a, Some(&h("A")));
        assert_eq!(
            set(&pool),
            [h("A"), h("C"), h("D")].into(),
            "normal mode (0<|A|<|H|): pool = H\\A; cheapest ∉ A → \
             cheapest ∈ pool (bug_003 — doc claimed ⊆ H\\A\\{{cheapest}})"
        );
        // (2) fallback, in_a=∅ (cache miss): pool = H\{cheapest}.
        let pool = h_explore_pool(&h_all, &HashSet::new(), Some(&h("A")));
        assert_eq!(
            set(&pool),
            [h("B"), h("C"), h("D")].into(),
            "fallback (|A|=0): pool = H\\{{cheapest}}"
        );
        // (3) fallback, A=H: pool = H\{cheapest}.
        let in_a: HashSet<_> = h_all.iter().cloned().collect();
        let pool = h_explore_pool(&h_all, &in_a, Some(&h("A")));
        assert_eq!(
            set(&pool),
            [h("B"), h("C"), h("D")].into(),
            "fallback (A=H): pool = H\\{{cheapest}}"
        );
    }

    fn feasible(cells: Vec<Cell>) -> SolveFullResult {
        SolveFullResult::Feasible(SolveMemo {
            a: AdmissibleSet {
                cells,
                c_star: 4,
                mem_bytes: 0,
                disk_bytes: 0,
            },
            all_candidates: Vec::new(),
            tier: "normal".into(),
        })
    }

    fn besteffort() -> SolveFullResult {
        SolveFullResult::BestEffort {
            c: 1,
            mem_bytes: 0,
            disk_bytes: 0,
            cells: Vec::new(),
            why: InfeasibleReason::SerialFloor,
        }
    }

    type Recorded = std::rc::Rc<std::cell::Cell<Option<HwClassName>>>;

    /// Records `try_solve`'s `h` argument (the chosen `h_to_try`) and
    /// returns `result`. For asserting on which h was picked.
    fn recording(
        result: SolveFullResult,
    ) -> (
        Recorded,
        impl FnOnce(&HwClassName, &HashSet<Cell>) -> SolveFullResult,
    ) {
        let slot: Recorded = std::rc::Rc::new(std::cell::Cell::new(None));
        let s = slot.clone();
        (slot, move |h: &HwClassName, _: &HashSet<Cell>| {
            s.set(Some(h.clone()));
            result
        })
    }

    /// **bug_004 falsification** — pin VALUE seeded from `mkh ^ ovr`,
    /// independent of `pool` slice order. Same `(prev=None, mkh, ovr)`
    /// with `pool=[a,b,c]` vs `pool=[c,a,b]` MUST pick the same h.
    /// Pre-R6B5 the value was `pool.choose(&mut per-drv-rng)` —
    /// HashMap iteration order leaked into the pin.
    #[test]
    fn resolve_pool_permutation_independent() {
        let h_all = [h("a"), h("b"), h("c")];
        let masked = HashSet::new();
        let p1 = [&h_all[0], &h_all[1], &h_all[2]];
        let p2 = [&h_all[2], &h_all[0], &h_all[1]];
        for (mkh, ovr) in [(7u64, 0u64), (0xdead_beef, 0), (42, 1234)] {
            let (rec1, ts1) = recording(feasible(vec![(h("a"), CapacityType::Spot)]));
            let (rec2, ts2) = recording(feasible(vec![(h("a"), CapacityType::Spot)]));
            let _ = resolve_h_explore(pin(None), mkh, ovr, &ctx(&p1, &masked), ts1);
            let _ = resolve_h_explore(pin(None), mkh, ovr, &ctx(&p2, &masked), ts2);
            assert_eq!(
                rec1.take(),
                rec2.take(),
                "pool order [a,b,c] vs [c,a,b] picked different h at \
                 (mkh={mkh:#x}, ovr={ovr}) — pin VALUE must be \
                 iteration-order-independent (bug_004)"
            );
        }
    }

    /// **mb_011-A** — `try_solve` infeasible → `Miss{next}` where
    /// `next ∈ pool \ {h_tried}`. NOT stuck on the infeasible h.
    /// `|pool|=3` so a 2-cycle does NOT trivially satisfy `next ≠
    /// h_tried` (mb_001's degenerate-domain shape).
    #[test]
    fn resolve_besteffort_rotates() {
        let h_all = [h("h1"), h("h2"), h("h3")];
        let masked = HashSet::new();
        let pool = [&h_all[0], &h_all[1], &h_all[2]];
        let (rec, ts) = recording(besteffort());
        let out = resolve_h_explore(pin(None), 99, 0, &ctx(&pool, &masked), ts);
        let h_tried = rec.take().expect("try_solve called");
        let HExploreOutcome::Miss { pin: p } = out else {
            panic!("BestEffort → Miss, not Hit")
        };
        let n =
            p.h.expect("pool.len()=3 → pool\\{h_tried} non-empty → next=Some");
        assert_ne!(
            n, h_tried,
            "infeasible → rotate to pool\\{{h_tried}}; got next={n} == h_tried"
        );
        assert!(pool.contains(&&n), "rotated next ∈ pool");
        assert!(p.prev_a.is_empty(), "rotation → prev_a=∅");
    }

    /// **mb_011-B** — Feasible but every cell ICE-masked → `Miss`.
    /// Caller routes around via the unrestricted memo instead of
    /// emitting known-unfulfillable cells.
    #[test]
    fn resolve_all_masked_is_miss() {
        let h_all = [h("h1"), h("h2")];
        let c1: Cell = (h("h1"), CapacityType::Spot);
        let c2: Cell = (h("h1"), CapacityType::Od);
        let masked: HashSet<Cell> = [c1.clone(), c2.clone()].into();
        let pool = [&h_all[0], &h_all[1]];
        // Force `prev.h=Some(h1)` so try_solve targets h1 (whose cells
        // are masked) regardless of which the seed would draw.
        let out = resolve_h_explore(pin(Some(h("h1"))), 0, 0, &ctx(&pool, &masked), |_, _| {
            feasible(vec![c1.clone(), c2.clone()])
        });
        match out {
            HExploreOutcome::Miss { pin: p } => {
                assert_eq!(
                    p.h.as_deref(),
                    Some("h2"),
                    "all-masked → rotate to pool\\{{h1}} = {{h2}}"
                );
            }
            HExploreOutcome::Hit { .. } => {
                panic!("Feasible-but-all-masked → Miss (route around), not Hit")
            }
        }
        // Control: one cell unmasked → Hit.
        let masked: HashSet<Cell> = [c1.clone()].into();
        let out = resolve_h_explore(pin(Some(h("h1"))), 0, 0, &ctx(&pool, &masked), |_, _| {
            feasible(vec![c1.clone(), c2.clone()])
        });
        assert!(
            matches!(out, HExploreOutcome::Hit { .. }),
            "one unmasked cell → Hit"
        );
    }

    /// **R17B0 / bug_001 falsification** — `Miss` carries `pin.prev_a`.
    /// At `|pool|=1` (singleton fixed-point), Feasible-all-masked →
    /// `pin.prev_a = m.a.cells` (fresh A'); BestEffort → `pin.prev_a =
    /// prev.prev_a` (preserve). Rotation (`|pool|>1`) → `∅`. Pre-R17B0:
    /// `Miss{next}` carried no cells; the `_ =>` arm dropped `m`; the
    /// caller's `next==prev_pin` reconstruction couldn't recover it.
    #[test]
    fn miss_feasible_all_masked_carries_cells() {
        let h0 = h("h0");
        let c_spot: Cell = (h0.clone(), CapacityType::Spot);
        let c_od: Cell = (h0.clone(), CapacityType::Od);
        let masked: HashSet<Cell> = [c_spot.clone(), c_od.clone()].into();
        let pool = [&h0];
        // (1) Feasible-all-masked, singleton: fresh A'={spot} carried.
        let prev = HExplorePin {
            h: Some(h0.clone()),
            prev_a: HashSet::new(),
        };
        let out = resolve_h_explore(prev, 0, 0, &ctx(&pool, &masked), |_, _| {
            feasible(vec![c_spot.clone()])
        });
        let HExploreOutcome::Miss { pin: p } = &out else {
            panic!("Feasible-all-masked → Miss")
        };
        assert_eq!(
            *out.pin(),
            HExplorePin {
                h: Some(h0.clone()),
                prev_a: [c_spot.clone()].into(),
            },
            "Feasible-all-masked at |pool|=1 → Miss.pin.prev_a = fresh \
             m.a.cells={{spot}}. bug_001 @ 36804895: `_ =>` dropped m; \
             Miss carried only `next`."
        );
        assert_eq!(p, out.pin(), ".pin() helper agrees with arm field");
        // (2) BestEffort, singleton: preserve prev.prev_a={od}.
        let prev = HExplorePin {
            h: Some(h0.clone()),
            prev_a: [c_od.clone()].into(),
        };
        let out = resolve_h_explore(prev, 0, 0, &ctx(&pool, &masked), |_, _| besteffort());
        assert_eq!(
            *out.pin(),
            HExplorePin {
                h: Some(h0.clone()),
                prev_a: [c_od.clone()].into(),
            },
            "BestEffort at |pool|=1 → Miss.pin.prev_a = prev.prev_a \
             (preserved — solve produced no A')"
        );
        // (3) |pool|=2 → rotation → prev_a=∅ (both Miss arms).
        let h1 = h("h1");
        let pool2 = [&h0, &h1];
        for (lbl, ts) in [
            ("Feasible-all-masked", feasible(vec![c_spot.clone()])),
            ("BestEffort", besteffort()),
        ] {
            let prev = HExplorePin {
                h: Some(h0.clone()),
                prev_a: [c_od.clone()].into(),
            };
            let out = resolve_h_explore(prev, 0, 0, &ctx(&pool2, &masked), |_, _| ts);
            assert_eq!(
                *out.pin(),
                HExplorePin {
                    h: Some(h1.clone()),
                    prev_a: HashSet::new(),
                },
                "{lbl} at |pool|=2 → rotation to h1 → prev_a=∅ (new h's \
                 first solve uses τ_enter)"
            );
        }
    }

    /// **mb_001 trajectory falsification (R7B0)** — `Miss^n` rotation
    /// MUST cover the full pool within `|pool|` consecutive misses.
    /// Pre-R7B0: rotation `pool.iter().filter(|x|x≠h).choose(pin_rng)`
    /// with `pin_rng` fresh-seeded each call AND not consumed on the
    /// `Some(h)=>h` arm → constant index `k` into `sorted(pool\{h})`
    /// → range `{p_k,p_{k+1}}` → 2-cycle attractor. r6's rotation
    /// tests use `|pool|=2` (where 2-cycle = full coverage); this
    /// drives `|pool|=5` so the 2-cycle is observable.
    #[test]
    fn resolve_miss_trajectory_covers_pool() {
        let h_all = [h("p0"), h("p1"), h("p2"), h("p3"), h("p4"), h("p5")];
        let masked = HashSet::new();
        // |pool| = 5 (excludes p0 = the cheapest). All-infeasible
        // (besteffort()) → every iteration is a Miss → rotation.
        let pool = [&h_all[1], &h_all[2], &h_all[3], &h_all[4], &h_all[5]];
        let want: HashSet<HwClassName> = pool.iter().map(|h| (**h).clone()).collect();
        let cx = ctx(&pool, &masked);

        // (a) stable pool, prev=None: drive 2·|pool| misses, collect
        // every h_tried. Round-robin covers pool in exactly |pool|
        // steps; pre-R7B0 |seen|≤4 (iter-0's seeded draw contributes
        // ≤2 + steady-state 2-cycle ≤2) → fails the equality.
        let mut seen = HashSet::new();
        let mut prev = pin(None);
        for _ in 0..10 {
            let (rec, ts) = recording(besteffort());
            let HExploreOutcome::Miss { pin: p } = resolve_h_explore(prev, 7, 0, &cx, ts) else {
                panic!("besteffort → Miss")
            };
            seen.insert(rec.take().expect("try_solve called"));
            prev = p;
        }
        assert_eq!(
            seen,
            want,
            "Miss^{{2·|pool|}} trajectory MUST visit every pool element \
             (round-robin). Pre-R7B0: pin_rng fresh-seeded + unconsumed \
             on Some(h)=>h → .choose() returns constant k → 2-cycle → \
             |seen|={}/5",
            seen.len()
        );

        // (b) prev ∈ h_all ∧ ∉ pool (the cheapest-excluded one):
        // pre-R7B0 filter `h ∈ h_all ∧ h ∉ in_a` ACCEPTS `p0` (in_a=∅)
        // → h_to_try=p0 ∉ pool → rotation .position() would miss. The
        // R7B0 filter `h ∈ pool` rejects → redraw from pool → joins
        // the round-robin cycle in ≤1 step.
        let mut seen = HashSet::new();
        let mut prev = pin(Some(h("p0")));
        for _ in 0..10 {
            let (rec, ts) = recording(besteffort());
            let HExploreOutcome::Miss { pin: p } = resolve_h_explore(prev, 7, 0, &cx, ts) else {
                panic!("besteffort → Miss")
            };
            seen.insert(rec.take().expect("try_solve called"));
            prev = p;
        }
        assert!(
            seen.is_superset(&want),
            "off-pool prev rejoins round-robin in ≤1 step → 2·|pool| \
             misses cover pool; got seen={seen:?}"
        );
    }

    /// **R7B0 filter** — `prev ∈ h_all ∧ ∉ pool` (e.g. became the
    /// cheapest, so excluded from `pool=H\{cheapest}`) MUST be
    /// rejected pre-solve and redrawn from `pool`. Pre-R7B0 filter
    /// `h ∈ h_all ∧ h ∉ in_a` accepts `h_off` (in_a=∅) → `h_to_try
    /// = h_off ∉ pool`.
    #[test]
    fn resolve_filter_rejects_off_pool_prev() {
        let h_all = [h("p0"), h("p1"), h("p2")];
        let masked = HashSet::new();
        // pool excludes p0 (the cheapest).
        let pool = [&h_all[1], &h_all[2]];
        let (rec, ts) = recording(besteffort());
        let _ = resolve_h_explore(pin(Some(h("p0"))), 7, 0, &ctx(&pool, &masked), ts);
        let tried = rec.take().expect("try_solve called");
        assert!(
            pool.iter().any(|p| **p == tried),
            "prev=p0 ∈ h_all ∧ ∉ pool → filter MUST reject and redraw \
             from pool; got h_tried={tried} (NOT in pool={{p1,p2}})"
        );
        assert_ne!(tried, h("p0"));
    }

    /// Singleton pool, infeasible → `Miss{next: Some(only)}` (round-
    /// robin fixed point). Equivalent to `None` since the pre-solve
    /// filter re-validates `∈ pool` next call and the singleton is
    /// the only redraw candidate anyway.
    #[test]
    fn resolve_singleton_pool_exhausted() {
        let h_all = [h("only")];
        let masked = HashSet::new();
        let pool = [&h_all[0]];
        let out = resolve_h_explore(pin(None), 5, 0, &ctx(&pool, &masked), |_, _| besteffort());
        match out.pin().h.as_deref() {
            Some(n) => assert_eq!(n, "only", "|pool|=1 round-robin → fixed point"),
            None => panic!("|pool|=1 → next=Some(only) fixed point; got None"),
        }
        assert!(
            matches!(out, HExploreOutcome::Miss { .. }),
            "BestEffort → Miss"
        );
        // And empty pool (prev=None) → Miss{pin: default} without
        // calling try_solve at all.
        let (rec, ts) = recording(feasible(vec![]));
        let out = resolve_h_explore(pin(None), 5, 0, &ctx(&[], &masked), ts);
        assert_eq!(*out.pin(), HExplorePin::default());
        assert!(matches!(out, HExploreOutcome::Miss { .. }));
        assert!(rec.take().is_none(), "empty pool → try_solve not called");
    }

    // ─── Algorithm-2 ladder tests ────────────────────────────────────────

    fn cfg() -> SlaConfig {
        SlaConfig {
            tiers: vec![Tier {
                name: "normal".into(),
                p50: None,
                p90: Some(1200.0),
                p99: None,
            }],
            default_tier: "normal".into(),
            probe: ProbeShape {
                cpu: 4.0,
                mem_per_core: 2 << 30,
                mem_base: 4 << 30,
                deadline_secs: 3600,
            },
            max_cores: 64.0,
            max_mem: 256 << 30,
            max_disk: 200 << 30,
            default_disk: 20 << 30,
            ..SlaConfig::test_default()
        }
    }

    fn fit(st: ExploreState) -> FittedParams {
        FittedParams {
            key: ModelKey {
                pname: "p".into(),
                system: "x".into(),
                tenant: "t".into(),
            },
            fit: DurationFit::Probe,
            mem: MemFit::Independent { p90: MemBytes(0) },
            disk_p90: None,
            sigma_resid: 0.2,
            log_residuals: Vec::new(),
            n_eff_ring: RingNEff(1.0),
            fit_df: FitDf(1.0),
            n_distinct_c: 1,
            sum_w: 1.0,
            span: 1.0,
            explore: st,
            t_min_ci: None,
            ci_computed_at: None,
            tier: None,
            hw_bias: Default::default(),
            alpha: crate::sla::alpha::UNIFORM,
            prior_source: None,
            is_fod: false,
        }
    }

    fn st(min: f64, max: f64, distinct: u8, sat: bool, wall: f64) -> ExploreState {
        ExploreState {
            distinct_c: distinct,
            min_c: RawCores(min),
            max_c: RawCores(max),
            saturated: sat,
            last_wall: WallSeconds(wall),
        }
    }

    #[test]
    fn empty_fit_returns_probe() {
        let d = next(None, &cfg(), &DrvHints::default());
        assert_eq!(d.c.0, 4.0);
        // mem = 4·2Gi + 4Gi = 12Gi
        assert_eq!(d.mem.0, 12 << 30);
        assert_eq!(d.disk.0, 20 << 30);
    }

    // r[verify sched.sla.explore-x4-first-bump]
    #[test]
    fn x4_bump_on_saturated_slow() {
        // One sample at c=4, saturated, wall=1500 > p90=1200 → bump to 16.
        let f = fit(st(4.0, 4.0, 1, true, 1500.0));
        let d = next(Some(&f), &cfg(), &DrvHints::default());
        assert_eq!(d.c.0, 16.0);
        // mem follows probe shape at the bumped c.
        assert_eq!(d.mem.0, 16 * (2 << 30) + (4 << 30));
    }

    // r[verify sched.sla.explore-saturation-gate]
    #[test]
    fn halve_on_unsaturated() {
        // c=8, NOT saturated → halve to 4.
        let f = fit(st(8.0, 8.0, 1, false, 1500.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 4.0);
        // Saturated but FAST (wall<p90) → also halve: it hit target with
        // headroom, so probe smaller to find the floor.
        let f = fit(st(8.0, 8.0, 1, true, 600.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 4.0);
    }

    // r[verify sched.sla.explore-freeze]
    #[test]
    fn probe_at_ceiling_walks_down_not_freeze() {
        // probe.cpu == max_cores: first sample lands min=max=64,
        // distinct_c=1. Without the `distinct_c >= 2` guard this is
        // frozen → re-emit 64 forever (the ceiling case never self-
        // heals: solve_tier's BestEffort fallback also returns
        // cap_c=max_cores for Probe fits, so span never widens).
        let f = fit(st(64.0, 64.0, 1, false, 100.0));
        assert_eq!(
            next(Some(&f), &cfg(), &DrvHints::default()).c.0,
            32.0,
            "single sample at ceiling → walk down, not freeze"
        );
        // probe.cpu == 1.0: same shape at the floor. (This case did
        // self-heal after 3 wasted builds via BestEffort→max_cores,
        // but it's still 3 wasted builds.)
        let f = fit(st(1.0, 1.0, 1, true, 1500.0));
        assert_eq!(
            next(Some(&f), &cfg(), &DrvHints::default()).c.0,
            4.0,
            "single sample at floor → ×4, not freeze"
        );
        // Two distinct samples that reached the wall → freeze (the
        // ladder DID walk there).
        let f = fit(st(32.0, 64.0, 2, true, 1500.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 64.0);
    }

    // r[verify sched.sla.explore-freeze]
    #[test]
    fn probe_at_boundary_gradient_into_wall_steps_away() {
        // The two cases `probe_at_ceiling_walks_down_not_freeze`
        // doesn't cover: gradient points INTO the boundary, so the
        // clamp eats the step. Before the opposite-direction fallback,
        // these re-emitted the boundary forever (distinct_c stuck at
        // 1 → frozen() never fires → solve gate never opens).

        // Ceiling, saturated+slow → ×4 clamps to 64 → step DOWN to 32.
        let f = fit(st(64.0, 64.0, 1, true, 1500.0));
        assert_eq!(
            next(Some(&f), &cfg(), &DrvHints::default()).c.0,
            32.0,
            "ceiling+saturated+slow: clamp ate ×4 → step ÷2 instead"
        );
        // Floor, unsaturated → ÷2 clamps to 1 → step UP to 4.
        let f = fit(st(1.0, 1.0, 1, false, 100.0));
        assert_eq!(
            next(Some(&f), &cfg(), &DrvHints::default()).c.0,
            4.0,
            "floor+unsaturated: clamp ate ÷2 → step ×4 instead"
        );
    }

    // r[verify sched.sla.explore-freeze]
    #[test]
    fn freeze_at_span4() {
        // span = 16/4 = 4 → frozen, re-emit max_c.
        let f = fit(st(4.0, 16.0, 2, true, 1500.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 16.0);
        // min_c hit floor (after walking) → frozen.
        let f = fit(st(1.0, 2.0, 2, false, 100.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 2.0);
    }

    #[test]
    fn x4_clamps_at_max_cores() {
        // c=32, saturated+slow → ×4=128, clamped to 64.
        let f = fit(st(32.0, 32.0, 1, true, 1500.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 64.0);
    }

    #[test]
    fn halve_floors_at_1() {
        let f = fit(st(2.0, 3.0, 2, false, 100.0));
        assert_eq!(next(Some(&f), &cfg(), &DrvHints::default()).c.0, 1.0);
    }

    #[test]
    fn disk_uses_fit_p90_when_present() {
        // disk_p90=None → cfg.default_disk.
        let f = fit(st(4.0, 4.0, 1, true, 1500.0));
        assert_eq!(
            next(Some(&f), &cfg(), &DrvHints::default()).disk.0,
            20 << 30
        );
        // disk_p90=Some → that value, even mid-ladder. Core-independent
        // scalar; no reason to re-climb DiskPressure on a fresh drv_hash.
        let mut f = fit(st(4.0, 4.0, 1, true, 1500.0));
        f.disk_p90 = Some(DiskBytes(75 << 30));
        assert_eq!(
            next(Some(&f), &cfg(), &DrvHints::default()).disk.0,
            75 << 30
        );
        // No fit → cfg.default_disk.
        assert_eq!(next(None, &cfg(), &DrvHints::default()).disk.0, 20 << 30);
    }

    #[test]
    fn mem_uses_fit_when_above_probe_shape() {
        // ÷2 path: c=32 unsaturated → c_down=16. Probe shape at 16 =
        // 16·2Gi + 4Gi = 36Gi. Observed Independent{p90:80Gi} ×
        // headroom(n_eff) must win (a fresh drv_hash would otherwise
        // OOM and re-climb floor.mem).
        let mut f = fit(st(32.0, 32.0, 1, false, 200.0));
        f.mem = MemFit::Independent {
            p90: MemBytes(80 << 30),
        };
        let d = next(Some(&f), &cfg(), &DrvHints::default());
        assert_eq!(d.c.0, 16.0);
        let want = ((80u64 << 30) as f64 * headroom(f.n_eff_ring)) as u64;
        assert_eq!(d.mem.0, want, "fit mem × headroom, not probe shape");
        // Independent{p90:0} sentinel → probe shape wins via .max().
        let f = fit(st(32.0, 32.0, 1, false, 200.0));
        let d = next(Some(&f), &cfg(), &DrvHints::default());
        assert_eq!(d.mem.0, 16 * (2 << 30) + (4 << 30));
    }

    #[test]
    fn tier_target_falls_back_to_p50() {
        // p50-only tier (legal config): before `Tier::binding_bound`,
        // `tier_p90` read `t.p90` only → None → 1200s default. With
        // `fit.tier=Some("bulk")` (p50=7200) and last_wall=1500:
        // 1500 < 7200 → ÷2 (correct); 1500 > 1200 → ×4 (the bug).
        let mut c = cfg();
        c.tiers = vec![
            Tier {
                name: "bulk".into(),
                p50: Some(7200.0),
                p90: None,
                p99: None,
            },
            Tier {
                name: "normal".into(),
                p50: None,
                p90: Some(1200.0),
                p99: None,
            },
        ];
        let mut f = fit(st(4.0, 4.0, 1, true, 1500.0));
        f.tier = Some("bulk".into());
        assert_eq!(
            next(Some(&f), &c, &DrvHints::default()).c.0,
            2.0,
            "1500s met bulk's p50=7200 → halve, not ×4"
        );
        // Control: same fit on the p90-bounded tier still ×4's.
        f.tier = Some("normal".into());
        assert_eq!(next(Some(&f), &c, &DrvHints::default()).c.0, 16.0);
    }

    #[test]
    fn feature_probe_overrides_default() {
        let mut c = cfg();
        c.feature_probes.insert(
            "kvm".into(),
            ProbeShape {
                cpu: 8.0,
                mem_per_core: 4 << 30,
                mem_base: 16 << 30,
                deadline_secs: 3600,
            },
        );
        let hints = DrvHints {
            required_features: vec!["kvm".into()],
            ..Default::default()
        };
        let d = next(None, &c, &hints);
        assert_eq!(d.c.0, 8.0);
        assert_eq!(d.mem.0, 8 * (4 << 30) + (16 << 30));
        // Feature not in feature_probes → fall back to default probe.
        let hints = DrvHints {
            required_features: vec!["big-parallel".into()],
            ..Default::default()
        };
        assert_eq!(next(None, &c, &hints).c.0, 4.0);
    }
}
