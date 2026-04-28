//! `AdminService.{Set,List,Clear}SlaOverride` / `ResetSlaModel` /
//! `SlaStatus` handler bodies. ADR-023 phase-6.

use tonic::Status;

use rio_proto::types::{SlaCandidateRow, SlaExplainResponse, SlaOverride, SlaStatusResponse};

use crate::db::SlaOverrideRow;
use crate::sla::explain::ExplainResult;
use crate::sla::types::{DurationFit, FittedParams, MemFit, RawCores, RefSeconds};

/// proto → row. `id`/`created_at` are server-assigned; ignore the
/// request's values.
pub(super) fn row_from_proto(o: &SlaOverride) -> Result<SlaOverrideRow, Status> {
    if o.pname.is_empty() {
        return Err(Status::invalid_argument("pname is required"));
    }
    Ok(SlaOverrideRow {
        id: 0,
        pname: o.pname.clone(),
        system: o.system.clone(),
        tenant: o.tenant.clone(),
        cluster: o.cluster.clone(),
        tier: o.tier.clone(),
        cores: o.cores,
        mem_bytes: o.mem_bytes,
        expires_at: o.expires_at_epoch,
        created_at: 0.0,
        created_by: o.created_by.clone(),
        ..Default::default()
    })
}

/// row → proto. Inverse of [`row_from_proto`] with server-populated
/// `id`/`created_at` round-tripped.
pub(super) fn row_to_proto(r: &SlaOverrideRow) -> SlaOverride {
    SlaOverride {
        id: r.id,
        pname: r.pname.clone(),
        system: r.system.clone(),
        tenant: r.tenant.clone(),
        cluster: r.cluster.clone(),
        tier: r.tier.clone(),
        cores: r.cores,
        mem_bytes: r.mem_bytes,
        expires_at_epoch: r.expires_at,
        created_at_epoch: r.created_at,
        created_by: r.created_by.clone(),
    }
}

/// `FittedParams` → `SlaStatusResponse` projection. Stringly-typed
/// `fit_kind`/`mem_kind` so the CLI doesn't track every variant.
pub(super) fn status_from_fit(
    fit: Option<&FittedParams>,
    active_override: Option<&SlaOverrideRow>,
) -> SlaStatusResponse {
    let Some(f) = fit else {
        return SlaStatusResponse {
            has_fit: false,
            active_override: active_override.map(row_to_proto),
            ..Default::default()
        };
    };
    let (fit_kind, p_bar) = match &f.fit {
        DurationFit::Probe => ("Probe", f64::INFINITY),
        DurationFit::Amdahl { .. } => ("Amdahl", f64::INFINITY),
        DurationFit::Capped { p_bar, .. } => ("Capped", p_bar.0),
        DurationFit::Usl { p_bar, .. } => ("Usl", p_bar.0),
    };
    let (s, p, q) = f.fit.spq();
    // `MemFit::Coupled` evaluated at p̄=∞ (Probe/Amdahl) is
    // `(a + b·∞.ln()).exp() as u64` → saturates to u64::MAX (18 EB).
    // Amdahl+Coupled is the test-fixture default; the export-corpus path
    // already guards with a 0.0 sentinel — match that convention here.
    let (mem_kind, mem_p90) = match &f.mem {
        MemFit::Coupled { .. } => (
            "Coupled",
            if p_bar.is_finite() {
                f.mem.at(RawCores(p_bar)).0
            } else {
                0
            },
        ),
        MemFit::Independent { p90 } => ("Independent", p90.0),
    };
    SlaStatusResponse {
        has_fit: true,
        fit_kind: fit_kind.into(),
        // Same 0.0 sentinel for non-finite f64 as `p_bar`/`mem_p90`:
        // `DurationFit::Probe.spq()` is `(∞, 0, 0)`; protojson encodes
        // ∞ as the string `"Infinity"`, serde_json as `null` — either
        // breaks the all-numeric output shape.
        s: if s.is_finite() { s } else { 0.0 },
        p,
        q,
        p_bar: if p_bar.is_finite() { p_bar } else { 0.0 },
        mem_kind: mem_kind.into(),
        mem_p90_bytes: mem_p90,
        disk_p90_bytes: f.disk_p90.map(|d| d.0),
        sigma_resid: f.sigma_resid,
        // proto field stays scalar; report ring n_eff (the operator-
        // facing "how many samples does this key have"). `fit_df` is
        // surfaced via `sla explain`'s `fit_summary`.
        n_eff: f.n_eff_ring.0,
        span: f.span,
        tier: f.tier.clone(),
        active_override: active_override.map(row_to_proto),
        prior_source: f
            .prior_source
            .map(|p| p.as_str().to_string())
            .unwrap_or_default(),
    }
}

