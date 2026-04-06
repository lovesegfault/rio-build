//! `xtask k8s stress` — detached stress-build harness.
//!
//! Fire N parallel `nix build --store ssh-ng://…` clients through SSM
//! tunnels, return immediately, manage them with `watch`/`list`/`cleanup`.
//!
//! **SIGKILL-orphan fix:** during QA we hand-rolled this via `setsid
//! nohup cargo xtask k8s rsb …`. Problem: SSM session-manager-plugin
//! children orphan when the parent is SIGKILLed (REPL timeout) —
//! [`ProcessGuard`](super::shared::ProcessGuard)::drop doesn't fire,
//! the PID isn't written anywhere, cleanup is manual `pkill`. Left 3
//! zombie tunnels + dead nix clients during one session.
//!
//! The fix here: write the PID to disk BEFORE detaching. `cleanup`
//! reads every session dir, probes PID liveness with `kill(pid, 0)`,
//! reaps dead sessions, kills alive ones on `--all`.
//!
//! State lives under `.stress-test/sessions/{unix_ts}/`:
//!   `meta.json`         target, parallel, ports, start_time, provider
//!   `pids.json`         { tunnels: [{port,pid}], builds: [{port,pid}] }
//!   `chaos.json`        [{node, pod_name, target_ip, chain}] (chaos subcmd)
//!   `build-{port}.log`  per-build stdout+stderr
//!   `watch.jsonl`       poll snapshots (appended by `stress watch`)

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Subcommand;
use console::style;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use super::chaos::{self, ChaosFrom, ChaosKind, ChaosTarget};
use super::provider::{Provider, ProviderKind};
use crate::config::XtaskConfig;
use crate::k8s::NS;
use crate::k8s::eks::smoke::CliCtx;
use crate::k8s::k3s::smoke::{port_forward, scheduler_leader_pod};
use crate::sh::repo_root;

#[derive(Subcommand)]
pub enum StressCmd {
    /// Fire N detached builds through SSM tunnels. Returns immediately;
    /// builds continue after xtask exits. Use `stress list` / `watch`
    /// / `cleanup` to manage.
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
    },
    /// Poll active builds + scheduler metrics every 30s. Appends
    /// to the session's watch.jsonl. Ctrl-C to stop.
    Watch {
        /// Session timestamp. Default: most recent.
        #[arg(long)]
        session: Option<String>,
    },
    /// Kill orphaned tunnels + build clients. Removes session dirs
    /// where all PIDs are dead.
    Cleanup {
        /// Also kill ALIVE sessions (not just orphans).
        #[arg(long)]
        all: bool,
    },
    /// Show active sessions with build progress.
    List,
    /// Inject a network fault. Unlike `run`, this BLOCKS for
    /// `--duration` (or until Ctrl-C) and cleans up on exit. Chaos
    /// that survives the xtask process is a footgun.
    ///
    /// `--kind blackhole`: privileged hostNetwork pod nsenters the
    /// host and inserts iptables DROP rules for `--target`'s pod IP.
    /// No FIN, no RST — only h2 keepalive can detect (I-048c).
    ///
    /// State is tracked in `<session>/chaos.json` BEFORE the rules go
    /// in, so `stress cleanup` can remediate even if xtask is
    /// SIGKILLed mid-chaos.
    Chaos {
        /// Fault kind. Only `blackhole` is implemented.
        #[arg(long, value_enum)]
        kind: ChaosKind,
        /// What to blackhole. `scheduler-leader` reads the lease.
        /// `builder-<N>`/`fetcher-<N>` index by sorted pod name.
        #[arg(long, value_parser = clap::value_parser!(ChaosTarget))]
        target: ChaosTarget,
        /// Which workers lose connectivity. The chaos pod runs
        /// hostNetwork on each worker's NODE.
        #[arg(long, value_parser = clap::value_parser!(ChaosFrom),
              default_value = "all-workers")]
        from: ChaosFrom,
        /// How long to hold the fault. `60s` or `60`.
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
        } => cmd_run(p, p_kind, cfg, &target, parallel, base_port, bench_flake).await,
        StressCmd::Watch { session } => cmd_watch(session).await,
        StressCmd::Cleanup { all } => cmd_cleanup(all).await,
        StressCmd::List => cmd_list(),
        StressCmd::Chaos {
            kind,
            target,
            from,
            duration,
        } => {
            // Chaos creates its own session dir (no detached PIDs to
            // track, but chaos.json needs a home for SIGKILL-recovery).
            // Reap dead sessions first — same courtesy as `run`.
            let _ = reap_dead_sessions();
            let ts = jiff::Timestamp::now().as_second();
            let dir = session_root().join(ts.to_string());
            fs::create_dir_all(&dir)?;
            // Minimal meta so `list`/`cleanup` can identify what this is.
            let meta = SessionMeta {
                target: format!("chaos:{kind:?}:{target}"),
                parallel: 0,
                ports: vec![],
                start_time: ts,
                provider: p_kind.to_string(),
            };
            fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;
            // Empty pids.json so iter_sessions doesn't warn.
            write_pids(&dir, &SessionPids::default())?;

            let res = chaos::run(&dir, kind, target, from, duration).await;

            // On clean exit, chaos.json is already cleared. If chaos
            // errored, leave the dir for `stress cleanup` to remediate.
            if res.is_ok() && chaos::read_chaos(&dir)?.entries.is_empty() {
                fs::remove_dir_all(&dir)?;
            }
            res
        }
    }
}

