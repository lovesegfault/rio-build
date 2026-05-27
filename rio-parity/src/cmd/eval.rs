//! `rio-parity eval` — build an eval set (design §5).

use clap::Args;

/// Placeholder argument set; the full surface lands with the
/// orchestration in a later change.
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
