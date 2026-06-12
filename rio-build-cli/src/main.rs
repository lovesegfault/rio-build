//! `rio` — the native-protocol build client binary (ADR-024).
//!
//! `rio build <installable>...` behaves like nix-fast-build from the
//! outside; everything between eval and build runs the native
//! protocol. Evaluation itself happens in the eval-parent process
//! (`config.eval_parent`) spawned with the worker channel on fd 3;
//! this binary is the coordinator.
//!
//! Observability: tracing spans carry `component = "build-client"`;
//! logs are JSON by default (`RIO_LOG_FORMAT=pretty` for humans) on
//! stderr — stdout is the status-line surface. No Prometheus exporter:
//! the observability spec defines server-side metric surfaces only,
//! and a short-lived CLI has no scrape endpoint to register against.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};

use rio_build_cli::acks::ClusterAckTable;
use rio_build_cli::config::{Config, ConfigOverlay};
use rio_build_cli::coordinator::clients::Clients;
use rio_build_cli::coordinator::{Coordinator, CoordinatorOpts, OutcomeState};
use rio_build_cli::evalchan;

#[derive(Parser)]
#[command(name = "rio", version, about = "rio native-protocol build client")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Submit builds over the native protocol (ADR-024).
    Build(BuildArgs),
}

#[derive(Args)]
struct BuildArgs {
    /// Installables to evaluate and build (flake attrs).
    #[arg(value_name = "INSTALLABLE")]
    installables: Vec<String>,

    /// Reattach to a running build's event stream and render it.
    #[arg(long, value_name = "BUILD_ID", conflicts_with_all = ["cancel", "installables"])]
    attach: Option<String>,

    /// Cancel a running build.
    #[arg(long, value_name = "BUILD_ID", conflicts_with_all = ["attach", "installables"])]
    cancel: Option<String>,

    /// Materialize outputs into the client CAS after completion.
    #[arg(long)]
    fetch: bool,

    /// Symlink the (first) fetched output here. Implies --fetch.
    #[arg(long, value_name = "PATH")]
    out_link: Option<PathBuf>,

    /// Build import-from-derivation locally instead of remotely.
    /// Flag-gated fallback, default off; not wired yet.
    #[arg(long)]
    local_ifd: bool,

    /// On interrupt (Ctrl-C/SIGTERM), exit and leave the submitted
    /// builds running cluster-side instead of cancelling them; each
    /// build id is printed with its `--attach` reattach hint.
    #[arg(long)]
    detach: bool,

    /// Continue building independent derivations after a failure.
    #[arg(long)]
    keep_going: bool,

    #[command(flatten)]
    overlay: ConfigOverlay,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let _otel = rio_common::observability::init_tracing("build-client")?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    match cli.command {
        Command::Build(args) => runtime.block_on(run_build(args)),
    }
}

