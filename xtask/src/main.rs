//! cargo-xtask: one-stop developer tooling for rio-build.
//!
//! Invoke via the workspace alias: `cargo xtask <cmd>`.

// Raw println!/eprintln! scroll the terminal past MultiProgress's
// tracked bottom, freezing a copy of active progress bars in scrollback.
// Use tracing::info!() or ui::suspend(|| println!(...)) instead.
// Allowed where no bars are active (e.g. --list output, pre-init).
#![deny(clippy::print_stdout, clippy::print_stderr)]

use anyhow::Result;
use clap::{Parser, Subcommand};
use clap_verbosity_flag::{Verbosity, WarnLevel};
use human_panic::setup_panic;

mod config;
mod fuzz;
mod git;
mod helm;
mod k8s;
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
    /// Kubernetes deploy (--provider {k3s,eks}).
    K8s(k8s::K8sArgs),
}

fn main() -> std::process::ExitCode {
    setup_panic!();

    // SAFETY: single-threaded — tokio runtime hasn't started yet.
    unsafe { sh::init_env() };

    // aws-sdk + kube both pull rustls with different crypto feature flags;
    // install the provider early so the first TLS use doesn't panic.
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
        Cmd::Regen { which } => regen::run(which, &cfg).await,
        Cmd::Mutants => mutants::run(),
        Cmd::Fuzz(args) => fuzz::run(args),
        Cmd::NewMigration(args) => migration::run(args),
        Cmd::K8s(args) => k8s::run(args, &cfg).await,
    }
}
