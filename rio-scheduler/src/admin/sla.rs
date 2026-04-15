//! `AdminService.{Set,List,Clear}SlaOverride` / `ResetSlaModel` /
//! `SlaStatus` handler bodies. ADR-023 phase-6.

use tonic::Status;

use rio_proto::types::{SlaCandidateRow, SlaExplainResponse, SlaOverride, SlaStatusResponse};

use crate::db::SlaOverrideRow;
use crate::sla::explain::ExplainResult;
use crate::sla::types::{DurationFit, FittedParams, MemFit};

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
        p50_secs: o.p50_secs,
        p90_secs: o.p90_secs,
        p99_secs: o.p99_secs,
        cores: o.cores,
        mem_bytes: o.mem_bytes,
        capacity_type: o.capacity_type.clone(),
        expires_at: o.expires_at_epoch,
        created_at: 0.0,
        created_by: o.created_by.clone(),
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
        p50_secs: r.p50_secs,
        p90_secs: r.p90_secs,
        p99_secs: r.p99_secs,
        cores: r.cores,
        mem_bytes: r.mem_bytes,
        capacity_type: r.capacity_type.clone(),
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
    let (mem_kind, mem_p90) = match &f.mem {
        MemFit::Coupled { .. } => ("Coupled", f.mem.at(f.fit.p_bar()).0),
        MemFit::Independent { p90 } => ("Independent", p90.0),
    };
    SlaStatusResponse {
        has_fit: true,
        fit_kind: fit_kind.into(),
        s,
        p,
        q,
        p_bar,
        mem_kind: mem_kind.into(),
        mem_p90_bytes: mem_p90,
        disk_p90_bytes: f.disk_p90.map(|d| d.0),
        sigma_resid: f.sigma_resid,
        n_eff: f.n_eff,
        span: f.span,
        tier: f.tier.clone(),
        active_override: active_override.map(row_to_proto),
        prior_source: f
            .prior_source
            .map(|p| p.as_str().to_string())
            .unwrap_or_default(),
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
