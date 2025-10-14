//! Rio Agent
//!
//! Build agent node that executes Nix builds in a Raft cluster.

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

    /// Join existing cluster at this seed URL (explicit mode)
    ///
    /// Explicitly joins the cluster instead of bootstrapping or auto-discovery.
    /// Example: --join http://agent1:50051
    #[arg(long, conflicts_with = "seeds")]
    join: Option<String>,

    /// Seed agent URLs for auto-discovery mode
    ///
    /// Try to join cluster from these seeds. If all fail, bootstrap new cluster with jitter.
    /// Comma-separated: --seeds agent1:50051,agent2:50051,agent3:50051
    #[arg(long, value_delimiter = ',', conflicts_with = "join")]
    seeds: Option<Vec<String>>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    let args = Args::parse();

    tracing::info!("Starting Rio agent on {}", args.listen);
    tracing::info!("Data directory: {}", args.data_dir);

    // Determine cluster mode
    let (agent, _h1, _h2, _h3) = if let Some(join_url) = args.join {
        // Explicit join mode
        tracing::info!("Joining cluster at: {}", join_url);
        agent::Agent::join(
            args.data_dir,
            args.listen.clone(),
            join_url,
            None,
            None,
            None,
        )
        .await?
    } else if let Some(seed_urls) = args.seeds {
        // Auto-discovery mode
        tracing::info!("Auto-discovery mode with {} seeds", seed_urls.len());
        agent::Agent::auto_join_or_bootstrap(
            args.data_dir,
            args.listen.clone(),
            seed_urls,
            None,
            None,
            None,
        )
        .await?
    } else {
        // Default: Bootstrap single-node cluster
        tracing::info!("Bootstrapping single-node Raft cluster");
        agent::Agent::bootstrap(args.data_dir, args.listen.clone(), None, None, None).await?
    };
    // Handles run in background, will be cleaned up on process exit

    // Start gRPC server
    grpc_server::serve(args.listen, agent).await?;

    Ok(())
}
