use std::collections::HashMap;

use rio_common::newtype;

newtype!(pub RawCores(f64): Display, Add, Sub, Mul<f64>, Div<f64>, Ord);
newtype!(pub RefSeconds(f64): Display, Add, Sub, Mul<f64>, Div<f64>, Ord);
newtype!(pub WallSeconds(f64): Display, Add, Sub, Mul<f64>, Div<f64>, Ord);
newtype!(pub MemBytes(u64): Display, Add, Ord);
newtype!(pub DiskBytes(u64): Display, Add, Ord);

// r[impl sched.sla.model-key-tenant-scoped]
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ModelKey {
    pub pname: String,
    pub system: String,
    pub tenant: String,
}

/// One code path, progressively unfrozen columns. ADR-023 §2.4 Model staging.
#[derive(Debug, Clone)]
pub enum DurationFit {
    Probe,
    Amdahl {
        s: RefSeconds,
        p: RefSeconds,
    },
    Capped {
        s: RefSeconds,
        p: RefSeconds,
        p_bar: RawCores,
    },
    Usl {
        s: RefSeconds,
        p: RefSeconds,
        q: f64,
        p_bar: RawCores,
    },
}

impl DurationFit {
    pub fn t_at(&self, c: RawCores) -> RefSeconds {
        match self {
            Self::Probe => RefSeconds(f64::INFINITY),
            Self::Amdahl { s, p } => RefSeconds(s.0 + p.0 / c.0),
            Self::Capped { s, p, p_bar } => RefSeconds(s.0 + p.0 / c.0.min(p_bar.0)),
            Self::Usl { s, p, q, p_bar } => {
                // q=0 ⇒ contention term is 0 regardless of c. Evaluating
                // `0.0 * ∞` (reachable: `c_opt()=∞` when q=0, `p_bar=∞`
                // from `observed_p_bar` when no sample is unsaturated)
                // is IEEE-754 NaN, which propagates through `t_min_ci`
                // → `reassign_tier` and silently breaks the Schmitt walk.
                let qc = if *q == 0.0 { 0.0 } else { q * c.0 };
                RefSeconds(s.0 + p.0 / c.0.min(p_bar.0) + qc)
            }
        }
    }
    pub fn c_opt(&self) -> RawCores {
        match self {
            Self::Usl { p, q, .. } if *q > 0.0 => RawCores((p.0 / q).sqrt()),
            _ => RawCores(f64::INFINITY),
        }
    }
    pub fn p_bar(&self) -> RawCores {
        match self {
            Self::Capped { p_bar, .. } | Self::Usl { p_bar, .. } => *p_bar,
            _ => RawCores(f64::INFINITY),
        }
    }
    /// Point estimate of T_min: T evaluated at `min(p̄, c_opt)`. For Amdahl
    /// (p̄=c_opt=∞) this is `S`; for Probe it is ∞.
    pub fn t_min(&self) -> RefSeconds {
        self.t_at(RawCores(self.p_bar().0.min(self.c_opt().0)))
    }
    /// (S, P, Q) tuple for the solve; Probe returns (inf, 0, 0).
    pub fn spq(&self) -> (f64, f64, f64) {
        match self {
            Self::Probe => (f64::INFINITY, 0.0, 0.0),
            Self::Amdahl { s, p } => (s.0, p.0, 0.0),
            Self::Capped { s, p, .. } => (s.0, p.0, 0.0),
            Self::Usl { s, p, q, .. } => (s.0, p.0, *q),
        }
    }
    /// Free-parameter count for the [`super::fit::z_q`] degrees-of-
    /// freedom calculation. `Probe` → 0 (no fit; z_q is unused on the
    /// probe path anyway).
    pub fn n_par(&self) -> u32 {
        match self {
            Self::Probe => 0,
            Self::Amdahl { .. } | Self::Capped { .. } => 2,
            Self::Usl { .. } => 3,
        }
    }
}

#[derive(Debug, Clone)]
pub enum MemFit {
    /// Koenker-Machado pseudo-R¹ ≥ 0.7 → log M = a + b·log c
    Coupled { a: f64, b: f64, r1: f64 },
    /// fallback: independent recency-weighted p90
    Independent { p90: MemBytes },
}

impl MemFit {
    pub fn at(&self, c: RawCores) -> MemBytes {
        match self {
            Self::Coupled { a, b, .. } => MemBytes((a + b * c.0.ln()).exp() as u64),
            Self::Independent { p90 } => *p90,
        }
    }
}

#[derive(Debug, Clone)]
pub struct ExploreState {
    pub distinct_c: u8,
    pub min_c: RawCores,
    pub max_c: RawCores,
    pub saturated: bool,
    pub last_wall: WallSeconds,
}

