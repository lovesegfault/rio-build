//! `xtask k8s stress` — stress-build harness.
//!
//! Fire N parallel `nix build --store ssh-ng://…` clients through SSM
//! tunnels and wait for them. Foreground: tunnels + builds are
//! [`ProcessGuard`]-/kill_on_drop-wrapped, so Ctrl-C or panic tears
//! everything down (including session-manager-plugin grandchildren —
//! I-158). Per-build logs land in `.stress-test/{ts}/build-{port}.log`.
//!
//! The previous design detached (`setsid` + `mem::forget`) and
//! persisted PIDs to `.stress-test/sessions/{ts}/pids.json` for
//! `watch`/`list`/`cleanup` across xtask invocations. Over-engineered
//! for a dev tool; the original motivation (REPL-timeout SIGKILL
//! orphans) is moot when run interactively.

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;
use console::style;
use tracing::{info, warn};

use super::chaos::{self, ChaosFrom, ChaosKind, ChaosTarget};
use super::provider::{Provider, ProviderKind};
use super::shared::ProcessGuard;
use crate::config::XtaskConfig;
use crate::sh::repo_root;

#[derive(Subcommand)]
pub enum StressCmd {
    /// Fire N builds through SSM tunnels and wait for them. Tunnels +
    /// builds tear down on Ctrl-C / exit.
    Run {
        /// nix-bench target (e.g. hello-shallow-32x, medium-mixed-16x).
        #[arg(long)]
        target: String,
        /// Number of parallel builds (each gets its own SSM tunnel + port).
        #[arg(long, default_value_t = 1)]
        parallel: u8,
        /// Base local port. Each build uses base_port + index.
        #[arg(long, default_value_t = 2250)]
        base_port: u16,
        /// Path to the nix-bench flake. Default: ~/src/nix-bench/main.
        #[arg(long)]
        bench_flake: Option<PathBuf>,
        /// Print active-build count + key scheduler gauges every 30s
        /// while waiting.
        #[arg(long)]
        watch: bool,
    },
    /// Inject a network fault. BLOCKS for `--duration` (or until
    /// Ctrl-C) and cleans up on exit. State written to
    /// `.stress-test/chaos/chaos.json` BEFORE the rules go in, so a
    /// SIGKILL'd run is remediated by the NEXT chaos invocation.
    ///
    /// `--kind blackhole`: privileged hostNetwork pod nsenters the
    /// host and inserts iptables DROP rules for `--target`'s pod IP.
    /// No FIN, no RST — only h2 keepalive can detect (I-048c).
    Chaos {
        #[arg(long, value_enum)]
        kind: ChaosKind,
        #[arg(long, value_parser = clap::value_parser!(ChaosTarget))]
        target: ChaosTarget,
        #[arg(long, value_parser = clap::value_parser!(ChaosFrom),
              default_value = "all-workers")]
        from: ChaosFrom,
        #[arg(long, value_parser = chaos::parse_duration_secs)]
        duration: Duration,
    },
}

pub async fn run(
    cmd: StressCmd,
    p: &dyn Provider,
    p_kind: ProviderKind,
    cfg: &XtaskConfig,
) -> Result<()> {
    match cmd {
        StressCmd::Run {
            target,
            parallel,
            base_port,
            bench_flake,
            watch,
        } => {
            cmd_run(
                p,
                p_kind,
                cfg,
                &target,
                parallel,
                base_port,
                bench_flake,
                watch,
            )
            .await
        }
        StressCmd::Chaos {
            kind,
            target,
            from,
            duration,
        } => {
            let dir = repo_root().join(".stress-test/chaos");
            fs::create_dir_all(&dir)?;
            // Remediate any prior SIGKILL'd run before starting.
            // Best-effort — kube errors warn, don't abort.
            if let Err(e) = chaos::remediate(&dir).await {
                warn!("stale-chaos remediation: {e:#}");
            }
            chaos::run(&dir, kind, target, from, duration).await
        }
    }
}

