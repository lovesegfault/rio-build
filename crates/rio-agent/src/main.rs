//! Rio Agent
//!
//! Build agent node that executes Nix builds.
//! Phase 1: Single agent, no Raft coordination.

use anyhow::Result;
use camino::Utf8PathBuf;
use clap::Parser;
use rio_agent::{agent, grpc_server};

/// Rio Agent - Distributed Nix build agent
#[derive(Parser, Debug)]
#[command(name = "rio-agent")]
#[command(about = "Rio build agent node", long_about = None)]
struct Args {
    /// Address to listen on for gRPC
    #[arg(short, long, default_value = "0.0.0.0:50051")]
    listen: String,

    /// Data directory for agent state
    #[arg(short, long, default_value = "/var/lib/rio")]
    data_dir: Utf8PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    tracing::info!("Starting Rio agent on {}", args.listen);
    tracing::info!("Data directory: {}", args.data_dir);

    // Phase 1: Create single agent (no Raft)
    let agent = agent::Agent::new(args.data_dir).await?;

    // Start gRPC server
    grpc_server::serve(args.listen, agent).await?;

    Ok(())
}