#[derive(Debug, Clone)]
pub struct FittedParams {
    pub key: ModelKey,
    pub fit: DurationFit,
    pub mem: MemFit,
    pub disk_p90: Option<DiskBytes>,
    pub sigma_resid: f64,
    /// Per-sample `ln(t_obs / t_pred)` from the most-recent refit. Feeds
    /// the MAD outlier gate (`ingest::is_outlier`) — kept on the cached
    /// fit so the NEXT tick can score new samples against the PREVIOUS
    /// fit's residual distribution without re-reading the ring. ≤32
    /// entries (ring-buffer cap), so the per-key cache cost is bounded.
    pub log_residuals: Vec<f64>,
    pub n_eff: f64,
    /// Count of distinct `cpu_limit` values in the ring (=
    /// [`super::ingest::AnchorRing::n_distinct_c`]). Feeds the
    /// [`super::fit::z_q`] degrees-of-freedom: post-convergence `n_eff`
    /// can be high while every fresh sample sits at the same c, leaving
    /// the design matrix near-rank-1 — `n_distinct_c` is the binding
    /// quantity then.
    pub n_distinct_c: u32,
    /// `Σw_i` over the ring. Feeds [`super::fit::z_q`]'s `√(1 + 1/Σw)`
    /// leverage term. NOT `n_eff`: under sub-unit weights `n_eff ≥ Σw`,
    /// so substituting would *narrow* the interval (anti-conservative).
    pub sum_w: f64,
    pub span: f64,
    pub explore: ExploreState,
    pub t_min_ci: Option<(RefSeconds, RefSeconds)>,
    /// Unix-epoch seconds when `t_min_ci` was last bootstrapped. Feeds the
    /// debounce in `ingest::should_recompute_ci`. `None` ⇔
    /// `t_min_ci.is_none()`.
    pub ci_computed_at: Option<f64>,
    pub tier: Option<String>,
    /// Per-hw_class residual bias: `median(wall_ref / T_ref(c))` over
    /// this key's samples on each hw_class. ADR-023 phase-10: the
    /// fleet-wide [`super::hw::HwTable`] factor is a CRC32 microbench;
    /// some pnames scale differently (e.g. mem-bandwidth-bound builds
    /// see less speedup on a fast-core class than the bench predicts).
    /// `bias[h] > 1.0` ⇔ this pname runs SLOWER on `h` than the fleet
    /// factor would imply. Default 1.0 if <3 samples on that hw_class.
    pub hw_bias: HashMap<String, f64>,
    /// Per-pname K=3 hardware mixture α ∈ Δ² — the effective scalar
    /// speedup on `h` is `α · factor[h]`. Fitted jointly with `T_ref(c)`
    /// by [`super::alpha::als_fit`]; defaults to [`super::alpha::UNIFORM`]
    /// (= phase-10 scalar behaviour) when the rank gate never passes.
    pub alpha: super::alpha::Alpha,
    /// Provenance of the prior partial-pooled into this fit
    /// ([`super::prior::partial_pool`]). `None` ⇔ priors disabled (no
    /// `[sla]` config) or this fit was never blended (test seeds).
    /// Surfaced via `SlaStatus.prior_source` so `rio-cli sla status`
    /// can show "this curve is 75% your seed table".
    pub prior_source: Option<super::prior::PriorSource>,
}

impl FittedParams {
    /// Student-t prediction-interval factor at quantile `q` for THIS
    /// fit's `(n_eff, n_distinct_c, n_par, Σw)`. Computed once per
    /// `(fit, q)` and threaded through [`super::quantile::quantile`] —
    /// cheap (one `inverse_cdf`) but constant across bisection steps so
    /// callers should hoist it.
    pub fn z_q(&self, q: f64) -> f64 {
        super::fit::z_q(
            q,
            self.n_eff,
            self.n_distinct_c,
            self.fit.n_par(),
            self.sum_w,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn duration_fit_copt() {
        let f = DurationFit::Usl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
            q: 0.05,
            p_bar: RawCores(24.0),
        };
        assert!((f.c_opt().0 - 200.0).abs() < 1e-6); // sqrt(2000/0.05)
    }
    #[test]
    fn t_at_c_amdahl() {
        let f = DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        };
        assert!((f.t_at(RawCores(10.0)).0 - 230.0).abs() < 1e-6);
    }
    // r[verify sched.sla.reassign-schmitt]
    #[test]
    fn usl_t_at_q_zero_c_infinity_is_finite() {
        // q=0, p_bar=∞ → c_opt()=∞ → t_min() evaluates t_at(∞). The
        // contention term must be 0, not 0·∞=NaN.
        let f = DurationFit::Usl {
            s: RefSeconds(10.0),
            p: RefSeconds(100.0),
            q: 0.0,
            p_bar: RawCores(f64::INFINITY),
        };
        let t = f.t_at(RawCores(f64::INFINITY));
        assert!(t.0.is_finite(), "t_at(∞) with q=0 must be finite, got {t}");
        assert_eq!(t.0, 10.0, "S + P/∞ + 0 = S");
        assert!(f.t_min().0.is_finite(), "t_min() must be finite");
        // Control: q>0 at finite c is unchanged.
        let g = DurationFit::Usl {
            s: RefSeconds(10.0),
            p: RefSeconds(100.0),
            q: 0.5,
            p_bar: RawCores(f64::INFINITY),
        };
        assert!((g.t_at(RawCores(4.0)).0 - 37.0).abs() < 1e-9);
    }
    #[test]
    fn t_at_c_capped_clamps_at_pbar() {
        let f = DurationFit::Capped {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
            p_bar: RawCores(8.0),
        };
        assert_eq!(f.t_at(RawCores(16.0)).0, f.t_at(RawCores(8.0)).0);
    }
}
