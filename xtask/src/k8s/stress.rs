//! `qa --load` implementation: fire N parallel `nix build --store
//! ssh-ng://…` clients through SSM tunnels and wait for them.
//! Foreground: tunnels + builds are [`ProcessGuard`]-/kill_on_drop-
//! wrapped, so Ctrl-C or panic tears everything down (including
//! session-manager-plugin grandchildren — I-158). Per-build logs land
//! in `.stress-test/{ts}/build-{port}.log`.
//!
//! The bench target is resolved to a `.drv` path ONCE on the
//! coordinator and every client builds `<drv>^*` directly. The
//! 2026-06-11 in-cluster campaign showed that per-client cold flake
//! eval (each client fetching nix-bench's ~8 flake inputs from
//! github + cache.nixos.org) dominates above ~128 clients — the
//! ladder measured internet egress, not rio. Clients never evaluate:
//! the drv closure is already in the coordinator's local store from
//! the pre-resolve, and `nix build --store …` uploads it to rio as
//! part of the first build (the rest hit QueryValidPaths).

use std::fs::{self, File};
use std::path::PathBuf;
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use console::style;
use tracing::info;

use super::provider::{Provider, ProviderKind};
use super::shared::ProcessGuard;
use crate::config::XtaskConfig;
use crate::sh::repo_root;

/// Knobs for the load stage, owned by `qa::QaOpts`.
pub(super) struct LoadOpts {
    /// `packages.x86_64-linux` attribute of the bench flake.
    pub target: String,
    pub parallel: u16,
    /// 0 → each tunnel binds its own ephemeral local port.
    pub base_port: u16,
    /// Bench flake checkout; default `~/src/nix-bench/main`.
    pub bench_flake: Option<PathBuf>,
    /// Poll scheduler metrics every 30s while builds run.
    pub watch: bool,
    /// Spread client starts evenly over this window instead of
    /// all-at-once. 512 simultaneous starts destabilized the EKS
    /// control plane in the 2026-06-11 campaign (~5min apiserver
    /// blackout, 7 NotReady nodes from Karpenter churn).
    pub stagger: Duration,
}

/// Start offset for client `i` of `n` when ramping over `window`:
/// evenly spaced, client 0 at t=0, client n-1 at `window * (n-1)/n`.
fn stagger_offset(i: u16, n: u16, window: Duration) -> Duration {
    if n == 0 || window.is_zero() {
        return Duration::ZERO;
    }
    let nanos = window.as_nanos() * u128::from(i) / u128::from(n);
    Duration::from_nanos(u64::try_from(nanos).unwrap_or(u64::MAX))
}

pub(super) async fn cmd_run(
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    opts: &LoadOpts,
) -> Result<()> {
    let bench = resolve_bench_flake(opts.bench_flake.clone())?;
    let installable = format!("{}#{}", bench.display(), opts.target);

    // Pre-resolve the target to a .drv ONCE. This kills the cold-eval
    // thundering herd (module doc) and also subsumes the I-161 warm-up:
    // clients no longer evaluate at all, so the ssh-ng connection never
    // sits idle during eval and the SSM gateway's 120s idle drop can't
    // trigger. Hard error — every client depends on the resolved path.
    info!("resolving {installable} to a .drv (cold eval can take ~2min)");
    let drv = resolve_drv(&installable)?;
    info!("target resolved: {drv}");

    run_port_forward(p, kind, cfg, &drv, opts).await
}

/// Resolve `installable` to its `.drv` store path. Instantiates the
/// derivation closure into the local store as a side effect — exactly
/// what the port-forward clients need to build `<drv>^*` without
/// evaluating.
fn resolve_drv(installable: &str) -> Result<String> {
    let out = std::process::Command::new("nix")
        .args(["path-info", "--derivation", "--impure", installable])
        .output()
        .context("spawn nix path-info for drv pre-resolve")?;
    anyhow::ensure!(
        out.status.success(),
        "nix path-info --derivation {installable} failed: {}",
        std::str::from_utf8(&out.stderr)
            .unwrap_or("<non-utf8 stderr>")
            .trim()
    );
    parse_drv_output(std::str::from_utf8(&out.stdout).context("nix path-info stdout not utf-8")?)
}