/// `SlaStatusResponse` → `DurationFit`. Inverse of `status_from_fit`'s
/// fit projection (mem/disk/stats fields ignored). `None` for `Probe`
/// (no curve) so callers can `let Some(fit) = … else { skip }`.
///
/// Cross-crate consumers (xtask gate_b, dashboard) reconstruct the
/// typed fit via this then call `fit.t_at(c)` — do NOT re-derive
/// `s + p/c [+ q·c]` (bug_032: misses the p̄ clamp on Capped/Usl).
///
/// Panics on an unknown `fit_kind` so a new [`DurationFit`] variant
/// compile-errors at `status_from_fit` AND fails here — both
/// directions forced.
pub fn duration_fit_from_status(r: &SlaStatusResponse) -> Option<DurationFit> {
    // 0.0 sentinel ⇔ ∞ (status_from_fit:97 — protojson can't encode ∞).
    let p_bar = if r.p_bar > 0.0 {
        RawCores(r.p_bar)
    } else {
        RawCores(f64::INFINITY)
    };
    match r.fit_kind.as_str() {
        "Probe" => None,
        "Amdahl" => Some(DurationFit::Amdahl {
            s: RefSeconds(r.s),
            p: RefSeconds(r.p),
        }),
        "Capped" => Some(DurationFit::Capped {
            s: RefSeconds(r.s),
            p: RefSeconds(r.p),
            p_bar,
        }),
        "Usl" => Some(DurationFit::Usl {
            s: RefSeconds(r.s),
            p: RefSeconds(r.p),
            q: r.q,
            p_bar,
        }),
        other => panic!("unknown fit_kind {other:?}; update duration_fit_from_status"),
    }
}

