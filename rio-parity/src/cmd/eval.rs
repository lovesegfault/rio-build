//! `rio-parity eval` — build an eval set from a Hydra evaluation: the
//! job manifest, the drvPath fidelity report, the dependency closure,
//! and the packed derivation archive.

use clap::Args;

// TODO: grow this placeholder into the full argument surface (scope
// selection, output directory, S3 destination, politeness-budget
// overrides) when the eval-set orchestration lands; until then only
// `--hydra-eval` exists so the binary has a parseable subcommand.
/// Placeholder argument set for `rio-parity eval`.
#[derive(Debug, Args)]
pub struct EvalArgs {
    /// Hydra evaluation id (e.g. 1824219).
    #[arg(long)]
    pub hydra_eval: u64,
}

pub async fn run(args: EvalArgs) -> anyhow::Result<()> {
    anyhow::bail!(
        "rio-parity eval is not implemented yet (requested hydra eval {})",
        args.hydra_eval
    );
}
