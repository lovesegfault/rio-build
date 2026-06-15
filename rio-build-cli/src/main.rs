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
//! stderr. Stdout carries the final result paths only — every status
//! and log line goes to stderr via the renderer. No Prometheus exporter:
//! the observability spec defines server-side metric surfaces only,
//! and a short-lived CLI has no scrape endpoint to register against.

use std::path::PathBuf;

use anyhow::{Context, bail};
use clap::{Args, Parser, Subcommand};

use rio_build_cli::acks::ClusterAckTable;
use rio_build_cli::config::{Config, ConfigOverlay};
use rio_build_cli::coordinator::clients::Clients;
use rio_build_cli::coordinator::{Coordinator, CoordinatorOpts, FailureLogOpts, OutcomeState};
use rio_build_cli::{evalchan, render};

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
    /// Print a derivation's stored build log from the cluster
    /// (the native replacement for `nix log`, which cannot work over
    /// ssh-ng).
    Log(LogArgs),
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

    /// Evaluate a plain Nix file (or a directory containing
    /// default.nix) instead of a flake, nix-build style: installables
    /// are attr paths into its top-level value; with no installables
    /// the top-level value itself is the build root.
    #[arg(short = 'f', long, value_name = "PATH")]
    file: Option<PathBuf>,

    /// Pass the Nix expression EXPR as argument NAME to the file's
    /// top-level function (file mode only, like nix-build --arg).
    #[arg(long, num_args = 2, value_names = ["NAME", "EXPR"], requires = "file")]
    arg: Vec<String>,

    /// Pass the string VALUE as argument NAME to the file's top-level
    /// function (file mode only, like nix-build --argstr).
    #[arg(long, num_args = 2, value_names = ["NAME", "VALUE"], requires = "file")]
    argstr: Vec<String>,

    /// Add an entry to the angle-bracket lookup path (`<nixpkgs>`),
    /// taking precedence over NIX_PATH (file mode only).
    #[arg(short = 'I', long = "include", value_name = "PATH", requires = "file")]
    include: Vec<String>,

    /// Continue building independent derivations after a failure.
    #[arg(long)]
    keep_going: bool,

    /// When the build fails on a derivation that already failed in an
    /// earlier build, replay the last N lines of the original failure's
    /// log.
    #[arg(long, default_value = "20", value_name = "N")]
    log_lines: u32,

    /// Replay the original failure's full build log instead of a tail.
    #[arg(short = 'L', long)]
    print_build_logs: bool,

    /// Renderer: auto picks tty when stderr+stdin are a tty and
    /// TERM≠dumb, ci when GITHUB_ACTIONS is set, otherwise plain (one
    /// line per state edge, for scripts).
    #[arg(long, value_enum, default_value = "auto")]
    render: render::RenderMode,

    /// Don't wrap successful build logs in ::group:: folds (CI renderer).
    #[arg(long)]
    no_fold: bool,

    /// Seconds a build may produce no output before its tail is dumped
    /// and further output is streamed live (CI renderer). 0 disables.
    #[arg(long, default_value = "300", value_name = "SECS")]
    stall_timeout: u64,

    #[command(flatten)]
    overlay: ConfigOverlay,
}

/// Arguments for `rio log`: read a stored derivation log for a build
/// owned by this tenant. A data command — raw log bytes on stdout,
/// status and errors on stderr.
#[derive(Args)]
struct LogArgs {
    /// Full /nix/store/...-*.drv path whose log to print. Resolving an
    /// installable/attr to its derivation needs an evaluation and is not
    /// supported yet — pass the .drv path (e.g. from a failure message).
    #[arg(value_name = "DRV_PATH")]
    drv_path: String,

    /// Pin the build the log should belong to. Default: the most recent
    /// execution of the derivation among your own builds.
    #[arg(long, value_name = "BUILD_ID")]
    build: Option<String>,

    /// Pin a specific execution id (e.g. from a failure message).
    #[arg(long, value_name = "EXEC_ID")]
    exec: Option<String>,