async fn run_build(args: BuildArgs) -> anyhow::Result<()> {
    let cfg: Config = rio_common::config::load("build", &args.overlay)?;
    cfg.validate()?;
    let token = cfg.tenant_token()?;
    let mut clients = Clients::connect(&cfg.scheduler_addr, &cfg.store_addr, token.clone())
        .await
        .context("connecting to cluster")?;

    if let Some(id) = &args.cancel {
        let cancelled = rio_build_cli::coordinator::cancel_build(&mut clients, id).await?;
        if cancelled {
            println!("cancelled {id}");
            return Ok(());
        }
        bail!("build {id} not found or already terminal");
    }

    let cas_root = cfg.cas_root();
    std::fs::create_dir_all(&cas_root)
        .with_context(|| format!("creating CAS root {}", cas_root.display()))?;

    if let Some(id) = &args.attach {
        let outcome = rio_build_cli::coordinator::attach_build(&mut clients, id, 0, true).await?;
        return finish(
            vec![outcome],
            args.fetch || args.out_link.is_some(),
            args.out_link,
            &mut clients,
            &cas_root,
        )
        .await;
    }

    if args.installables.is_empty() {
        bail!("nothing to build: pass at least one installable (or --attach/--cancel)");
    }
    let Some(eval_parent) = &cfg.eval_parent else {
        bail!(
            "config `eval_parent` is not set — `rio build <installable>` needs the eval-parent \
             binary. --attach and --cancel work without it."
        );
    };

    let (chan, mut child) = evalchan::spawn_eval_parent(eval_parent, &[])
        .with_context(|| format!("spawning eval parent {}", eval_parent.display()))?;

    let acks = std::sync::Arc::new(std::sync::Mutex::new(ClusterAckTable::open(
        &cas_root,
        cfg.ack_scope(token.as_deref()),
        std::time::Duration::from_secs(cfg.ack_ttl_secs),
    )));

    let mut coordinator = Coordinator {
        clients: clients.clone(),
        acks,
        cas_root: cas_root.clone(),
        opts: CoordinatorOpts {
            keep_going: args.keep_going,
            page_max_nodes: cfg.page_max_nodes,
            fetch: args.fetch || args.out_link.is_some(),
            out_link: args.out_link,
            local_ifd: args.local_ifd,
            detach_on_interrupt: args.detach,
            ..CoordinatorOpts::default()
        },
    };

    // The first SIGINT/SIGTERM cancels this invocation's builds (or
    // detaches under --detach); a second one stops waiting for the
    // cancel acknowledgements. The handler is only installed for the
    // submit path: interrupting --attach/--cancel keeps the default
    // disposition (exit, never cancel).
    let (sig_tx, sig_rx) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut int), Ok(mut term)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            // Registration failed: the default signal disposition stays
            // in effect (the process just dies on Ctrl-C).
            return;
        };
        loop {
            tokio::select! {
                _ = int.recv() => {}
                _ = term.recv() => {}
            }
            if sig_tx.send(()).is_err() {
                return;
            }
        }
    });

    let summary = coordinator.run(chan, args.installables, sig_rx).await?;
    // Reap the eval parent (it exits on Shutdown/EOF).
    let _ = child.wait().await;

    if summary.detached {
        // Outcome lines were already printed by the detach path.
        return Ok(());
    }
    let mut failed = false;
    for o in &summary.outcomes {
        match &o.state {
            OutcomeState::Completed { output_paths } => {
                println!("{}: built {}", o.attr, output_paths.join(" "));
                for f in &o.fetched {
                    println!("{}: fetched to {}", o.attr, f.display());
                }
            }
            OutcomeState::Failed { message } => {
                eprintln!("{}: FAILED: {message}", o.attr);
                failed = true;
            }
            OutcomeState::Cancelled { reason } => {
                eprintln!("{}: cancelled: {reason}", o.attr);
                failed = true;
            }
            OutcomeState::EvalFailed { message } => {
                eprintln!("{}: evaluation failed: {message}", o.attr);
                failed = true;
            }
            OutcomeState::Detached => {}
        }
    }
    if failed || summary.interrupted {
        std::process::exit(1);
    }
    Ok(())
}

/// Post-attach fetch handling (mirrors the in-run `--fetch` path).
async fn finish(
    outcomes: Vec<rio_build_cli::coordinator::BuildOutcome>,
    fetch: bool,
    out_link: Option<PathBuf>,
    clients: &mut Clients,
    cas_root: &std::path::Path,
) -> anyhow::Result<()> {
    let mut failed = false;
    for o in &outcomes {
        match &o.state {
            OutcomeState::Completed { output_paths } => {
                println!("build {}: completed {}", o.build_id, output_paths.join(" "));
                if fetch {
                    for (i, p) in output_paths.iter().enumerate() {
                        let dest = rio_build_cli::fetch::materialize(clients, cas_root, p).await?;
                        println!("fetched to {}", dest.display());
                        if i == 0
                            && let Some(link) = &out_link
                        {
                            rio_build_cli::fetch::out_link(link, &dest)?;
                        }
                    }
                }
            }
            other => {
                eprintln!("build {}: {other:?}", o.build_id);
                failed = true;
            }
        }
    }
    if failed {
        std::process::exit(1);
    }
    Ok(())
}