// ─── state files ────────────────────────────────────────────────────

#[derive(Serialize, Deserialize)]
struct SessionMeta {
    target: String,
    parallel: u8,
    ports: Vec<u16>,
    start_time: i64,
    provider: String,
}

#[derive(Serialize, Deserialize, Default)]
struct SessionPids {
    tunnels: Vec<PidEntry>,
    builds: Vec<PidEntry>,
}

#[derive(Serialize, Deserialize, Clone, Copy)]
struct PidEntry {
    port: u16,
    pid: u32,
}

#[derive(Serialize)]
struct WatchSnapshot<'a> {
    ts: i64,
    builds_active: &'a str,
    scheduler_metrics: &'a str,
    top_nodes: &'a str,
}

fn session_root() -> PathBuf {
    repo_root().join(".stress-test/sessions")
}

/// Session dir → display name. Dirs are named `{unix_ts}` (ASCII
/// digits), so to_str always succeeds; "?" is a defensive fallback.
fn session_name(dir: &Path) -> &str {
    dir.file_name().and_then(|n| n.to_str()).unwrap_or("?")
}

fn write_pids(dir: &Path, pids: &SessionPids) -> Result<()> {
    // Write-then-rename for atomicity: if xtask is SIGKILLed mid-write,
    // readers see either the old complete file or nothing, never a
    // partial JSON.
    let tmp = dir.join("pids.json.tmp");
    let dst = dir.join("pids.json");
    fs::write(&tmp, serde_json::to_string_pretty(pids)?)?;
    fs::rename(tmp, dst)?;
    Ok(())
}

fn read_pids(dir: &Path) -> Result<SessionPids> {
    let p = dir.join("pids.json");
    if !p.exists() {
        return Ok(SessionPids::default());
    }
    Ok(serde_json::from_str(&fs::read_to_string(p)?)?)
}

fn read_meta(dir: &Path) -> Result<SessionMeta> {
    Ok(serde_json::from_str(&fs::read_to_string(
        dir.join("meta.json"),
    )?)?)
}

// ─── PID probes ─────────────────────────────────────────────────────

/// Probe PID liveness via signal-0. ESRCH = dead; EPERM = alive but
/// not ours (e.g. PID reused by a root process). Same pattern as
/// `rio-test-support/src/pg.rs::pid_alive`.
#[cfg(target_os = "linux")]
fn pid_alive(pid: u32) -> bool {
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    match kill(Pid::from_raw(pid as i32), None) {
        Ok(_) | Err(nix::errno::Errno::EPERM) => true,
        Err(_) => false,
    }
}