    /// Print only the last N lines. Default: the full log.
    #[arg(long, value_name = "N")]
    log_lines: Option<u32>,

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
        Command::Log(args) => runtime.block_on(run_log(args)),
    }
}

/// Mirror the coordinator's resolved tracing level into the env value
/// the eval parent maps onto nix's own verbosity, so `RUST_LOG=debug`
/// also surfaces nix fetch/eval detail. `None` means leave nix at its
/// default: info matches it already, and OFF has no nix equivalent
/// worth forcing.
fn nix_verbosity_env(level: tracing::level_filters::LevelFilter) -> Option<&'static str> {
    use tracing::level_filters::LevelFilter;
    if level == LevelFilter::ERROR {
        Some("error")
    } else if level == LevelFilter::WARN {
        Some("warn")
    } else if level == LevelFilter::DEBUG {
        Some("debug")
    } else if level == LevelFilter::TRACE {
        Some("trace")
    } else {
        None
    }
}

/// `rio log <DRV_PATH> [--build] [--exec] [--log-lines]`: stream a
/// stored derivation log to stdout. Works without an eval parent
/// configured (like `--attach`/`--cancel`); ownership and tenancy are
/// enforced server-side — the scheduler only serves executions
/// attributable to this tenant's builds.
async fn run_log(args: LogArgs) -> anyhow::Result<()> {
    // Fail early on anything that isn't a .drv store path: attr →
    // derivation resolution would need an evaluation (eval parent), which
    // this data command deliberately avoids.
    match rio_nix::store_path::StorePath::parse(&args.drv_path) {
        Ok(sp) if sp.is_derivation() => {}
        _ => bail!(
            "{:?} is not a /nix/store/...-*.drv path. Resolving an installable to its \
             derivation needs an evaluation and is not supported yet — pass the .drv path \
             printed in the build output or failure message.",
            args.drv_path
        ),
    }

    let cfg: Config = rio_common::config::load("build", &args.overlay)?;
    cfg.validate()?;
    let token = cfg.tenant_token()?;
    let mut clients = Clients::connect(&cfg.scheduler_addr, &cfg.store_addr, token)
        .await
        .context("connecting to cluster")?;

    let req = rio_proto::scheduler::GetDerivationLogRequest {
        build_id: args.build.unwrap_or_default(),
        derivation_path: args.drv_path.clone(),
        exec_id: args.exec.unwrap_or_default(),
        tail_lines: args.log_lines.unwrap_or(0),
        since_line: 0,
    };
    let mut stream = clients
        .scheduler
        .get_derivation_log(clients.req(req)?)
        .await
        .map_err(|status| anyhow::anyhow!("{}", status.message()))?
        .into_inner();

    // Raw log bytes to stdout — this is the machine-readable surface;
    // status and errors stay on stderr.
    use std::io::Write;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut printed = 0usize;
    loop {
        match stream.message().await {
            Ok(Some(chunk)) => {
                for raw in &chunk.lines {
                    out.write_all(raw)?;
                    out.write_all(b"\n")?;
                    printed += 1;
                }
                if chunk.is_complete {
                    break;
                }
            }
            Ok(None) => break,
            Err(status) => bail!("log stream failed: {}", status.message()),
        }
    }
    out.flush()?;
    if printed == 0 {
        bail!(
            "no log available for {} (no execution recorded under your builds, the execution \
             produced no output, or the log has expired)",
            args.drv_path
        );
    }
    Ok(())
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

    let render_opts = render::RenderOpts {
        mode: args.render,
        no_fold: args.no_fold,
        stall_timeout: args.stall_timeout,
    };
    // High while a pager owns the terminal: Ctrl-C must reach the pager
    // (exits less follow mode), not abort builds behind its back.
    let pager_gate = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let failure_log = FailureLogOpts {
        log_lines: args.log_lines,
        print_build_logs: args.print_build_logs,
    };
    if let Some(id) = &args.attach {
        let (render, render_task) = render::spawn(render_opts, pager_gate);
        let outcome =
            rio_build_cli::coordinator::attach_build(&mut clients, id, render, failure_log).await?;
        let _ = render_task.await;
        return finish(
            vec![outcome],
            args.fetch || args.out_link.is_some(),
            args.out_link,
            &mut clients,
            &cas_root,
        )
        .await;
    }

    // File mode with zero installables is nix-build's "build the
    // file's top-level value" — eval_plan submits the empty attr path.
    if args.installables.is_empty() && args.file.is_none() {
        bail!("nothing to build: pass at least one installable, --file, or --attach/--cancel");
    }
    let Some(eval_parent) = &cfg.eval_parent else {
        bail!(
            "config `eval_parent` is not set — `rio build <installable>` needs the eval-parent \
             binary. --attach and --cancel work without it."
        );
    };

    let file_opts = FileEvalOpts {
        args: &args.arg,
        argstrs: &args.argstr,
        includes: &args.include,
    };
    let (parent_args, attrs) = eval_plan(
        &args.installables,
        args.file.as_deref(),
        file_opts,
        &cas_root,
    )?;
    // Under the TTY renderer, eval-parent stderr would land inside the
    // ephemeral region and corrupt it; pipe it and forward through the
    // renderer instead.
    let pipe_stderr = !matches!(render_opts.mode, render::RenderMode::Plain);
    // `current()` is the subscriber's global max-level hint, so a per-target
    // directive (`RUST_LOG=info,h2=trace`) raises nix verbosity too — accepted
    // coarseness for a debugging aid, not worth resolving per target.
    let nix_verbosity = nix_verbosity_env(tracing::level_filters::LevelFilter::current());
    let (chan, mut child) =
        evalchan::spawn_eval_parent(eval_parent, &parent_args, pipe_stderr, nix_verbosity)
            .with_context(|| format!("spawning eval parent {}", eval_parent.display()))?;

    let acks = std::sync::Arc::new(std::sync::Mutex::new(ClusterAckTable::open(
        &cas_root,
        cfg.ack_scope(token.as_deref()),
        std::time::Duration::from_secs(cfg.ack_ttl_secs),
    )));

    let (render, render_task) = render::spawn(render_opts, pager_gate.clone());
    if let Some(stderr) = child.stderr.take() {
        let r = render.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                r.note(line);
            }
        });
    }
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
            failure_log,
            ..CoordinatorOpts::default()
        },
        render,
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
            if pager_gate.load(std::sync::atomic::Ordering::Relaxed) {
                continue;
            }
            if sig_tx.send(()).is_err() {
                return;
            }
        }
    });

    let summary = coordinator.run(chan, attrs, sig_rx).await?;
    // Reap the eval parent (it exits on Shutdown/EOF).
    let _ = child.wait().await;
    // Stop the renderer (drains the channel, clears any live region)
    // before the result-path lines and the failure summaries below.
    drop(coordinator);
    let _ = render_task.await;

    if summary.detached {
        // Outcome lines were already printed by the detach path.
        return Ok(());
    }
    // r[impl bc.render.stdout-results]
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

