//! `qa --load` implementation: fire N parallel `nix build --store
//! ssh-ng://…` clients through SSM tunnels and wait for them.
//! Foreground: tunnels + builds are [`ProcessGuard`]-/kill_on_drop-
//! wrapped, so Ctrl-C or panic tears everything down (including
//! session-manager-plugin grandchildren — I-158). Per-build logs land
//! in `.stress-test/{ts}/build-{port}.log`.

use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use console::style;
use tracing::info;

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
    let installable = format!("{}#{target}", bench.display());

    let ts = jiff::Timestamp::now().as_second();
    let dir = repo_root().join(".stress-test").join(ts.to_string());
    fs::create_dir_all(&dir)?;
    info!("logs: {}", dir.display());

    // I-161 eval-cache warm; the drv path it returns is only needed by
    // fsbench (build→node attribution), not here.
    let _ = super::shared::pre_eval_installable(&installable, &[]);

    // SIGINT handler registered BEFORE spawning anything: default
    // disposition terminates abnormally (no Drop), so ProcessGuard's
    // killpg never runs and tunnels (own process group via
    // `process_group(0)`) leak — the I-158 orphan.
    //
    // `signal()`, NOT `ctrl_c()`: `ctrl_c()` is `async fn`, so the
    // sigaction installs at first POLL (the select! below), not at
    // call. tokio::pin! does not poll. The spawn loop awaits
    // `p.tunnel(port)` (SSM bind + banner wait) per port — Ctrl-C
    // during that window would still hit the default
    // disposition. `signal()` is a plain fn that registers the
    // sigaction synchronously at call time.
    let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;

    // Tunnels (ProcessGuard: killpg on drop) + builds (kill_on_drop).
    // Held in Vecs for the lifetime of the await below — Ctrl-C or `?`
    // anywhere drops everything.
    let mut tunnels: Vec<ProcessGuard> = Vec::with_capacity(parallel as usize);
    let mut builds: Vec<(u16, tokio::process::Child)> = Vec::with_capacity(parallel as usize);

    for i in 0..parallel {
        // base_port 0 → each tunnel on its own ephemeral port.
        let req = if base_port == 0 {
            0
        } else {
            base_port + u16::from(i)
        };
        info!("tunnel[{i}]: establishing");
        // Tunnel naming: the log file is keyed by the BOUND port, which
        // spawn_remote_nix_build only knows after the tunnel is up — so
        // the path passed in is provisional and renamed-by-construction
        // via a two-step: ephemeral requests get a per-index temp name.
        let log_path = dir.join(format!("build-req{i}.log"));
        let rb = super::shared::spawn_remote_nix_build(p, req, cfg, &installable, &log_path, &[])
            .await?;
        // Re-key the log to the bound port (the historical name format
        // operators grep for). Rename is same-dir, atomic, and the
        // child holds the fd — the stream is unaffected.
        let final_path = dir.join(format!("build-{}.log", rb.port));
        if fs::rename(&log_path, &final_path).is_err() {
            info!("log stays at {}", log_path.display());
        }
        tunnels.push(rb.tunnel);
        builds.push((rb.port, rb.child));
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
                            "queued={} running={} open_attempts={}",
                            m.derivations_queued, m.derivations_running, m.open_attempts
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
