//! `rio-parity run` — the parity campaign engine.
//!
//! Executes one parity campaign against one eval set: plan the in-scope
//! jobs, sweep cache.nixos.org for Hydra's per-job ground truth,
//! optionally warm upstream-built dependencies, submit batches to the
//! rio cluster, collect and classify outcomes, and render the report.
//! Campaign state is append-only JSONL on the pod volume (periodically
//! synced to S3) so an interrupted run can resume without repeating
//! terminal work.
//!
//! Submodules are wired incrementally; [`run`] gains the full stage
//! machine once every stage exists.

pub mod artifact;
pub mod batch;
pub mod classify;
pub mod collect;
pub mod evalset_input;
pub mod glob;
pub mod grpc;
pub mod hydra_truth;
pub mod model;
pub mod plan;
pub mod reader;
pub mod report;
pub mod spec;
pub mod state;
pub mod stderrparse;
pub mod submit;
pub mod submitter;
pub mod warm;
pub mod watchdog;

use std::path::PathBuf;

use clap::Args;

/// CLI arguments for `rio-parity run`.
#[derive(Debug, Args)]
pub struct RunArgs {
    /// Path to the campaign spec JSON (written by `xtask parity launch`).
    #[arg(long)]
    pub spec: PathBuf,
    /// Local state directory (pod emptyDir). Created if missing.
    #[arg(long, default_value = "./parity-state")]
    pub state_dir: PathBuf,
    /// Local directory containing the downloaded+untarred eval set.
    /// When absent the engine downloads it from S3 per the spec.
    #[arg(long)]
    pub eval_set_dir: Option<PathBuf>,
    /// Override the spec's job limit (smoke runs).
    #[arg(long)]
    pub limit: Option<usize>,
    /// Hard deadline (RFC3339). The engine renders an explicitly-partial
    /// report at the deadline.
    #[arg(long)]
    pub deadline: Option<String>,
    /// Allow running even when the spec does not carry a launch-time
    /// tenant-upstream assertion (the run is flagged low-confidence).
    #[arg(long, default_value_t = false)]
    pub allow_unverified_tenants: bool,
    /// Skip the S3 sync (local development).
    #[arg(long, default_value_t = false)]
    pub no_s3: bool,
}

/// Entry point for `rio-parity run`.
///
/// TODO: wire the stage state machine (plan → hydra-truth → warm →
/// submit/collect → report) once the individual stages exist; until then
/// the subcommand fails fast instead of pretending to run a campaign.
pub async fn run(_args: RunArgs) -> anyhow::Result<()> {
    anyhow::bail!("rio-parity run: campaign stages are not wired up yet")
}
