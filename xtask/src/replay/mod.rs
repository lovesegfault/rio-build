//! `cargo xtask replay` — operator surface for build-replay campaigns.
//!
//! The campaign engine itself is the in-cluster `rio-replay` Job; xtask
//! only verifies prerequisites, provisions the campaign tenants/Secrets,
//! applies the Jobs, and renders the S3 artifacts. The abort/cleanup
//! subcommands are stubs for a later milestone (M2) so `--help` documents
//! the full campaign lifecycle.
//!
//! Operator note: chart-side enablement (the engine's CiliumNetworkPolicy
//! admissions + the campaign tenants' gateway build-policy defaults) comes
//! from deploying with `cargo xtask k8s up --deploy-replay`; see
//! `cargo xtask replay launch --help` for the redeploy warning that applies
//! while a campaign is running.

use anyhow::{Result, bail};
use clap::{Args, Subcommand};

pub mod eval;
pub mod jobs;
pub mod launch;
pub mod preflight;
pub mod report;
pub mod repro;
pub mod s3;
pub mod status;

/// Campaign tenants (created by `replay launch` directly via rio-cli —
/// never via `k8s grant`, which unconditionally adds cache.nixos.org).
/// The same three names are seeded as gateway build-policy defaults by the
/// helm chart when `replay.enabled=true`
/// (infra/helm/rio-build/templates/gateway.yaml); the launch pre-flight
/// cross-checks the deployed gateway config against these constants so a
/// rename on either side fails loudly before anything is submitted.
pub const TENANT_LEAF: &str = "replay-leaf";
pub const TENANT_SELFHOSTED: &str = "replay-selfhosted";
pub const TENANT_WARM: &str = "replay-warm";

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
/// `replay.namespace` value and the `app.kubernetes.io/name: rio-replay`
/// pod label — the chart's CiliumNetworkPolicies admit the engine only
/// from that namespace+label pair — and with the replay IRSA trust
/// binding to `rio-replay:rio-replay` (infra/eks/replay.tf).
pub const NS_REPLAY: &str = "rio-replay";
pub const SA_REPLAY: &str = "rio-replay";

/// S3 prefix inside the chunk bucket; must match the replay IRSA policy
/// (infra/eks/replay.tf, object actions scoped to `replay/*`) and the
/// engine's layout — the eval CLI's default `--s3-prefix` and the
/// campaign spec's default `replay/campaigns` artifact prefix both live
/// under it.
pub const S3_PREFIX: &str = "replay";

/// GC retention (hours) for the campaign tenants — 30 days, passed to
/// `rio-cli create-tenant --gc-retention-hours` by `replay launch`.
pub const TENANT_RETENTION_HOURS: u32 = 720;

#[derive(Args)]
pub struct ReplayArgs {
    #[command(subcommand)]
    cmd: ReplayCmd,
}

#[derive(Subcommand)]
enum ReplayCmd {
    /// Create the evaluation-recorder Job (alias: eval): record (or
    /// reuse) a replay archive for one Hydra evaluation (publishes under
    /// `replay/archives/…` in S3).
    #[command(alias = "eval")]
    Record(eval::EvalArgs),
    /// Pre-flight the cluster, provision campaign tenants/keys/Secrets,
    /// and apply the campaign Job.
    ///
    /// Requires the chart to have been deployed with `cargo xtask k8s up
    /// --deploy-replay`, and that requirement does not end at launch: any
    /// redeploy while the campaign is running must also pass
    /// --deploy-replay, because helm gets a full fresh value set on every
    /// upgrade and omitting the flag reverts replay.enabled to false
    /// (dropping the engine's network admissions and the campaign
    /// tenants' build-policy defaults) and rolls the gateway. The
    /// launch-time pre-flight cannot protect against a redeploy that
    /// happens after launch.
    Launch(launch::LaunchArgs),
    /// Show campaign progress (progress.json from S3 + Job state).
    Status(status::StatusArgs),
    /// Download the campaign report (summary.md, plus progress.json and
    /// gate.json for context) into a local directory and print the summary;
    /// --check exits non-zero when the recorded regression gate tripped.
    Report(report::ReportArgs),
    /// Re-run exactly one derivation of a finished campaign as a fresh
    /// single-unit campaign — the engine-native invocation referenced by
    /// the `repro` field of results.jsonl records.
    Repro(repro::ReproArgs),
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

pub async fn run(args: ReplayArgs) -> Result<()> {
    match args.cmd {
        ReplayCmd::Record(a) => eval::run(a).await,
        ReplayCmd::Launch(a) => launch::run(a).await,
        ReplayCmd::Status(a) => status::run(a).await,
        ReplayCmd::Report(a) => report::run(a).await,
        ReplayCmd::Repro(a) => repro::run(a).await,
        ReplayCmd::Abort { .. } => not_yet("abort"),
        ReplayCmd::Cleanup { .. } => not_yet("cleanup"),
    }
}

/// The M2 subcommands (abort/cleanup) ship as stubs so the CLI surface
/// documents the full campaign lifecycle from the start, but they fail
/// loudly instead of pretending to work.
fn not_yet(what: &str) -> Result<()> {
    bail!("`cargo xtask replay {what}` is not yet implemented (planned for M2)")
}

#[cfg(test)]
mod tests {
    #[test]
    fn m2_stubs_bail_with_milestone_hint() {
        for cmd in ["abort", "cleanup"] {
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
            assert!(t.starts_with("replay-"), "{t}");
        }
        assert_eq!(
            all.iter().collect::<std::collections::BTreeSet<_>>().len(),
            3
        );
    }
}
