//! `cargo xtask parity` — operator surface for the nixpkgs-parity campaign.
//!
//! The campaign engine itself is the in-cluster `rio-parity` Job; xtask
//! only verifies prerequisites, provisions the campaign tenants/Secrets,
//! applies the Jobs, and renders the S3 artifacts. The repro/abort/cleanup
//! subcommands are stubs for a later milestone (M2) so `--help` documents
//! the full campaign lifecycle.
//!
//! Operator note: chart-side enablement (the engine's CiliumNetworkPolicy
//! admissions + the campaign tenants' gateway build-policy defaults) comes
//! from deploying with `cargo xtask k8s up --deploy-parity`; see
//! `cargo xtask parity launch --help` for the redeploy warning that applies
//! while a campaign is running.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

pub mod jobs;
pub mod preflight;
pub mod s3;

// Some constants below (and the `s3` helpers) have no non-test users yet:
// the eval/launch/status/report implementations that consume them are
// landing next. Each `allow(dead_code)` comes off with its first user.

/// Campaign tenants (created by `parity launch` directly via rio-cli —
/// never via `k8s grant`, which unconditionally adds cache.nixos.org).
/// The same three names are seeded as gateway build-policy defaults by the
/// helm chart when `parity.enabled=true`
/// (infra/helm/rio-build/templates/gateway.yaml); the launch pre-flight
/// cross-checks the deployed gateway config against these constants so a
/// rename on either side fails loudly before anything is submitted.
#[allow(dead_code)]
pub const TENANT_LEAF: &str = "parity-leaf";
#[allow(dead_code)]
pub const TENANT_SELFHOSTED: &str = "parity-selfhosted";
#[allow(dead_code)]
pub const TENANT_WARM: &str = "parity-warm";

/// Namespace + ServiceAccount the campaign Jobs run under (created by
/// xtask, not the chart). Must stay in sync with the chart's
/// `parity.namespace` value and the `app.kubernetes.io/name: rio-parity`
/// pod label — the chart's CiliumNetworkPolicies admit the engine only
/// from that namespace+label pair — and with the parity IRSA trust
/// binding to `rio-parity:rio-parity` (infra/eks/parity.tf).
pub const NS_PARITY: &str = "rio-parity";
pub const SA_PARITY: &str = "rio-parity";

/// S3 prefix inside the chunk bucket; must match the parity IRSA policy
/// (infra/eks/parity.tf, object actions scoped to `parity/*`) and the
/// engine's layout — the eval CLI's default `--s3-prefix` and the
/// campaign spec's default `parity/campaigns` artifact prefix both live
/// under it.
#[allow(dead_code)]
pub const S3_PREFIX: &str = "parity";

/// GC retention (hours) for the campaign tenants — 30 days, passed to
/// `rio-cli create-tenant --gc-retention-hours` by `parity launch`.
#[allow(dead_code)]
pub const TENANT_RETENTION_HOURS: u32 = 720;

#[derive(Args)]
pub struct ParityArgs {
    #[command(subcommand)]
    cmd: ParityCmd,
}

#[derive(Subcommand)]
enum ParityCmd {
    /// Build (or reuse) an eval set: apply the parity-eval Job for one
    /// Hydra evaluation (writes under `parity/evals/<id>/…` in S3).
    Eval(EvalArgs),
    /// Pre-flight the cluster, provision campaign tenants/keys/Secrets,
    /// and apply the campaign Job.
    ///
    /// Requires the chart to have been deployed with `cargo xtask k8s up
    /// --deploy-parity`, and that requirement does not end at launch: any
    /// redeploy while the campaign is running must also pass
    /// --deploy-parity, because helm gets a full fresh value set on every
    /// upgrade and omitting the flag reverts parity.enabled to false
    /// (dropping the engine's network admissions and the campaign
    /// tenants' build-policy defaults) and rolls the gateway. The
    /// launch-time pre-flight cannot protect against a redeploy that
    /// happens after launch.
    Launch(LaunchArgs),
    /// Show campaign progress (progress.json from S3 + Job state).
    Status(StatusArgs),
    /// Download and render the campaign report (summary.md).
    Report(ReportArgs),
    /// Print (or run) the recorded repro command for one job. M2.
    Repro {
        /// Campaign id.
        campaign: String,
        /// Job name (manifest `job` field).
        job: String,
    },
    /// Abort a running campaign (delete the Job, keep S3 state). M2.
    Abort {
        /// Campaign id.
        campaign: String,
    },
    /// Remove campaign Job/Secrets; optionally delete the campaign
    /// tenants so GC reclaims their data after retention. M2.
    Cleanup {
        /// Campaign id.
        campaign: String,
        /// Also delete the three campaign tenants.
        #[arg(long)]
        delete_tenants: bool,
    },
}

// Placeholder arg structs — swapped for the real ones as the
// eval/launch/status/report implementations land (each lands by
// replacing exactly one of these and its `run` arm). Campaign-scoped
// commands take the campaign id positionally.
#[derive(Args)]
pub struct EvalArgs {}
#[derive(Args)]
pub struct LaunchArgs {}
#[derive(Args)]
pub struct StatusArgs {
    /// Campaign id.
    pub campaign: String,
}
#[derive(Args)]
pub struct ReportArgs {
    /// Campaign id.
    pub campaign: String,
}

pub async fn run(args: ParityArgs) -> Result<()> {
    match args.cmd {
        // Placeholder arms: each of the four campaign commands bails
        // until its implementation lands and replaces the arm.
        ParityCmd::Eval(_) => bail!("`cargo xtask parity eval` is not implemented yet"),
        ParityCmd::Launch(_) => bail!("`cargo xtask parity launch` is not implemented yet"),
        ParityCmd::Status(a) => bail!(
            "`cargo xtask parity status {}` is not implemented yet",
            a.campaign
        ),
        ParityCmd::Report(a) => bail!(
            "`cargo xtask parity report {}` is not implemented yet",
            a.campaign
        ),
        ParityCmd::Repro { .. } => not_yet("repro"),
        ParityCmd::Abort { .. } => not_yet("abort"),
        ParityCmd::Cleanup { .. } => not_yet("cleanup"),
    }
}

/// The M2 subcommands (repro/abort/cleanup) ship as stubs so the CLI
/// surface documents the full campaign lifecycle from the start, but
/// they fail loudly instead of pretending to work.
fn not_yet(what: &str) -> Result<()> {
    bail!("`cargo xtask parity {what}` is not yet implemented (planned for M2)")
}

#[cfg(test)]
mod tests {
    #[test]
    fn m2_stubs_bail_with_milestone_hint() {
        for cmd in ["repro", "abort", "cleanup"] {
            let err = super::not_yet(cmd).unwrap_err().to_string();
            assert!(err.contains(cmd) && err.contains("M2"), "{err}");
        }
    }

    #[test]
    fn campaign_tenant_names_are_distinct_and_prefixed() {
        let all = [
            super::TENANT_LEAF,
            super::TENANT_SELFHOSTED,
            super::TENANT_WARM,
        ];
        for t in all {
            assert!(t.starts_with("parity-"), "{t}");
        }
        assert_eq!(
            all.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3
        );
    }
}
