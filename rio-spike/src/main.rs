mod benchmark;
mod fuse_store;
mod overlay;
mod synthetic_db;
mod validate;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "rio-spike")]
#[command(about = "FUSE+overlay+sandbox platform validation spike")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Mount a passthrough FUSE filesystem serving files from a backing directory
    FuseMount {
        /// Path to the backing directory containing store paths
        #[arg(long)]
        backing_dir: PathBuf,

        /// Path where the FUSE filesystem will be mounted
        #[arg(long)]
        mount_point: PathBuf,

        /// Enable FUSE passthrough mode (Linux 6.9+, bypasses userspace for reads)
        #[arg(long)]
        passthrough: bool,
    },

    /// Set up an overlayfs with a lower directory and per-build upper layer
    OverlaySetup {
        /// Lower layer path (e.g., a FUSE mount point)
        #[arg(long)]
        lower: PathBuf,

        /// Base directory for upper/work/merged dirs
        #[arg(long)]
        base_dir: PathBuf,

        /// Build identifier (used to name overlay directories)
        #[arg(long, default_value = "test-build")]
        build_id: String,
    },

    /// Generate a synthetic Nix store SQLite database from nix path-info JSON
    SqliteGen {
        /// Output path for the SQLite database file
        #[arg(long)]
        output: PathBuf,

        /// Store path to generate DB for (will query nix path-info --json --recursive)
        #[arg(long)]
        store_path: String,
    },

    /// Run the full FUSE -> overlay -> sandbox -> build validation chain
    Validate {
        /// Run all validation checks including concurrent isolation and benchmarks
        #[arg(long)]
        all: bool,

        /// Path to backing directory with pre-populated store paths.
        /// If not provided, will auto-populate from the local Nix store.
        #[arg(long)]
        backing_dir: Option<PathBuf>,
    },

    /// Benchmark FUSE read latency vs direct filesystem reads
    Benchmark {
        /// Path to the backing directory containing test files
        #[arg(long)]
        backing_dir: PathBuf,

        /// Path where the FUSE filesystem is (or will be) mounted
        #[arg(long)]
        mount_point: PathBuf,

        /// Maximum number of concurrent reader threads
        #[arg(long, default_value = "16", value_parser = clap::builder::RangedU64ValueParser::<usize>::new().range(1..))]
        max_concurrency: usize,
    },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .json()
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::FuseMount {
            backing_dir,
            mount_point,
            passthrough,
        } => fuse_store::run_fuse_mount(&backing_dir, &mount_point, passthrough),

        Command::OverlaySetup {
            lower,
            base_dir,
            build_id,
        } => overlay::run_overlay_setup(&lower, &base_dir, &build_id),

        Command::SqliteGen { output, store_path } => {
            synthetic_db::run_sqlite_gen(&output, &store_path).await
        }

        Command::Validate { all, backing_dir } => validate::run_validate(all, backing_dir).await,

        Command::Benchmark {
            backing_dir,
            mount_point,
            max_concurrency,
        } => benchmark::run_benchmark(&backing_dir, &mount_point, max_concurrency),
    }
}
