//! Rio Build CLI
//!
//! CLI client for submitting Nix builds to Rio agents.

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::Parser;
use rio_build::{client, evaluator, output_handler};

/// Rio Build - Distributed Nix build client
#[derive(Parser, Debug)]
#[command(name = "rio-build")]
#[command(about = "Submit Nix builds to Rio agents", long_about = None)]
struct Args {
    /// Nix file to build
    #[arg(value_name = "NIX_FILE")]
    nix_file: Utf8PathBuf,

    /// Agent URL to connect to
    #[arg(short, long, default_value = "http://localhost:50051")]
    agent: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    // Phase 1: Simplified flow - direct connection to single agent
    // 1. Evaluate the Nix file to get build info
    let build_info = evaluator::evaluate_build(&args.nix_file).await?;

    // 2. Connect to the agent
    let mut client = client::RioClient::connect(&args.agent).await?;

    // 3. Submit the build and get a stream of updates
    let stream = client.submit_build(build_info).await?;

    // 4. Handle the build stream (logs, outputs, completion)
    output_handler::handle_build_stream(stream).await?;

    Ok(())
}
