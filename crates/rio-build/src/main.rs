//! Rio Build CLI
//!
//! CLI client for submitting Nix builds to Rio agents.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use clap_serde_derive::ClapSerde;
use serde::Serialize;
use url::Url;

use rio_build::{client, config, evaluator, output_handler};

/// Rio Build - Distributed Nix build client
#[derive(ClapSerde, Serialize, Debug)]
#[command(name = "rio-build")]
#[command(about = "Submit Nix builds to Rio agents", long_about = None)]
struct Args {
    /// Nix file to build
    #[arg(value_name = "NIX_FILE")]
    nix_file: Utf8PathBuf,

    /// Seed agent URLs for cluster discovery
    ///
    /// Can be specified via:
    /// 1. CLI flag: --seed-agents http://agent1:50051 --seed-agents http://agent2:50051
    /// 2. Config file: ~/.config/rio/config.toml
    ///
    /// CLI values override config file values.
    #[arg(short, long)]
    #[serde(default)]
    seed_agents: Vec<Url>,
}

#[tokio::main]
async fn main() -> Result<()> {
    // Initialize tracing
    tracing_subscriber::fmt::init();

    // Load config file and merge with CLI args
    let args = load_config_and_args()?;

    // Validate
    if args.seed_agents.is_empty() {
        let config_path = config::default_config_path()?;
        anyhow::bail!(
            "No seed agents configured. Either:\n  \
             1. Add seed_agents to {}\n  \
             2. Use --seed-agents flag\n  \
             3. Run with --help for more info",
            config_path
        );
    }

    // Validate URLs
    for seed_url in &args.seed_agents {
        if seed_url.scheme() != "http" && seed_url.scheme() != "https" {
            anyhow::bail!(
                "Invalid URL scheme '{}' for seed agent: {}. Must be http:// or https://",
                seed_url.scheme(),
                seed_url
            );
        }
    }

    // 1. Evaluate the Nix file to get build info
    let build_info = evaluator::evaluate_build(&args.nix_file).await?;

    // 2. Discover cluster and connect to leader (Phase 2.6)
    tracing::info!(
        "Discovering cluster from {} seed agents",
        args.seed_agents.len()
    );
    let cluster_info = rio_build::cluster::discover_cluster(&args.seed_agents).await?;

    tracing::info!(
        "Cluster discovered: leader={} at {}",
        cluster_info.leader_id,
        cluster_info.leader_address
    );

    let mut client = client::RioClient::connect(&cluster_info.leader_address).await?;

    // 3. Submit the build and get a stream of updates
    let stream = client.submit_build(build_info, &cluster_info).await?;

    // 4. Handle the build stream (logs, outputs, completion)
    output_handler::handle_build_stream(stream).await?;

    Ok(())
}

/// Load config file and merge with CLI arguments
///
/// Priority: CLI args > config file > defaults
fn load_config_and_args() -> Result<Args> {
    let config_path = config::default_config_path()?;

    // Try to load config file
    let file_config = if let Some(contents) = config::load_config_file(&config_path)? {
        toml::from_str::<<Args as ClapSerde>::Opt>(&contents)
            .with_context(|| format!("Failed to parse config file: {}", config_path))?
    } else {
        // No config file, use defaults
        <Args as ClapSerde>::Opt::default()
    };

    // Merge with CLI args (CLI takes precedence)
    let args = Args::from(file_config).merge_clap();

    Ok(args)
}
