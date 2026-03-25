//! cargo-xtask: one-stop developer tooling for rio-build.
//!
//! Invoke via the workspace alias: `cargo xtask <cmd>`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use human_panic::setup_panic;

mod config;
mod dev;
mod eks;
mod fuzz;
mod git;
mod helm;
mod kube;
mod migration;
mod mutants;
mod regen;
mod sh;
mod ssh;
mod tofu;
mod ui;

use config::XtaskConfig;

#[derive(Parser)]
#[command(name = "xtask", about = "rio-build developer tooling", version)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Regenerate derived files (sqlx cache, CRDs, grafana, Cargo.json).
    /// With no subcommand, runs all regenerators in sequence.
    Regen {
        #[command(subcommand)]
        which: Option<regen::RegenCmd>,
    },
    /// Run cargo-mutants with the scoped config and print a summary.
    Mutants,
    /// Run a fuzz target (finds the right fuzz/ dir).
    Fuzz(fuzz::FuzzArgs),
    /// Create a new SQL migration and pin its checksum.
    NewMigration(migration::MigrationArgs),
    /// Local k8s (k3s/kind) deploy recipes.
    Dev {
        #[command(subcommand)]
        cmd: dev::DevCmd,
    },
    /// EKS deploy recipes (tofu + helm + ECR).
    Eks {
        #[command(subcommand)]
        cmd: eks::EksCmd,
    },
}

fn main() -> Result<()> {
    setup_panic!();

    // aws-sdk + kube both pull rustls with different crypto feature flags;
    // install the provider early so the first TLS use doesn't panic.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    ui::init_tracing();
    let cfg = XtaskConfig::load()?;

    // Only spin up tokio for commands that need it.
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(run(cli.cmd, cfg))
}

async fn run(cmd: Cmd, cfg: XtaskConfig) -> Result<()> {
    match cmd {
        Cmd::Regen { which } => regen::run(which, &cfg).await,
        Cmd::Mutants => mutants::run(),
        Cmd::Fuzz(args) => fuzz::run(args),
        Cmd::NewMigration(args) => migration::run(args),
        Cmd::Dev { cmd } => dev::run(cmd, &cfg).await,
        Cmd::Eks { cmd } => eks::run(cmd, &cfg).await,
    }
}