/// nix-build-style evaluation flags forwarded to the eval parent in
/// file mode (clap rejects them without `--file`).
#[derive(Clone, Copy, Default)]
struct FileEvalOpts<'a> {
    /// `--arg NAME EXPR` pairs, flattened by clap (`num_args = 2`).
    args: &'a [String],
    /// `--argstr NAME VALUE` pairs, flattened by clap.
    argstrs: &'a [String],
    /// `-I` lookup-path entries.
    includes: &'a [String],
}

/// Derive the eval-parent argv + the WorkItem attrs from the
/// installables. File mode (`-f/--file`): installables are attr paths
/// into the file's top-level value; none means the top-level value
/// itself (the empty attr path, nix-build parity). Flake mode: every
/// installable is `ref#fragment` (or a bare ref = default attr); all
/// must share ONE flake ref — the eval parent locks one flake per
/// invocation (ADR-024: lock flake + fetch inputs once, pre-fork).
fn eval_plan(
    installables: &[String],
    file: Option<&std::path::Path>,
    file_opts: FileEvalOpts<'_>,
    cas_root: &std::path::Path,
) -> anyhow::Result<(Vec<String>, Vec<String>)> {
    let mut argv = vec!["--cas".to_string(), cas_root.display().to_string()];
    if let Some(f) = file {
        argv.push("--file".into());
        argv.push(f.display().to_string());
        for pair in file_opts.args.chunks(2) {
            argv.push("--arg".into());
            argv.extend(pair.iter().cloned());
        }
        for pair in file_opts.argstrs.chunks(2) {
            argv.push("--argstr".into());
            argv.extend(pair.iter().cloned());
        }
        for inc in file_opts.includes {
            argv.push("-I".into());
            argv.push(inc.clone());
        }
        let attrs = if installables.is_empty() {
            vec![String::new()]
        } else {
            installables.to_vec()
        };
        return Ok((argv, attrs));
    }
    let mut flake_ref: Option<String> = None;
    let mut attrs = Vec::with_capacity(installables.len());
    for inst in installables {
        let (r, frag) = match inst.split_once('#') {
            Some((r, frag)) => (r, frag),
            None => (inst.as_str(), ""),
        };
        let r = if r.is_empty() { "." } else { r };
        match &flake_ref {
            None => flake_ref = Some(r.to_string()),
            Some(prev) if prev == r => {}
            Some(prev) => bail!(
                "all installables must share one flake ref ({prev} vs {r}) — \
                 the eval parent locks one flake per invocation"
            ),
        }
        attrs.push(frag.to_string());
    }
    argv.push("--flake".into());
    argv.push(flake_ref.expect("installables is non-empty"));
    Ok((argv, attrs))
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

#[cfg(test)]
mod tests {
    use super::{Cli, Command, FileEvalOpts, eval_plan, nix_verbosity_env};
    use clap::Parser;
    use std::path::Path;
    use tracing::level_filters::LevelFilter;

    #[test]
    fn nix_verbosity_env_maps_tracing_level() {
        let cases = [
            (LevelFilter::OFF, None),
            (LevelFilter::ERROR, Some("error")),
            (LevelFilter::WARN, Some("warn")),
            (LevelFilter::INFO, None),
            (LevelFilter::DEBUG, Some("debug")),
            (LevelFilter::TRACE, Some("trace")),
        ];
        for (level, expected) in cases {
            assert_eq!(nix_verbosity_env(level), expected, "level={level}");
        }
    }

    #[test]
    fn eval_plan_flake_mode_splits_fragments() {
        let (argv, attrs) = eval_plan(
            &[".#hello".into(), ".#world".into(), ".".into()],
            None,
            FileEvalOpts::default(),
            Path::new("/cas"),
        )
        .unwrap();
        assert_eq!(argv, vec!["--cas", "/cas", "--flake", "."]);
        assert_eq!(attrs, vec!["hello", "world", ""]);
    }

    #[test]
    fn eval_plan_rejects_mixed_flake_refs() {
        let err = eval_plan(
            &["./a#x".into(), "./b#y".into()],
            None,
            FileEvalOpts::default(),
            Path::new("/cas"),
        )
        .unwrap_err();
        assert!(err.to_string().contains("share one flake ref"));
    }

    #[test]
    fn eval_plan_file_mode_passes_attrs_verbatim() {
        let (argv, attrs) = eval_plan(
            &["pkgA".into(), "nested.pkgB".into()],
            Some(Path::new("/tmp/fixture.nix")),
            FileEvalOpts::default(),
            Path::new("/cas"),
        )
        .unwrap();
        assert_eq!(argv, vec!["--cas", "/cas", "--file", "/tmp/fixture.nix"]);
        assert_eq!(attrs, vec!["pkgA", "nested.pkgB"]);
    }

    #[test]
    fn eval_plan_file_mode_zero_installables_builds_top_level() {
        let (argv, attrs) = eval_plan(
            &[],
            Some(Path::new("/tmp/default.nix")),
            FileEvalOpts::default(),
            Path::new("/cas"),
        )
        .unwrap();
        assert_eq!(argv, vec!["--cas", "/cas", "--file", "/tmp/default.nix"]);
        // The empty attr path is the file's top-level value.
        assert_eq!(attrs, vec![String::new()]);
    }

    #[test]
    fn eval_plan_file_mode_forwards_nix_build_flags() {
        let arg = vec!["tagged".to_string(), "true".to_string()];
        let argstr = vec!["name".to_string(), "custom".to_string()];
        let include = vec!["probe=/tmp/probe".to_string()];
        let (argv, _) = eval_plan(
            &["pkgA".into()],
            Some(Path::new("/tmp/fixture.nix")),
            FileEvalOpts {
                args: &arg,
                argstrs: &argstr,
                includes: &include,
            },
            Path::new("/cas"),
        )
        .unwrap();
        assert_eq!(
            argv,
            vec![
                "--cas",
                "/cas",
                "--file",
                "/tmp/fixture.nix",
                "--arg",
                "tagged",
                "true",
                "--argstr",
                "name",
                "custom",
                "-I",
                "probe=/tmp/probe",
            ]
        );
    }

    fn build_args(argv: &[&str]) -> super::BuildArgs {
        match Cli::try_parse_from(argv).unwrap().command {
            Command::Build(args) => args,
            Command::Log(_) => panic!("expected `rio build`"),
        }
    }

    #[test]
    fn log_lines_flags_parse() {
        // Failure-replay knobs: --log-lines N tail (default 20) and
        // -L/--print-build-logs for the full log.
        let args = build_args(&["rio", "build", ".#x", "--log-lines", "5", "-L"]);
        assert_eq!(args.log_lines, 5);
        assert!(args.print_build_logs);

        let args = build_args(&["rio", "build", ".#x"]);
        assert_eq!(args.log_lines, 20);
        assert!(!args.print_build_logs);
    }

    #[test]
    fn rio_log_subcommand_parses() {
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv";
        let args = match Cli::try_parse_from([
            "rio",
            "log",
            drv,
            "--build",
            "0190f7a1-7c2e-7d10-b5c5-3be41b1c6f7e",
            "--exec",
            "0190f7a1-7c2e-7d10-b5c5-3be41b1c6f00",
            "--log-lines",
            "7",
        ])
        .unwrap()
        .command
        {
            Command::Log(args) => args,
            Command::Build(_) => panic!("expected `rio log`"),
        };
        assert_eq!(args.drv_path, drv);
        assert_eq!(
            args.build.as_deref(),
            Some("0190f7a1-7c2e-7d10-b5c5-3be41b1c6f7e")
        );
        assert!(args.exec.is_some());
        assert_eq!(args.log_lines, Some(7));

        // Drv path only: build/exec unpinned, full log.
        let args = match Cli::try_parse_from(["rio", "log", drv]).unwrap().command {
            Command::Log(args) => args,
            Command::Build(_) => panic!("expected `rio log`"),
        };
        assert!(args.build.is_none() && args.exec.is_none() && args.log_lines.is_none());
    }

    #[test]
    fn nix_build_flags_rejected_without_file() {
        // --arg/--argstr/-I only make sense for the file's top-level
        // function; in flake mode they must error, not be ignored.
        for args in [
            vec!["rio", "build", ".#hello", "--arg", "a", "1"],
            vec!["rio", "build", ".#hello", "--argstr", "a", "v"],
            vec!["rio", "build", ".#hello", "-I", "p=/tmp"],
        ] {
            assert!(Cli::try_parse_from(&args).is_err(), "accepted {args:?}");
        }
        assert!(Cli::try_parse_from(["rio", "build", "-f", "x.nix", "--argstr", "a", "v"]).is_ok());
    }
}