/// Pure parse of `nix path-info --derivation` stdout (one store path).
fn parse_drv_output(stdout: &str) -> Result<String> {
    let path = stdout.trim();
    anyhow::ensure!(
        path.starts_with("/nix/store/") && path.ends_with(".drv") && path.lines().count() == 1,
        "expected a single .drv store path from nix path-info, got: {path:?}"
    );
    Ok(path.to_string())
}

#[allow(clippy::print_stderr)]
async fn run_port_forward(
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    drv: &str,
    opts: &LoadOpts,
) -> Result<()> {
    let parallel = opts.parallel;
    let key = crate::ssh::privkey_path(cfg)?;
    let buildable = format!("{drv}^*");

    let ts = jiff::Timestamp::now().as_second();
    let dir = repo_root().join(".stress-test").join(ts.to_string());
    fs::create_dir_all(&dir)?;
    info!("logs: {}", dir.display());

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

    let ramp_start = tokio::time::Instant::now();
    for i in 0..parallel {
        // --load-stagger: hold client i back until its slot in the ramp.
        // select! so a Ctrl-C during a long window aborts immediately
        // instead of queuing behind the remaining sleeps.
        let due = ramp_start + stagger_offset(i, parallel, opts.stagger);
        if tokio::time::Instant::now() < due {
            tokio::select! {
                biased;
                _ = sigint.recv() => anyhow::bail!("interrupted"),
                _ = tokio::time::sleep_until(due) => {}
            }
        }
        // base_port 0 → each tunnel on its own ephemeral port.
        let req = if opts.base_port == 0 {
            0
        } else {
            opts.base_port
                .checked_add(i)
                .context("base_port + parallel overflows u16")?
        };
        if req != 0 {
            super::shared::kill_port_listeners(req);
        }
        info!("tunnel[{i}]: establishing");
        let (port, guard) = p.tunnel(req).await?;
        tunnels.push(guard);

        let log_path = dir.join(format!("build-{port}.log"));
        let log_file = File::create(&log_path)?;
        let log_err = log_file.try_clone()?;
        let store = format!(
            "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
            key.display()
        );

        let child = tokio::process::Command::new("nix")
            .args(["build", "--store", &store, "--eval-store", "auto"])
            .arg(&buildable)
            // -L: stream build logs to stderr. Without it, redirected
            // stderr stays empty until the first `copying path` line
            // (I-051). No --impure: a .drv^* installable is pure.
            .args(["--no-link", "-L", "--max-jobs", "0"])
            // I-149/I-161: see `shared::NIX_SSHOPTS_BASE`.
            .env("NIX_SSHOPTS", super::shared::NIX_SSHOPTS_BASE)
            .stdin(Stdio::null())
            .stdout(log_file)
            .stderr(log_err)
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("spawn nix build (drv: {drv})"))?;
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
            if opts.watch {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_drv_output_accepts_single_drv_path() {
        let p = "/nix/store/abc123-small-mixed-4x.drv\n";
        assert_eq!(
            parse_drv_output(p).unwrap(),
            "/nix/store/abc123-small-mixed-4x.drv"
        );
    }

    #[test]
    fn stagger_zero_window_starts_everyone_immediately() {
        for i in 0..8 {
            assert_eq!(stagger_offset(i, 8, Duration::ZERO), Duration::ZERO);
        }
    }

    #[test]
    fn stagger_offsets_evenly_spaced_inside_window() {
        let w = Duration::from_secs(64);
        let offsets: Vec<_> = (0..8).map(|i| stagger_offset(i, 8, w)).collect();
        assert_eq!(offsets[0], Duration::ZERO);
        assert_eq!(offsets[1], Duration::from_secs(8));
        assert_eq!(offsets[7], Duration::from_secs(56));
        assert!(offsets.windows(2).all(|p| p[0] < p[1]), "not monotonic");
        assert!(*offsets.last().unwrap() < w, "last start must be < window");
    }

    #[test]
    fn parse_drv_output_rejects_garbage() {
        // Empty (eval produced nothing), an output path (forgot
        // --derivation), and multi-line (multiple installables) would
        // each silently break every client — fail at the coordinator.
        for bad in [
            "",
            "/nix/store/abc123-small-mixed-4x",
            "error: attribute missing",
            "/nix/store/a.drv\n/nix/store/b.drv\n",
        ] {
            assert!(parse_drv_output(bad).is_err(), "accepted {bad:?}");
        }
    }
}
