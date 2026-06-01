//! cargo-xtask: one-stop developer tooling for rio-build.
//!
//! Invoke via the workspace alias: `cargo xtask <cmd>`.

use anyhow::Result;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use human_panic::setup_panic;

mod aws;
mod config;
mod fuzz;
mod git;
mod helm;
mod k8s;
mod lint;
mod migration;
mod mutants;
mod regen;
mod replay;
mod sh;
mod ssh;
mod tofu;
mod ui;

use config::XtaskConfig;

#[derive(Parser)]
#[command(name = "xtask", about = "rio-build developer tooling", version)]
struct Cli {
    /// -v: show child output, -vv: debug, -vvv: trace. -q: errors only.
    ///
    /// WarnLevel default so -v bumps to Info (deps at info = our
    /// "verbose" threshold). xtask itself stays at info even at
    /// default via the filter override in ui::init.
    #[command(flatten)]
    verbose: Verbosity<WarnLevel>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Regenerate derived files (sqlx cache, CRDs, Cargo.json).
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
    /// Kubernetes deploy (--provider {k3s,eks}).
    K8s(k8s::K8sArgs),
    /// build-replay campaigns: make the cluster replay-ready (setup),
    /// record archives, launch campaigns, watch status, fetch reports
    /// (--check gates CI on the recorded regression gate), re-run single
    /// units (repro), run the engine locally against a local archive
    /// (dev); abort/cleanup are stubs for a later milestone (M2).
    Replay(replay::ReplayArgs),
    /// Workspace-level invariant checks ("lints that can't be lints").
    /// With no subcommand, runs every lint.
    Lint {
        #[command(subcommand)]
        which: Option<lint::Lint>,
    },
}

fn main() -> std::process::ExitCode {
    setup_panic!();

    // SAFETY: single-threaded — tokio runtime hasn't started yet.
    unsafe { sh::init_env() };

    // The workspace links a single rustls CryptoProvider (aws-lc-rs);
    // installing it early guards against a transitive dep re-enabling
    // `ring`, which would make the first TLS use panic.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = Cli::parse();
    ui::init(cli.verbose.tracing_level_filter());

    let result = XtaskConfig::load().and_then(|cfg| {
        let rt = tokio::runtime::Runtime::new()?;
        rt.block_on(run(cli.cmd, cfg))
    });

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(e) => {
            // `{:#}` = anyhow's chain format ("outer: middle: inner"),
            // no backtrace. For the full stack, re-run with -vvv and
            // RUST_BACKTRACE=1 — the tracing error! below will include it.
            tracing::error!("{e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

async fn run(cmd: Cmd, cfg: XtaskConfig) -> Result<()> {
    match cmd {
        Cmd::Regen { which } => regen::run(which).await,
        Cmd::Mutants => mutants::run(),
        Cmd::Fuzz(args) => fuzz::run(args),
        Cmd::NewMigration(args) => migration::run(args),
        Cmd::K8s(args) => k8s::run(args, &cfg).await,
        Cmd::Replay(args) => replay::run(args, &cfg).await,
        Cmd::Lint { which: Some(l) } => lint::run(&l),
        Cmd::Lint { which: None } => lint::run_all(),
    }
}
