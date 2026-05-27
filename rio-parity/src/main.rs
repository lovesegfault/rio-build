//! rio-parity binary entry point.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rio-parity", version, about = "nixpkgs-parity campaign engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Build an eval set (design §5) from a Hydra evaluation.
    Eval(rio_parity::cmd::eval::EvalArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // JSON logs by default (RIO_LOG_FORMAT=pretty for humans), RUST_LOG
    // filtering, optional OTLP — same bootstrap every rio binary uses.
    let _otel_guard = rio_common::observability::init_tracing("parity")?;
    let cli = Cli::parse();
    match cli.cmd {
        Cmd::Eval(args) => rio_parity::cmd::eval::run(args).await,
    }
}