#[allow(clippy::too_many_arguments, clippy::print_stderr)]
async fn cmd_run(
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    target: &str,
    parallel: u8,
    base_port: u16,
    bench_flake: Option<PathBuf>,
    watch: bool,
) -> Result<()> {
    let bench = resolve_bench_flake(bench_flake)?;
    let key = crate::ssh::privkey_path(cfg)?;
    let installable = format!("{}#{target}", bench.display());

    let ts = jiff::Timestamp::now().as_second();
    let dir = repo_root().join(".stress-test").join(ts.to_string());
    fs::create_dir_all(&dir)?;
    info!("logs: {}", dir.display());

    // I-161: warm the eval cache so the build's ssh-ng connection
    // doesn't sit idle during cold --impure eval. nix opens the
    // connection on first remote query, then evaluates locally; over
    // SSM port-forward, server-originated keepalive replies don't
    // reliably round-trip when there's zero client→server data, so the
    // gateway drops the session at 120s while nix is still evaluating.
    // Pre-evaluating shrinks the connect→submit window to <5s.
    info!("pre-evaluating {installable} (cold-eval can take ~2min)");
    let pre_eval = std::process::Command::new("nix")
        .args(["path-info", "--derivation", "--impure", &installable])
        .output()
        .context("spawn nix path-info for pre-eval")?;
    if !pre_eval.status.success() {
        warn!(
            "pre-eval failed (continuing): {}",
            std::str::from_utf8(&pre_eval.stderr)
                .unwrap_or("<non-utf8 stderr>")
                .trim()
        );
    }

    // SIGINT handler registered BEFORE spawning anything: default
    // disposition terminates abnormally (no Drop), so ProcessGuard's
    // killpg never runs and tunnels (own process group via
    // `process_group(0)`) leak — the I-158 orphan.
    //
    // `signal()`, NOT `ctrl_c()`: `ctrl_c()` is `async fn`, so the
    // sigaction installs at first POLL (the select! below), not at
    // call. tokio::pin! does not poll. The spawn loop awaits
    // `p.tunnel(port)` (up to 150s NLB poll + banner wait) per port —
    // Ctrl-C during that window would still hit the default
    // disposition. `signal()` is a plain fn that registers the
    // sigaction synchronously at call time.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // Tunnels (ProcessGuard: killpg on drop) + builds (kill_on_drop).
    // Held in Vecs for the lifetime of the await below — Ctrl-C or `?`
    // anywhere drops everything.
    let mut tunnels: Vec<ProcessGuard> = Vec::with_capacity(parallel as usize);
    let mut builds: Vec<(u16, tokio::process::Child)> = Vec::with_capacity(parallel as usize);

    for i in 0..parallel {
        let port = base_port + u16::from(i);
        super::shared::kill_port_listeners(port);
        info!("tunnel[{port}]: establishing");
        tunnels.push(p.tunnel(port).await?);

        let log_path = dir.join(format!("build-{port}.log"));
        let log_file = File::create(&log_path)?;
        let log_err = log_file.try_clone()?;
        let store = format!(
            "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
            key.display()
        );

        let child = tokio::process::Command::new("nix")
            .args(["build", "--store", &store, "--eval-store", "auto"])
            .arg(&installable)
            // -L: stream build logs to stderr. Without it, redirected
            // stderr stays empty until the first `copying path` line —
            // ~2.5min of silence on cold-cache eval (I-051).
            .args(["--impure", "--no-link", "-L", "--max-jobs", "0"])
            // I-149/I-161: see `shared::NIX_SSHOPTS_BASE`.
            .env("NIX_SSHOPTS", super::shared::NIX_SSHOPTS_BASE)
            .stdin(Stdio::null())
            .stdout(log_file)
            .stderr(log_err)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn nix build (installable: {installable})"))?;
        info!(
            "build[{port}]: pid={} → {}",
            child.id().unwrap_or(0),
            log_path.display()
        );
        builds.push((port, child));
    }

    eprintln!(
        "{} {parallel} build(s) running ({}) — Ctrl-C to abort",
        style("▸").blue(),
        kind
    );

    // Await all. With --watch, also poll status every 30s. Single
    // select! covers both paths so Ctrl-C without --watch ALSO unwinds
    // (→ ProcessGuard::Drop → killpg) instead of taking the default
    // SIGINT disposition.
    let mut ok = 0usize;
    tokio::select! {
        biased;
        _ = sigint.recv() => anyhow::bail!("interrupted"),
        r = async {
            if watch {
                let client = crate::k8s::client::client().await?;
                let mut tick = tokio::time::interval(Duration::from_secs(30));
                loop {
                    tick.tick().await;
                    let mut alive = 0usize;
                    for (_, c) in &mut builds {
                        if matches!(c.try_wait(), Ok(None)) { alive += 1; }
                    }
                    let m = crate::k8s::status::gather_scheduler_metrics(&client)
                        .await
                        .map(|m| format!(
                            "queued={} fetcher_q={} fetcher_util={:.2}",
                            m.derivations_queued, m.fetcher_queue_depth, m.fetcher_utilization
                        ))
                        .unwrap_or_else(|| "(metrics unavailable)".into());
                    eprintln!("{} alive={alive}/{parallel}  {}", style("·").dim(), style(m).dim());
                    if alive == 0 { break; }
                }
            }
            for (port, mut child) in builds {
                let status = child.wait().await?;
                if status.success() {
                    ok += 1;
                    eprintln!("  {} [{port}] ok", style("✓").green());
                } else {
                    eprintln!(
                        "  {} [{port}] {status} — see {}/build-{port}.log",
                        style("✗").red(),
                        dir.display()
                    );
                }
            }
            anyhow::Ok(())
        } => r?,
    }
    drop(tunnels);

    eprintln!();
    eprintln!("{} {ok}/{parallel} succeeded", style("✓").green());
    if ok < parallel as usize {
        anyhow::bail!("{} build(s) failed", parallel as usize - ok);
    }
    Ok(())
}

fn resolve_bench_flake(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let p = explicit.unwrap_or_else(|| {
        std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("src/nix-bench/main")
    });
    anyhow::ensure!(
        p.join("flake.nix").exists(),
        "nix-bench flake not found at {}\n\
         (pass --bench-flake /path/to/nix-bench/main)",
        p.display()
    );
    Ok(p)
}