#[cfg(not(target_os = "linux"))]
fn pid_alive(_pid: u32) -> bool {
    // stress harness is Linux-only (SSM, setsid); conservative on
    // other targets so the crate still compiles.
    true
}

/// SIGTERM → wait up to 2s → SIGKILL. Returns whether the PID is
/// gone after the sequence.
#[cfg(target_os = "linux")]
fn kill_graceful(pid: u32) -> bool {
    use nix::sys::signal::{Signal, kill, killpg};
    use nix::unistd::Pid;
    let p = Pid::from_raw(pid as i32);
    // I-158: tunnel pids are group leaders (ProcessGuard::spawn sets
    // process_group(0)). killpg catches session-manager-plugin
    // grandchildren that single-pid kill would orphan. Build pids are
    // session leaders too (setsid below), so killpg is correct for
    // both. Falls back to single-pid kill if killpg ESRCHs (pid was
    // recorded before process_group(0) was added — old session dirs).
    if killpg(p, Signal::SIGTERM).is_err() {
        let _ = kill(p, Some(Signal::SIGTERM));
    }
    // Poll for death: 20 × 100ms = 2s.
    for _ in 0..20 {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if killpg(p, Signal::SIGKILL).is_err() {
        let _ = kill(p, Some(Signal::SIGKILL));
    }
    // SIGKILL is unblockable, but reaping isn't synchronous with
    // kill(2). These are session-leader procs (setsid) with no
    // waiting parent — init auto-reaps, fast but not instant. A
    // single 100ms+check raced reap and printed spurious "survived
    // SIGKILL" warnings (I-050b). Poll for ESRCH instead; 5 × 100ms
    // is generous.
    for _ in 0..5 {
        std::thread::sleep(Duration::from_millis(100));
        if !pid_alive(pid) {
            return true;
        }
    }
    false
}

#[cfg(not(target_os = "linux"))]
fn kill_graceful(_pid: u32) -> bool {
    false
}

// ─── run ────────────────────────────────────────────────────────────

// eprintln! summary block runs AFTER all tunnels are established (no
// active spinner). Same pattern as status.rs.
#[allow(clippy::print_stderr)]
async fn cmd_run(
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    target: &str,
    parallel: u8,
    base_port: u16,
    bench_flake: Option<PathBuf>,
) -> Result<()> {
    let bench = resolve_bench_flake(bench_flake)?;
    let key = crate::ssh::privkey_path(cfg)?;

    // Opportunistic reap: drop session dirs where every PID is dead.
    // Don't touch anything still alive (might be a concurrent run).
    let _ = reap_dead_sessions();

    let ts = jiff::Timestamp::now().as_second();
    let dir = session_root().join(ts.to_string());
    fs::create_dir_all(&dir)?;

    let ports: Vec<u16> = (0..parallel).map(|i| base_port + u16::from(i)).collect();
    let meta = SessionMeta {
        target: target.to_string(),
        parallel,
        ports: ports.clone(),
        start_time: ts,
        provider: kind.to_string(),
    };
    fs::write(dir.join("meta.json"), serde_json::to_string_pretty(&meta)?)?;

    let mut pids = SessionPids::default();
    // Flush the (empty) pids file before spawning anything: if the
    // tunnel spawn succeeds but we're SIGKILLed before the write,
    // cleanup at least sees the session dir exists.
    write_pids(&dir, &pids)?;

    info!("session dir: {}", dir.display());

    let installable = format!("{}#{target}", bench.display());

    // I-161: warm the eval cache so the detached build's ssh-ng
    // connection doesn't sit idle during cold --impure eval. nix opens
    // the connection on first remote query, then evaluates locally;
    // over SSM port-forward, server-originated keepalive replies don't
    // reliably round-trip when there's zero client→server data, so the
    // gateway drops the session at 120s while nix is still evaluating.
    // Pre-evaluating shrinks the connect→submit window to <5s. The
    // ServerAliveInterval in NIX_SSHOPTS_BASE below is the second
    // defense; gateway-side keepalive_max=9 (300s) is the third.
    info!("pre-evaluating {installable} (cold-eval can take ~2min)");
    let pre_eval = std::process::Command::new("nix")
        .args(["path-info", "--derivation", "--impure", &installable])
        .output()
        .context("spawn nix path-info for pre-eval")?;
    if !pre_eval.status.success() {
        // Non-fatal: the detached build re-evaluates anyway. Surface
        // the stderr so a typo in --target shows up here instead of
        // only in the per-port log.
        warn!(
            "pre-eval failed (continuing): {}",
            std::str::from_utf8(&pre_eval.stderr)
                .unwrap_or("<non-utf8 stderr>")
                .trim()
        );
    }

    for &port in &ports {
        info!("tunnel[{port}]: establishing");
        let guard = p.tunnel(port).await?;
        let tunnel_pid = guard
            .child
            .id()
            .context("tunnel child has no PID (already reaped?)")?;
        // I-158: ProcessGuard::spawn puts the child in its own process
        // group (pgid == this pid). cleanup's kill_graceful targets the
        // group, so the recorded pid catches session-manager-plugin too.
        pids.tunnels.push(PidEntry {
            port,
            pid: tunnel_pid,
        });
        // Critical: flush PID to disk BEFORE forgetting the guard.
        // After this line, cleanup can find the tunnel even if we die.
        write_pids(&dir, &pids)?;
        // Drop the kill-on-drop. The tunnel outlives xtask.
        std::mem::forget(guard);
        info!("tunnel[{port}]: pid={tunnel_pid} (detached)");

        let log_path = dir.join(format!("build-{port}.log"));
        let log_file = File::create(&log_path)?;
        let log_err = log_file.try_clone()?;
        let store = format!(
            "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
            key.display()
        );

        let mut cmd = std::process::Command::new("nix");
        cmd.args(["build", "--store", &store, "--eval-store", "auto"])
            .arg(&installable)
            // -L: stream build logs to stderr. Without it, redirected
            // stderr stays empty until the first `copying path` line —
            // ~2.5min of silence on cold-cache eval, indistinguishable
            // from a hang (I-051).
            .args(["--impure", "--no-link", "-L", "--max-jobs", "0"])
            // I-161: this site predated the I-149 ServerAlive fix and
            // re-broke at the cold-eval idle window. See
            // `shared::NIX_SSHOPTS_BASE` for the full mechanism.
            .env("NIX_SSHOPTS", super::shared::NIX_SSHOPTS_BASE)
            .stdin(Stdio::null())
            .stdout(log_file)
            .stderr(log_err);

        // setsid: become a session leader. Survives SIGHUP when xtask's
        // process group orphans (terminal closed, REPL SIGKILL, etc).
        // pre_exec is unsafe because it runs post-fork pre-exec — must
        // be async-signal-safe. setsid(2) is.
        // SAFETY: setsid is async-signal-safe; no allocations or locks.
        unsafe {
            cmd.pre_exec(|| {
                nix::unistd::setsid()?;
                Ok(())
            });
        }

        let child = cmd.spawn().with_context(|| {
            format!("spawn nix build (is nix on PATH? installable: {installable})")
        })?;
        let build_pid = child.id();
        pids.builds.push(PidEntry {
            port,
            pid: build_pid,
        });
        write_pids(&dir, &pids)?;
        // std::process::Child has no kill-on-drop. Dropping it leaves
        // the process running — exactly what we want. Explicit forget
        // here documents the intent; a bare drop would work too.
        std::mem::forget(child);
        info!("build[{port}]: pid={build_pid} → {}", log_path.display());
    }

    eprintln!();
    eprintln!(
        "{} {} builds detached (session {})",
        style("✓").green(),
        parallel,
        style(ts).cyan()
    );
    eprintln!("  watch:   cargo xtask k8s -p {kind} stress watch");
    eprintln!("  list:    cargo xtask k8s -p {kind} stress list");
    eprintln!("  cleanup: cargo xtask k8s -p {kind} stress cleanup");
    Ok(())
}

fn resolve_bench_flake(explicit: Option<PathBuf>) -> Result<PathBuf> {
    let p = explicit.unwrap_or_else(|| {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        home.join("src/nix-bench/main")
    });
    anyhow::ensure!(
        p.join("flake.nix").exists(),
        "nix-bench flake not found at {}\n\
         (pass --bench-flake /path/to/nix-bench/main)",
        p.display()
    );
    Ok(p)
}

/// Drop session dirs where every recorded PID is dead AND no chaos
/// entries are pending. Called at the start of `run`/`chaos` as a
/// courtesy — doesn't touch the session being created (it doesn't
/// exist yet). Silently ignores unreadable dirs.
///
/// The chaos check matters: a chaos session has zero PIDs (empty
/// pids.json) but a non-empty chaos.json. Reaping it before
/// remediation would orphan the iptables rules.
fn reap_dead_sessions() -> Result<()> {
    for (dir, pids) in iter_sessions()? {
        if all_dead(&pids) && chaos::read_chaos(&dir)?.entries.is_empty() {
            fs::remove_dir_all(&dir)?;
            info!("reaped dead session {}", dir.display());
        }
    }
    Ok(())
}

// ─── watch ──────────────────────────────────────────────────────────

/// Watch poll cadence.
const WATCH_INTERVAL: Duration = Duration::from_secs(30);

// Watch's one-liner-per-poll prints between sleeps; no progress bars
// active (CliCtx::open's forward spawns are already returned).
#[allow(clippy::print_stderr)]
async fn cmd_watch(session: Option<String>) -> Result<()> {
    let dir = find_session(session)?;
    let meta = read_meta(&dir)?;
    info!(
        "watching session {} ({} × {})",
        session_name(&dir),
        meta.parallel,
        meta.target
    );

    // Three port-forwards: scheduler gRPC + store gRPC (via CliCtx)
    // and scheduler:9091 prometheus. All tear down when watch exits
    // (ProcessGuard drop fires — watch is a long-lived foreground
    // process, not detached like run).
    let client = crate::kube::client().await?;
    // I-101: ephemeral local ports — no collision with a concurrent
    // `k8s cli` invocation (previously offset to 19101/19102/19091).
    let cli = CliCtx::open(&client, 0, 0).await?;
    // I-050: svc/rio-scheduler only exposes port 9001 — `kubectl
    // port-forward svc/rio-scheduler X:9091` exits immediately with
    // "does not have a service port 9091". port_forward() nulls
    // stderr (k3s/smoke.rs:78), so the error vanishes; the 2s sleep
    // that was here never noticed. Target the leader pod (which DOES
    // expose 9091) and TCP-poll like tunnel_grpc does for the gRPC
    // forwards. Second Lease lookup (CliCtx::open did one internally)
    // is one extra `kubectl get` — negligible vs. a 30s poll loop.
    let leader = scheduler_leader_pod().await?;
    let (metrics_port, _metrics_fwd) = port_forward(NS, &leader, 0, 9091).await?;
    crate::ui::poll(
        "scheduler metrics TCP accept",
        Duration::from_secs(2),
        10,
        || async {
            let s = tokio::net::TcpStream::connect(("127.0.0.1", metrics_port)).await;
            Ok(s.is_ok().then_some(()))
        },
    )
    .await?;

    let jsonl = dir.join("watch.jsonl");
    let mut out = OpenOptions::new().create(true).append(true).open(&jsonl)?;

    info!(
        "polling every {WATCH_INTERVAL:?}; appending to {}",
        jsonl.display()
    );
    info!("Ctrl-C to stop");

    loop {
        let ts = jiff::Timestamp::now().as_second();

        // Each probe is best-effort: a transient CLI error or metrics
        // blip renders as a string, not a watch-abort. Same philosophy
        // as status.rs — watch is useful precisely when things break.
        let builds = cli
            .run(&["--json", "builds", "--status", "active"])
            .unwrap_or_else(|e| format!("(rio-cli error: {e:#})"));

        let metrics = scrape_metrics(metrics_port).await;

        let top = std::process::Command::new("kubectl")
            .args(["top", "nodes", "--no-headers"])
            .output()
            .map_err(|e| format!("(kubectl top spawn error: {e})"))
            .and_then(|o| {
                String::from_utf8(o.stdout).map_err(|e| format!("(kubectl top non-utf8: {e})"))
            })
            .unwrap_or_else(|e| e);

        let snap = WatchSnapshot {
            ts,
            builds_active: &builds,
            scheduler_metrics: &metrics,
            top_nodes: &top,
        };
        // One line per snapshot. serde_json never emits a newline
        // mid-object, so a SIGINT between writeln! calls leaves a
        // valid (truncated) jsonl.
        writeln!(out, "{}", serde_json::to_string(&snap)?)?;
        out.flush()?;

        // Human one-liner to stderr.
        let active_count = builds.matches("\"build_id\"").count();
        let metrics_line = metrics.lines().take(2).collect::<Vec<_>>().join(" ");
        eprintln!(
            "{} [{}] active={} {}",
            style("·").dim(),
            jiff::Timestamp::from_second(ts)
                .map(|t| t.to_string())
                .unwrap_or_default(),
            active_count,
            style(metrics_line).dim()
        );

        tokio::time::sleep(WATCH_INTERVAL).await;
    }
}

/// GET the prometheus /metrics endpoint and keep just the rio_
/// scheduler lines. Full dump is ~400 lines; watch.jsonl only cares
/// about queue depth + derivation counters.
async fn scrape_metrics(port: u16) -> String {
    let url = format!("http://127.0.0.1:{port}/metrics");
    match reqwest::get(&url).await {
        Ok(r) => match r.text().await {
            Ok(body) => body
                .lines()
                .filter(|l| {
                    l.starts_with("rio_scheduler_fod_queue_depth")
                        || l.starts_with("rio_scheduler_derivations_queued")
                        || l.starts_with("rio_scheduler_builds_active")
                })
                .collect::<Vec<_>>()
                .join("\n"),
            Err(e) => format!("(metrics body read error: {e})"),
        },
        Err(e) => format!("(metrics GET {url} error: {e})"),
    }
}

fn find_session(explicit: Option<String>) -> Result<PathBuf> {
    let root = session_root();
    if let Some(s) = explicit {
        let d = root.join(&s);
        anyhow::ensure!(d.is_dir(), "session {s} not found at {}", d.display());
        return Ok(d);
    }
    // Most recent = highest unix-timestamp dirname.
    let mut dirs: Vec<_> = fs::read_dir(&root)
        .with_context(|| {
            format!(
                "no sessions dir at {} (run `stress run` first)",
                root.display()
            )
        })?
        .filter_map(|e| e.ok().filter(|e| e.path().is_dir()))
        .map(|e| e.path())
        .collect();
    dirs.sort();
    dirs.pop()
        .with_context(|| format!("no sessions in {} (run `stress run` first)", root.display()))
}

// ─── cleanup ────────────────────────────────────────────────────────

// No progress bars here: plain iteration + kill + print.
#[allow(clippy::print_stderr)]
async fn cmd_cleanup(all: bool) -> Result<()> {
    let sessions = iter_sessions()?;
    if sessions.is_empty() {
        eprintln!("{} no sessions", style("·").dim());
        return Ok(());
    }

    let mut killed = 0usize;
    let mut removed = 0usize;
    let mut chaos_remediated = 0usize;

    for (dir, pids) in sessions {
        let name = session_name(&dir);

        // Chaos remediation first: if chaos.json has entries, the
        // chaos pod might still be holding iptables DROP rules on
        // worker nodes (xtask was SIGKILLed mid-chaos). Spawn a
        // one-shot remediation pod per affected node, then proceed
        // with the normal PID cleanup. Best-effort — kube errors warn
        // but don't abort the rest of cleanup.
        match chaos::remediate(&dir).await {
            Ok((n, true)) => {
                eprintln!(
                    "{} session {} chaos: remediated {} node(s)",
                    style("✓").green(),
                    name,
                    n,
                );
                chaos_remediated += n;
            }
            Ok((_, false)) => {} // no chaos.json — normal session
            Err(e) => warn!("session {name} chaos remediation: {e:#}"),
        }

        let entries: Vec<_> = pids
            .tunnels
            .iter()
            .map(|e| ("tunnel", *e))
            .chain(pids.builds.iter().map(|e| ("build", *e)))
            .collect();

        let alive: Vec<_> = entries.iter().filter(|(_, e)| pid_alive(e.pid)).collect();

        if alive.is_empty() {
            fs::remove_dir_all(&dir)?;
            removed += 1;
            eprintln!(
                "{} session {} (all {} PIDs dead)",
                style("✓").green(),
                name,
                entries.len()
            );
            continue;
        }

        if !all {
            eprintln!(
                "{} session {} ({} alive — pass --all to kill)",
                style("·").yellow(),
                name,
                alive.len()
            );
            for (kind, e) in &alive {
                eprintln!("    {kind}[{}] pid={}", e.port, e.pid);
            }
            continue;
        }

        // --all: terminate everything.
        eprintln!(
            "{} session {} (killing {} alive)",
            style("▸").red(),
            name,
            alive.len()
        );
        for (kind, e) in &alive {
            if kill_graceful(e.pid) {
                eprintln!("    killed {kind}[{}] pid={}", e.port, e.pid);
                killed += 1;
            } else {
                warn!("{kind}[{}] pid={} survived SIGKILL", e.port, e.pid);
            }
        }

        if all_dead(&pids) {
            fs::remove_dir_all(&dir)?;
            removed += 1;
        }
    }

    eprintln!();
    let chaos_suffix = if chaos_remediated > 0 {
        format!(", remediated {chaos_remediated} chaos node(s)")
    } else {
        String::new()
    };
    eprintln!(
        "{} killed {} processes, removed {} sessions{chaos_suffix}",
        style("✓").green(),
        killed,
        removed
    );
    Ok(())
}

fn iter_sessions() -> Result<Vec<(PathBuf, SessionPids)>> {
    let root = session_root();
    if !root.exists() {
        return Ok(vec![]);
    }
    let mut out = vec![];
    for entry in fs::read_dir(&root)? {
        let entry = entry?;
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        // Unreadable pids.json → skip (might be a half-created session).
        match read_pids(&dir) {
            Ok(p) => out.push((dir, p)),
            Err(e) => warn!("session {}: {e:#}", dir.display()),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

fn all_dead(pids: &SessionPids) -> bool {
    pids.tunnels
        .iter()
        .chain(pids.builds.iter())
        .all(|e| !pid_alive(e.pid))
}

// ─── list ───────────────────────────────────────────────────────────

// No progress bars here: plain table print.
#[allow(clippy::print_stderr)]
fn cmd_list() -> Result<()> {
    let sessions = iter_sessions()?;
    if sessions.is_empty() {
        eprintln!("{} no sessions", style("·").dim());
        return Ok(());
    }

    for (dir, pids) in sessions {
        let name = session_name(&dir);
        let meta = read_meta(&dir).ok();

        let alive_tun = pids.tunnels.iter().filter(|e| pid_alive(e.pid)).count();
        let alive_bld = pids.builds.iter().filter(|e| pid_alive(e.pid)).count();

        let header = match &meta {
            Some(m) => format!(
                "{} × {} ({})",
                style(m.parallel).cyan(),
                style(&m.target).bold(),
                m.provider
            ),
            None => style("(no meta.json)").dim().to_string(),
        };

        eprintln!(
            "{} {} — {} — tunnels {}/{} builds {}/{}",
            if alive_bld > 0 {
                style("●").green()
            } else {
                style("○").dim()
            },
            style(&name).cyan(),
            header,
            alive_tun,
            pids.tunnels.len(),
            alive_bld,
            pids.builds.len()
        );

        // Tail each build log's last non-empty line.
        for e in &pids.builds {
            let log = dir.join(format!("build-{}.log", e.port));
            let tail = tail_line(&log).unwrap_or_else(|| "(empty)".into());
            let marker = if pid_alive(e.pid) {
                style("·").green()
            } else {
                style("×").dim()
            };
            eprintln!("    {marker} [{:5}] {}", e.port, style(tail).dim());
        }
    }
    Ok(())
}

/// Last non-empty line of a file. Cheap scan — build logs are
/// line-oriented and capped at whatever `nix build` emits.
fn tail_line(path: &Path) -> Option<String> {
    let f = File::open(path).ok()?;
    std::io::BufReader::new(f)
        .lines()
        .map_while(Result::ok)
        .filter(|l| !l.trim().is_empty())
        .last()
}

// ─── tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meta_roundtrip() {
        let m = SessionMeta {
            target: "hello-shallow-32x".into(),
            parallel: 4,
            ports: vec![2250, 2251, 2252, 2253],
            start_time: 1_700_000_000,
            provider: "eks".into(),
        };
        let s = serde_json::to_string(&m).unwrap();
        let r: SessionMeta = serde_json::from_str(&s).unwrap();
        assert_eq!(r.target, "hello-shallow-32x");
        assert_eq!(r.parallel, 4);
        assert_eq!(r.ports, vec![2250, 2251, 2252, 2253]);
        assert_eq!(r.provider, "eks");
    }

    #[test]
    fn pids_roundtrip_via_disk() {
        // write_pids uses write-then-rename; verify the full disk path
        // (not just serde) so a schema change that breaks read_pids
        // shows up here.
        let dir = tempfile::tempdir().unwrap();
        let p = SessionPids {
            tunnels: vec![PidEntry {
                port: 2250,
                pid: 111,
            }],
            builds: vec![
                PidEntry {
                    port: 2250,
                    pid: 222,
                },
                PidEntry {
                    port: 2251,
                    pid: 333,
                },
            ],
        };
        write_pids(dir.path(), &p).unwrap();
        assert!(
            !dir.path().join("pids.json.tmp").exists(),
            "tmp file left behind"
        );
        let r = read_pids(dir.path()).unwrap();
        assert_eq!(r.tunnels.len(), 1);
        assert_eq!(r.tunnels[0].pid, 111);
        assert_eq!(r.builds.len(), 2);
        assert_eq!(r.builds[1].port, 2251);
    }

    #[test]
    fn read_pids_missing_is_default() {
        // cleanup must tolerate a session dir where run() was SIGKILLed
        // between mkdir and the first write_pids().
        let dir = tempfile::tempdir().unwrap();
        let p = read_pids(dir.path()).unwrap();
        assert!(p.tunnels.is_empty());
        assert!(p.builds.is_empty());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn self_pid_is_alive() {
        assert!(pid_alive(std::process::id()));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn bogus_pid_is_dead() {
        // kill(0, 0) means "probe my own process GROUP", not PID 0 —
        // that returns Ok (current process exists). Use a high PID
        // instead: pid_max defaults to 2^22 on 64-bit, so i32::MAX is
        // safely unoccupied.
        assert!(!pid_alive(i32::MAX as u32));
    }

    #[test]
    fn all_dead_empty_is_true() {
        // Edge case: a session dir with no PIDs (SIGKILL during run()'s
        // init) should be reapable.
        assert!(all_dead(&SessionPids::default()));
    }

    #[test]
    fn tail_line_picks_last_nonempty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("log");
        fs::write(&p, "first\nsecond\n\nthird\n\n").unwrap();
        assert_eq!(tail_line(&p).as_deref(), Some("third"));
    }

    #[test]
    fn tail_line_missing_is_none() {
        assert_eq!(tail_line(Path::new("/nonexistent/log")), None);
    }
}
