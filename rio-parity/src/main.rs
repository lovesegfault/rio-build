//! rio-parity binary entry point.

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rio-parity", version, about = "nixpkgs-parity campaign engine")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

// EvalArgs is much larger than RunArgs, but this enum is built exactly
// once at startup and clap's derive cannot parse into a Box<EvalArgs>,
// so boxing the variant is not worth the indirection.
#[allow(clippy::large_enum_variant)]
#[derive(Subcommand)]
enum Cmd {
    /// Build an eval set (manifest, fidelity report, drv-closure
    /// archive) from a Hydra evaluation.
    Eval(rio_parity::cmd::eval::EvalArgs),
    /// Run a parity campaign against a previously built eval set.
    Run(rio_parity::run::RunArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Parse args first so `--help`/`--version`/usage errors print
    // plainly instead of being preceded by JSON log lines.
    let cli = Cli::parse();
    // JSON logs by default (RIO_LOG_FORMAT=pretty for humans), RUST_LOG
    // filtering, optional OTLP — same bootstrap every rio binary uses.
    let _otel_guard = rio_common::observability::init_tracing("parity")?;
    match cli.cmd {
        Cmd::Eval(args) => rio_parity::cmd::eval::run(args).await,
        Cmd::Run(args) => rio_parity::run::run(args).await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_command_debug_assert() {
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
