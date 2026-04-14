// TODO(ADR-023): drop once Phase 2 lands
#![allow(dead_code)]

use rio_common::newtype;

newtype!(pub RawCores(f64): Display, Add, Sub, Mul<f64>, Div<f64>, Ord);
newtype!(pub RefSeconds(f64): Display, Add, Sub, Mul<f64>, Div<f64>, Ord);
newtype!(pub WallSeconds(f64): Display, Add, Sub, Mul<f64>, Div<f64>, Ord);
newtype!(pub MemBytes(u64): Display, Add, Ord);
newtype!(pub DiskBytes(u64): Display, Add, Ord);

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
            Self::Usl { s, p, q, p_bar } => RefSeconds(s.0 + p.0 / c.0.min(p_bar.0) + q * c.0),
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
    pub frozen: bool,
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
    pub n_eff: f64,
    pub span: f64,
    pub explore: ExploreState,
    pub t_min_ci: Option<(RefSeconds, RefSeconds)>,
    /// Unix-epoch seconds when `t_min_ci` was last bootstrapped. Feeds the
    /// debounce in `ingest::should_recompute_ci`. `None` ⇔
    /// `t_min_ci.is_none()`.
    pub ci_computed_at: Option<f64>,
    pub tier: Option<String>,
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
