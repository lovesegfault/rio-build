//! `qa --load` implementation: fire N parallel `nix build --store
//! ssh-ng://…` clients through SSM tunnels and wait for them.
//! Foreground: tunnels + builds are [`ProcessGuard`]-/kill_on_drop-
//! wrapped, so Ctrl-C or panic tears everything down (including
//! session-manager-plugin grandchildren — I-158). Per-build logs land
//! in `.stress-test/{ts}/build-{port}.log`.

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use console::style;
use tracing::{info, warn};

use super::provider::{Provider, ProviderKind};
use super::shared::ProcessGuard;
use crate::config::XtaskConfig;
use crate::sh::repo_root;

#[allow(clippy::too_many_arguments, clippy::print_stderr)]
pub(super) async fn cmd_run(
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