/// [`ExplainResult`] → proto. The CLI renders the candidate table; the
/// dashboard (phase-8) gets the same shape over gRPC-Web.
pub(super) fn explain_to_proto(r: &ExplainResult) -> SlaExplainResponse {
    SlaExplainResponse {
        fit_summary: r.fit_summary.clone(),
        prior_source: r.prior_source.clone(),
        override_applied: r.override_applied.clone(),
        candidates: r
            .candidates
            .iter()
            .map(|c| SlaCandidateRow {
                tier: c.tier.clone(),
                c_star: c.c_star,
                mem_bytes: c.mem,
                binding_constraint: c.binding_constraint.clone(),
                feasible: c.feasible,
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sla::types::{ExploreState, FitDf, ModelKey, RingNEff, WallSeconds};

    fn amdahl_coupled() -> FittedParams {
        FittedParams {
            key: ModelKey {
                pname: "x".into(),
                system: "x86_64-linux".into(),
                tenant: "t".into(),
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
            n_eff_ring: RingNEff(8.0),
            fit_df: FitDf(8.0),
            n_distinct_c: 5,
            sum_w: 10.0,
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
            hw_bias: Default::default(),
            alpha: crate::sla::alpha::UNIFORM,
            prior_source: None,
            is_fod: false,
        }
    }

    /// Regression: Amdahl/Probe → p̄=∞; `MemFit::Coupled` evaluated at ∞
    /// is `(a + b·∞.ln()).exp() as u64` → saturates to u64::MAX (18 EB).
    /// Amdahl+Coupled is the test-fixture default — operator-facing
    /// garbage for a common shape. Now matches the export-corpus path's
    /// 0-sentinel guard.
    #[test]
    fn status_from_fit_amdahl_coupled_finite_mem() {
        let r = status_from_fit(Some(&amdahl_coupled()), None);
        assert_eq!(
            r.mem_p90_bytes, 0,
            "p̄=∞ + Coupled → 0 sentinel, not u64::MAX"
        );
        assert_eq!(
            r.p_bar, 0.0,
            "p̄=∞ → 0.0 sentinel (protojson can't encode ∞)"
        );
        assert_eq!(r.mem_kind, "Coupled");
        assert_eq!(r.fit_kind, "Amdahl");
    }

    /// Regression for bug_037: `DurationFit::Probe.spq()` returns
    /// `(∞, 0, 0)`. Before the fix, `s` was written raw → serde_json
    /// emitted `"s": null` while `p_bar` (same ∞ source) was guarded to
    /// `0.0`. Probe is the universal cold-start state; every freshly-
    /// onboarded pname hit this.
    #[test]
    fn status_from_fit_probe_finite_s() {
        let f = FittedParams {
            fit: DurationFit::Probe,
            mem: MemFit::Independent {
                p90: crate::sla::types::MemBytes(100),
            },
            ..amdahl_coupled()
        };
        let r = status_from_fit(Some(&f), None);
        assert!(r.has_fit);
        assert_eq!(r.fit_kind, "Probe");
        assert_eq!(r.s, 0.0, "Probe spq() s=∞ → 0.0 sentinel");
        assert_eq!(r.p_bar, 0.0, "p̄=∞ → 0.0 sentinel (same convention)");
    }

    /// `duration_fit_from_status ∘ status_from_fit = id` on the fit
    /// projection (for non-Probe; Probe → None). Pins both the variant
    /// AND `t_at` at c below+above p̄=8 so the clamp direction is locked
    /// independently of which variant happened to round-trip.
    #[test]
    fn duration_fit_from_status_inverts_status_from_fit() {
        // Compile-time tripwire: a new DurationFit variant breaks this
        // match → forces a fixture below + a `status_from_fit` arm.
        fn _exhaustive(f: &DurationFit) {
            match f {
                DurationFit::Probe
                | DurationFit::Amdahl { .. }
                | DurationFit::Capped { .. }
                | DurationFit::Usl { .. } => (),
            }
        }
        let fixtures = [
            DurationFit::Amdahl {
                s: RefSeconds(30.0),
                p: RefSeconds(2000.0),
            },
            DurationFit::Capped {
                s: RefSeconds(30.0),
                p: RefSeconds(2000.0),
                p_bar: RawCores(8.0),
            },
            DurationFit::Usl {
                s: RefSeconds(30.0),
                p: RefSeconds(2000.0),
                q: 0.5,
                p_bar: RawCores(8.0),
            },
        ];
        for fit in &fixtures {
            let fp = FittedParams {
                fit: fit.clone(),
                ..amdahl_coupled()
            };
            let st = status_from_fit(Some(&fp), None);
            let rt = duration_fit_from_status(&st).expect("non-Probe → Some");
            // Variant identity (Amdahl's p̄=∞ round-trips via the 0.0
            // sentinel, so structural eq holds for Capped/Usl; Amdahl
            // discards p̄ on the way out so eq holds there too).
            match fit {
                DurationFit::Amdahl { .. } => assert!(matches!(rt, DurationFit::Amdahl { .. })),
                DurationFit::Capped { .. } => assert!(matches!(rt, DurationFit::Capped { .. })),
                DurationFit::Usl { .. } => assert!(matches!(rt, DurationFit::Usl { .. })),
                DurationFit::Probe => unreachable!(),
            }
            // t_at at c below + above p̄=8 — pins the clamp.
            for c in [4.0, 32.0] {
                assert_eq!(
                    rt.t_at(RawCores(c)),
                    fit.t_at(RawCores(c)),
                    "round-trip t_at({c}) for {fit:?}"
                );
            }
        }
        // Probe → None (gate_b structurally skips; previously the
        // open-coded form computed t_ref ≡ 0 → division blowup).
        let fp = FittedParams {
            fit: DurationFit::Probe,
            ..amdahl_coupled()
        };
        assert!(duration_fit_from_status(&status_from_fit(Some(&fp), None)).is_none());
    }

    /// `duration_fit_from_status` is total on `status_from_fit`'s output
    /// range — the `fit=None` arm (`has_fit=false`, `fit_kind=""`)
    /// returns `None`, not panic. bug_008: pre-fix the `""` hits the
    /// `other => panic!` arm.
    #[test]
    fn duration_fit_from_status_total_on_status_from_fit_range() {
        assert!(duration_fit_from_status(&status_from_fit(None, None)).is_none());
    }

    /// Finite p̄ (Capped) + Coupled → mem evaluated at p̄, not sentineled.
    #[test]
    fn status_from_fit_capped_coupled_evaluates_at_p_bar() {
        let mut f = amdahl_coupled();
        f.fit = DurationFit::Capped {
            s: RefSeconds(10.0),
            p: RefSeconds(400.0),
            p_bar: RawCores(8.0),
        };
        let r = status_from_fit(Some(&f), None);
        // (22.0 + 0.5·ln(8)).exp() ≈ e^23.04 ≈ 1.0e10 — finite, non-zero.
        assert!(r.mem_p90_bytes > 0 && r.mem_p90_bytes < u64::MAX);
        assert_eq!(r.p_bar, 8.0);
    }
}
