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

pub mod eval;
pub mod jobs;
pub mod launch;
pub mod preflight;
pub mod report;
pub mod s3;
pub mod status;

/// Campaign tenants (created by `parity launch` directly via rio-cli —
/// never via `k8s grant`, which unconditionally adds cache.nixos.org).
/// The same three names are seeded as gateway build-policy defaults by the
/// helm chart when `parity.enabled=true`
/// (infra/helm/rio-build/templates/gateway.yaml); the launch pre-flight
/// cross-checks the deployed gateway config against these constants so a
/// rename on either side fails loudly before anything is submitted.
pub const TENANT_LEAF: &str = "parity-leaf";
pub const TENANT_SELFHOSTED: &str = "parity-selfhosted";
pub const TENANT_WARM: &str = "parity-warm";

/// Campaign-tenant matrix: (tenant, exact upstream URL set it must have,
/// expected `force_build_roots` in the deployed gateway build-policy).
/// The single source for tenant provisioning, the launch pre-flight, and
/// the tests, so the three can never drift; `keep_going` is true for
/// every campaign tenant and asserted separately by
/// [`preflight::check_build_policy`].
pub const TENANT_MATRIX: [(&str, &[&str], bool); 3] = [
    (TENANT_LEAF, &[preflight::CACHE_NIXOS_ORG], true),
    (TENANT_SELFHOSTED, &[], false),
    (TENANT_WARM, &[preflight::CACHE_NIXOS_ORG], false),
];

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
pub const S3_PREFIX: &str = "parity";

/// GC retention (hours) for the campaign tenants — 30 days, passed to
/// `rio-cli create-tenant --gc-retention-hours` by `parity launch`.
pub const TENANT_RETENTION_HOURS: u32 = 720;

#[derive(Args)]
pub struct ParityArgs {
    #[command(subcommand)]
    cmd: ParityCmd,
}

#[derive(Subcommand)]
enum ParityCmd {
    /// Record (or reuse) a replay archive: apply the parity-eval Job for
    /// one Hydra evaluation (publishes under `parity/archives/…` in S3).
    Eval(eval::EvalArgs),
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
    Launch(launch::LaunchArgs),
    /// Show campaign progress (progress.json from S3 + Job state).
    Status(status::StatusArgs),
    /// Download the campaign report (summary.md, plus progress.json for
    /// context) into a local directory and print the summary.
    Report(report::ReportArgs),
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

pub async fn run(args: ParityArgs) -> Result<()> {
    match args.cmd {
        ParityCmd::Eval(a) => eval::run(a).await,
        ParityCmd::Launch(a) => launch::run(a).await,
        ParityCmd::Status(a) => status::run(a).await,
        ParityCmd::Report(a) => report::run(a).await,
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
        // The matrix is the single source for provisioning and pre-flight;
        // it must cover each campaign tenant exactly once.
        let all: Vec<&str> = super::TENANT_MATRIX.iter().map(|(t, _, _)| *t).collect();
        assert_eq!(
            all,
            [
                super::TENANT_LEAF,
                super::TENANT_SELFHOSTED,
                super::TENANT_WARM,
            ]
        );
        for t in &all {
            assert!(t.starts_with("parity-"), "{t}");
        }
        assert_eq!(
            all.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3
        );
    }
}
