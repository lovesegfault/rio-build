//! `xtask k8s replay` — replay a recorded build-load archive against a rio
//! deployment at the recorded cadence and compare outcomes against what the
//! recording says was built.
//!
//! See `docs/dev/2026-05-24-xtask-k8s-replay-design.md` for the design and
//! the archive-format compatibility contract. The sibling modules implement
//! the engine; this module owns the CLI surface and the orchestration
//! pipeline: open archive → schedule → supply context → endpoint/pool →
//! prewarm → timeline → comparison → report (and the offline `--dry-run`
//! short-circuit).

use std::collections::BTreeSet;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use tokio::sync::RwLock;

use crate::config::XtaskConfig;
use crate::k8s::provider::{Provider, ProviderKind};
use crate::ui;

mod archive;
mod client;
mod compare;
mod prewarm;
mod report;
mod substituter;
mod supply;
mod timeline;

use archive::ReplayArchive;
use client::{Endpoint, GatewayPool, HostKeyPolicy, default_connections};
use compare::{DivergenceLog, VerdictCounts, classify_request};
use prewarm::{PrewarmConfig, build_supply_context};
use substituter::Substituter;
use supply::{PathSource, UploadClaims, resolve_source};
use timeline::{InFlightTracker, ScheduledRequest, TimelineConfig, build_schedule, run_timeline};

/// Exit-code policy for a replay run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum FailOn {
    /// Always exit 0 (unless the run itself errored).
    None,
    /// Exit nonzero if any regression, upload rejection, or request error occurred.
    Regression,
    /// Exit nonzero if any divergence at all occurred.
    Divergence,
}

impl std::fmt::Display for FailOn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Lowercase to match the clap ValueEnum rendering (--fail-on regression).
        clap::ValueEnum::to_possible_value(self)
            .expect("no skipped variants")
            .get_name()
            .fmt(f)
    }
}

#[derive(Debug, clap::Args)]
pub struct ReplayArgs {
    /// Path to the replay archive: a `.dwarfs` image or an unpacked archive
    /// directory.
    #[arg(long)]
    pub archive: PathBuf,

    /// Time-compression factor (> 0). 2.0 replays a 1-hour window in 30 min.
    #[arg(long, default_value_t = 1.0)]
    pub speedup: f64,

    /// Maximum concurrent in-flight requests (one SSH channel + daemon
    /// session each).
    #[arg(long, default_value_t = 32)]
    pub max_sessions: usize,

    /// SSH connections to spread channels over. Default: ceil(max_sessions/4)
    /// (the gateway allows 4 concurrent channels per connection).
    #[arg(long)]
    pub connections: Option<usize>,

    /// Substituters the target can reach on its own; paths covered by any of
    /// these are not uploaded. Repeatable.
    #[arg(long = "target-substituter", default_values_t = vec!["https://cache.nixos.org".to_string()])]
    pub target_substituters: Vec<String>,

    /// Consecutive failed rebuilds required before a recorded-success build
    /// failure is reported as a regression.
    #[arg(long, default_value_t = 3)]
    pub confirm_regressions: u32,

    /// Skip the bulk pre-supply phase; dependencies are then uploaded
    /// per-request inside the timeline (lower timing fidelity). Substituter
    /// coverage probes still run — the per-request supply ladder needs them.
    #[arg(long)]
    pub no_prewarm: bool,

    /// Do not replay recorded client disconnects; wait for those builds to
    /// finish instead.
    #[arg(long)]
    pub no_disconnect_replay: bool,

    /// Resolve everything and run the timeline without connecting to any
    /// cluster or network.
    #[arg(long)]
    pub dry_run: bool,

    /// Replay only the first N requests (by recorded offset).
    #[arg(long)]
    pub limit: Option<usize>,

    /// Print a scheduler-metrics line every 30s during the run.
    #[arg(long)]
    pub watch: bool,

    /// Bypass the provider tunnel and connect to this `ssh-ng://host:port`
    /// endpoint instead.
    #[arg(long)]
    pub store: Option<String>,

    /// SSH private key for `--store` targets (default: the deploy key).
    #[arg(long)]
    pub ssh_key: Option<PathBuf>,

    /// Pinned host key (path to a public-key file or a `SHA256:…`
    /// fingerprint) for non-loopback `--store` targets.
    #[arg(long)]
    pub ssh_host_key: Option<String>,

    /// Exit-code policy.
    #[arg(long, value_enum, default_value_t = FailOn::None)]
    pub fail_on: FailOn,

    /// Directory for run artifacts (summary.json, divergences.jsonl).
    /// Default: `.stress-test/replay/<unix-ts>/`
    #[arg(long)]
    pub report_dir: Option<PathBuf>,
}

pub async fn run(
    args: ReplayArgs,
    provider: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
) -> Result<()> {
    validate_args(&args)?;

    // Everything the run leaves behind (summary.json, divergences.jsonl)
    // lands here. No per-run tracing file layer: xtask has no precedent for
    // one (ui::init owns the global stderr subscriber); the console plus the
    // JSON artifacts are the run record.
    let report_dir = match &args.report_dir {
        Some(dir) => dir.clone(),
        None => crate::sh::repo_root()
            .join(".stress-test/replay")
            .join(jiff::Timestamp::now().as_second().to_string()),
    };
    std::fs::create_dir_all(&report_dir)
        .with_context(|| format!("create report directory {}", report_dir.display()))?;
    tracing::info!(report_dir = %report_dir.display(), "replay artifacts directory");

    // Open the archive: metadata is parsed eagerly, payloads stay lazy.
    let archive = ui::step("replay: open archive", || {
        let path = args.archive.clone();
        async move {
            tokio::task::spawn_blocking(move || ReplayArchive::open(&path))
                .await
                .context("the archive open task panicked or was cancelled")?
        }
    })
    .await?;
    let archive = Arc::new(archive);

    let manifest = archive.manifest();
    tracing::info!(
        requests = archive.requests().len(),
        recorded_drvs = manifest.drvs,
        embedded_srcs = manifest.embedded_srcs,
        fat = manifest.fat,
        created_at = %manifest.created_at,
        recorded_target_substituters = ?manifest.target_substituters,
        demoted_impure_drvs = archive.impure_env().len(),
        "replay archive opened"
    );
    if manifest.requests != archive.requests().len() as u64 {
        tracing::warn!(
            manifest = manifest.requests,
            parsed = archive.requests().len(),
            "manifest request count disagrees with requests.jsonl"
        );
    }
    if !archive.has_builds() {
        tracing::warn!(
            "the archive has no builds.jsonl — outcome validation is disabled \
             (every replayed build will be reported as a skip)"
        );
    }

    if args.dry_run {
        run_dry(&args, &archive, &report_dir).await?;
        return Ok(());
    }

    run_live(args, provider, kind, cfg, archive, report_dir).await
}

/// CLI sanity checks that don't need the archive.
fn validate_args(args: &ReplayArgs) -> Result<()> {
    ensure!(
        args.speedup > 0.0 && args.speedup.is_finite(),
        "--speedup must be a positive number"
    );
    ensure!(
        args.ssh_host_key.is_none() || args.store.is_some(),
        "--ssh-host-key only makes sense together with --store"
    );
    ensure!(
        args.ssh_key.is_none() || args.store.is_some(),
        "--ssh-key only makes sense together with --store"
    );
    ensure!(args.max_sessions >= 1, "--max-sessions must be at least 1");
    if let Some(connections) = args.connections {
        ensure!(connections >= 1, "--connections must be at least 1");
    }
    if let Some(limit) = args.limit {
        ensure!(limit >= 1, "--limit must be at least 1");
    }
    Ok(())
}

/// `--dry-run`: build the schedule and the supply context fully offline (no
/// substituter probes, no cluster), report what the replay WOULD do, and
/// write the summary artifacts. This is the path the integration test runs.
async fn run_dry(
    args: &ReplayArgs,
    archive: &Arc<ReplayArchive>,
    report_dir: &Path,
) -> Result<report::Summary> {
    let schedule = build_schedule(
        archive.requests(),
        archive.builds(),
        args.speedup,
        args.limit,
        !args.no_disconnect_replay,
    );
    let roots = schedule_roots(&schedule);

    // Empty substituter slices: no coverage probes, no relay resolution —
    // the context build stays entirely inside the archive.
    let context_started = Instant::now();
    let (ctx, closure) = build_supply_context(archive, &roots, &[], &[], 1).await?;
    let context_build_secs = context_started.elapsed().as_secs_f64();

    // Offline source resolution over the union closure: without probes the
    // supply ladder can only answer workload / embedded / unknown.
    let closure_drvs: BTreeSet<&str> = closure
        .topo
        .iter()
        .map(|node| node.drv_path.as_str())
        .collect();
    let mut workload_built = 0usize;
    let mut embedded_uploadable = 0usize;
    let mut unresolved = 0usize;
    for path in &closure.all_paths {
        if closure_drvs.contains(path.as_str()) {
            continue;
        }
        match resolve_source(
            path,
            &ctx.workload_outputs,
            &ctx.target_coverage,
            archive,
            &ctx.relay_narinfos,
        ) {
            PathSource::NotSupplied { workload: true } => workload_built += 1,
            PathSource::Archive => embedded_uploadable += 1,
            PathSource::NotSupplied { workload: false } => unresolved += 1,
            // Unreachable offline: there are no probe results to point at a
            // substituter.
            PathSource::TargetSubstituter | PathSource::Relay { .. } => {}
        }
    }

    let first_due = schedule.first().map(|s| s.due).unwrap_or_default();
    let last_due = schedule.last().map(|s| s.due).unwrap_or_default();
    println!("replay dry-run plan");
    println!("  requests              {}", schedule.len());
    println!(
        "  due window            first {:.1}s, last {:.1}s (speedup {}x)",
        first_due.as_secs_f64(),
        last_due.as_secs_f64(),
        args.speedup
    );
    println!("  unique closure paths  {}", closure.all_paths.len());
    println!(
        "  derivations           {} (uploaded as drv text)",
        closure.topo.len()
    );
    println!(
        "  workload derivations  {} (recorded build outcomes the target must reproduce)",
        ctx.workload.drvs.len()
    );
    println!("  workload outputs      {workload_built} (built by the target, never supplied)");
    println!("  embedded uploadable   {embedded_uploadable}");
    println!("  unresolved offline    {unresolved} (a live run probes substituters for these)");

    let summary = report::Summary {
        archive: args.archive.display().to_string(),
        window_secs: window_secs(archive.manifest()),
        wall_clock_secs: 0.0,
        speedup: args.speedup,
        requests_total: schedule.len(),
        requests_replayed: 0,
        demoted_impure_drvs: archive.impure_env().len(),
        validation_enabled: archive.has_builds(),
        context_build_secs,
        prewarm: None,
        counts: VerdictCounts::default(),
        max_dispatch_lateness_ms: 0,
        fail_on: args.fail_on.to_string(),
        dry_run: true,
        report_dir: report_dir.display().to_string(),
    };
    let summary_path = report::write_summary_json(report_dir, &summary)?;
    tracing::info!(summary = %summary_path.display(), "wrote replay summary");
    println!("{}", report::render_console(&summary));
    Ok(summary)
}

/// The live path: resolve the gateway endpoint (provider tunnel or
/// `--store`), connect the SSH pool, prewarm, run the timeline, classify the
/// outcomes, and report.
async fn run_live(
    args: ReplayArgs,
    provider: &dyn Provider,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    archive: Arc<ReplayArchive>,
    report_dir: PathBuf,
) -> Result<()> {
    // Substituters. A target substituter that doesn't parse is a hard error
    // (coverage decisions depend on it); a recorded relay source that
    // doesn't parse only shrinks what can be relayed.
    let mut target_substituters = Vec::new();
    for url in &args.target_substituters {
        target_substituters.push(
            Substituter::parse(url)
                .await
                .with_context(|| format!("--target-substituter {url}"))?,
        );
    }
    let mut src_substituters = Vec::new();
    for url in &archive.manifest().src_substituters {
        // Plain-HTTP relay sources from the (untrusted) archive manifest are
        // both an integrity downgrade for relayed NARs and the cheapest SSRF
        // surface an archive could carry — only https:// and s3:// manifest
        // sources are honored.
        if url.trim_start().to_ascii_lowercase().starts_with("http://") {
            tracing::warn!(
                "ignoring recorded relay substituter {url}: plain http:// sources from the \
                 archive manifest are not honored (https:// or s3:// only)"
            );
            continue;
        }
        match Substituter::parse(url).await {
            Ok(substituter) => src_substituters.push(substituter),
            Err(err) => tracing::warn!(
                "skipping recorded substituter {url}: {err:#} — paths only available there \
                 cannot be relayed"
            ),
        }
    }
    // Announce the archive-sourced relay hosts up front, BEFORE any probe or
    // fetch traffic is issued, so an operator can see exactly which hosts
    // this archive will make the replay reach out to.
    if src_substituters.is_empty() {
        tracing::info!(
            "no relay substituters from the archive manifest — gaps will only be filled from \
             the archive itself or the target's own substituters"
        );
    } else {
        let urls: Vec<String> = src_substituters.iter().map(Substituter::url).collect();
        tracing::info!(
            "relay sources from the archive manifest: {} — narinfo probes and NAR fetches will \
             be issued to these hosts",
            urls.join(", ")
        );
    }

    // Schedule + run-wide supply context (closure walk, coverage probes,
    // relay narinfo resolution).
    let schedule = build_schedule(
        archive.requests(),
        archive.builds(),
        args.speedup,
        args.limit,
        !args.no_disconnect_replay,
    );
    let requests_total = schedule.len();
    let roots = schedule_roots(&schedule);
    let context_started = Instant::now();
    let (mut ctx, union_closure) = build_supply_context(
        &archive,
        &roots,
        &target_substituters,
        &src_substituters,
        PrewarmConfig::default().coverage_concurrency,
    )
    .await?;
    let context_build_secs = context_started.elapsed().as_secs_f64();

    // Gateway endpoint: explicit --store, or the provider's gateway tunnel
    // (held open for the whole run, exactly like `rsb`).
    let (endpoint, key_path, policy, _tunnel_guard) = if let Some(store) = &args.store {
        let endpoint = parse_store_url(store)?;
        let policy = match &args.ssh_host_key {
            Some(pin) => HostKeyPolicy::Pinned(pin.clone()),
            None if is_loopback_host(&endpoint.host) => HostKeyPolicy::AcceptLoopback,
            None => HostKeyPolicy::KnownHosts,
        };
        let key_path = match &args.ssh_key {
            Some(key) => key.clone(),
            None => crate::ssh::privkey_path(cfg)?,
        };
        (endpoint, key_path, policy, None)
    } else {
        let port = free_local_port()?;
        // Mirror `rsb`: reap anything a crashed previous run left listening
        // on the port, then hold the tunnel guard until the run ends.
        crate::k8s::shared::kill_port_listeners(port);
        let guard = ui::step(&format!("replay: tunnel to the {kind} gateway"), || {
            provider.tunnel(port)
        })
        .await?;
        (
            Endpoint {
                host: "127.0.0.1".to_string(),
                port,
            },
            crate::ssh::privkey_path(cfg)?,
            HostKeyPolicy::AcceptLoopback,
            Some(guard),
        )
    };

    // SSH connection pool (4 daemon channels per connection).
    let connections = args
        .connections
        .unwrap_or_else(|| default_connections(args.max_sessions));
    let pool_capacity = connections * client::CHANNELS_PER_CONNECTION;
    if pool_capacity < args.max_sessions {
        tracing::warn!(
            "--connections {connections} gives only {pool_capacity} daemon channels \
             ({} per connection); effective concurrency is capped there, below \
             --max-sessions {}",
            client::CHANNELS_PER_CONNECTION,
            args.max_sessions
        );
    }
    let pool = ui::step(
        &format!("replay: connect to gateway {endpoint} ({connections} connections)"),
        || GatewayPool::connect(&endpoint, connections, &key_path, policy),
    )
    .await?;
    let pool = Arc::new(pool);

    // Bulk pre-supply, unless --no-prewarm (the supply context above is kept
    // either way — the per-request ladder needs it).
    let prewarm_report = if args.no_prewarm {
        ui::step_skip(
            "replay: prewarm",
            "--no-prewarm (gaps upload per-request inside the timeline)",
        );
        None
    } else {
        let prewarm_cfg = PrewarmConfig::for_pool(&pool);
        Some(
            prewarm::run(
                &archive,
                &pool,
                &mut ctx,
                &union_closure,
                &src_substituters,
                &prewarm_cfg,
            )
            .await?,
        )
    };

    // Timeline: replay every request at its (speedup-scaled) recorded offset.
    let ctx = Arc::new(RwLock::new(ctx));
    let claims = Arc::new(UploadClaims::new());
    let tracker = Arc::new(InFlightTracker::new());
    let src_substituters = Arc::new(src_substituters);

    let heartbeat = spawn_heartbeat(Arc::clone(&tracker));
    let watch = args.watch.then(spawn_watch);

    let timeline_cfg = TimelineConfig {
        max_sessions: args.max_sessions,
        confirm_regressions: args.confirm_regressions,
        ..TimelineConfig::default()
    };
    let timeline_started = Instant::now();
    let outcomes = ui::step(
        &format!("replay: run timeline ({requests_total} requests)"),
        || {
            run_timeline(
                Arc::clone(&archive),
                Arc::clone(&pool),
                Arc::clone(&ctx),
                claims,
                Arc::clone(&src_substituters),
                schedule,
                Arc::clone(&tracker),
                timeline_cfg,
            )
        },
    )
    .await;
    let wall_clock_secs = timeline_started.elapsed().as_secs_f64();
    heartbeat.abort();
    if let Some(watch) = watch {
        watch.abort();
    }
    let outcomes = outcomes?;

    // Comparison: classify every derived path against the recording,
    // streaming divergence records as they are found.
    let mut divergence_log = DivergenceLog::create(&report_dir)?;
    let demoted: BTreeSet<String> = archive.impure_env().keys().cloned().collect();
    let mut counts = VerdictCounts::default();
    let mut max_lateness = Duration::ZERO;
    for outcome in &outcomes {
        let verdicts = classify_request(
            outcome,
            archive.builds(),
            &demoted,
            &mut counts,
            &mut divergence_log,
        )?;
        max_lateness = max_lateness.max(outcome.dispatch_lateness);
        // Verdicts come back in request order, i.e. aligned with `results`.
        for (derived, (_, verdict)) in outcome.results.iter().zip(&verdicts) {
            if verdict.is_divergence() {
                tracing::debug!(
                    request = outcome.index,
                    derived = %timeline::format_derived(&derived.drv_path, &derived.outputs),
                    verdict = verdict.label(),
                    "divergence"
                );
            }
        }
    }

    // Report. The summary is written and printed BEFORE the exit-code policy
    // can fail the run, so a red run still shows what happened.
    let summary = report::Summary {
        archive: args.archive.display().to_string(),
        window_secs: window_secs(archive.manifest()),
        wall_clock_secs,
        speedup: args.speedup,
        requests_total,
        requests_replayed: outcomes.len(),
        demoted_impure_drvs: archive.impure_env().len(),
        validation_enabled: archive.has_builds(),
        context_build_secs,
        prewarm: prewarm_report,
        counts,
        max_dispatch_lateness_ms: u64::try_from(max_lateness.as_millis()).unwrap_or(u64::MAX),
        fail_on: args.fail_on.to_string(),
        dry_run: false,
        report_dir: report_dir.display().to_string(),
    };
    let summary_path = report::write_summary_json(&report_dir, &summary)?;
    tracing::info!(
        summary = %summary_path.display(),
        divergences = %divergence_log.path().display(),
        "wrote replay artifacts"
    );
    println!("{}", report::render_console(&summary));

    if report::exit_code(args.fail_on, &summary.counts) != 0 {
        bail!(
            "replay finished with {} regressions / {} divergences / {} request errors (fail-on={})",
            summary.counts.regressions,
            summary.counts.divergences(),
            summary.counts.request_errors,
            args.fail_on
        );
    }
    Ok(())
}

/// Supply-context roots: the scheduled requests' drv paths, deduplicated, in
/// schedule order.
fn schedule_roots(schedule: &[ScheduledRequest]) -> Vec<String> {
    let mut roots: Vec<String> = Vec::new();
    for scheduled in schedule {
        for (drv_path, _outputs) in &scheduled.request.paths {
            if !roots.contains(drv_path) {
                roots.push(drv_path.clone());
            }
        }
    }
    roots
}

/// Recorded window length in seconds (manifest `to` − `from`).
fn window_secs(manifest: &archive::Manifest) -> f64 {
    manifest.to.duration_since(manifest.from).as_secs_f64()
}

/// Parse `--store ssh-ng://[user@]host[:port]` into an [`Endpoint`]. Other
/// URL forms (another scheme, a path, query parameters) are rejected — store
/// URI options like `ssh-key=` have dedicated flags here.
fn parse_store_url(store: &str) -> Result<Endpoint> {
    let url = reqwest::Url::parse(store)
        .with_context(|| format!("--store {store} is not a valid URL"))?;
    ensure!(
        url.scheme() == "ssh-ng",
        "--store must be an ssh-ng://host:port URL (got scheme {:?})",
        url.scheme()
    );
    ensure!(
        url.path().is_empty() || url.path() == "/",
        "--store must not carry a path (got {:?})",
        url.path()
    );
    ensure!(
        url.query().is_none() && url.fragment().is_none(),
        "--store must not carry query parameters or a fragment \
         (use --ssh-key / --ssh-host-key instead of store-URI options)"
    );
    if !url.username().is_empty() {
        tracing::warn!(
            "--store user {:?} is ignored — the gateway derives the tenant from the SSH key \
             comment",
            url.username()
        );
    }
    let host = url
        .host_str()
        .ok_or_else(|| anyhow!("--store {store} has no host"))?
        // IPv6 literals come back bracketed; the SSH client dials (host, port).
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_string();
    let port = url.port().unwrap_or(22);
    Ok(Endpoint { host, port })
}

/// Loopback test for the default host-key policy of `--store` targets:
/// `localhost` or a loopback IP literal.
fn is_loopback_host(host: &str) -> bool {
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<IpAddr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
}

/// Bind `127.0.0.1:0`, take the OS-assigned port, and release the listener.
/// The provider tunnel needs the port number up front (its readiness probe
/// reads the SSH banner on it), so kubectl's own `:0` support is not usable
/// here; the small release-to-rebind window is acceptable for a dev tool.
fn free_local_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0))
        .context("bind 127.0.0.1:0 to pick a free local port for the gateway tunnel")?;
    Ok(listener
        .local_addr()
        .context("read the picked local port")?
        .port())
}

/// Background task: every ~5s, log how many requests are in flight and which
/// entry has been in its stage the longest. Quiet while nothing is in
/// flight; aborted by the caller when the timeline finishes.
fn spawn_heartbeat(tracker: Arc<InFlightTracker>) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            let (in_flight, oldest) = tracker.snapshot();
            if in_flight == 0 {
                continue;
            }
            match oldest {
                Some((index, session, stage, age)) => tracing::info!(
                    in_flight,
                    oldest_request = index,
                    oldest_session = session,
                    oldest_stage = ?stage,
                    oldest_age_s = age.as_secs(),
                    "replay heartbeat"
                ),
                None => tracing::info!(in_flight, "replay heartbeat"),
            }
        }
    })
}

/// Background task for `--watch`: every 30s, scrape the scheduler leader's
/// metrics (the same helper `qa --load --watch` uses) and log one line. When
/// no kube client can be built (e.g. `--store` pointing at a host that is
/// not a provider cluster), warn once and stop.
fn spawn_watch() -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let client = match crate::k8s::client::client().await {
            Ok(client) => client,
            Err(err) => {
                tracing::warn!("--watch unavailable (no cluster client): {err:#}");
                return;
            }
        };
        let mut tick = tokio::time::interval(Duration::from_secs(30));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tick.tick().await;
            match crate::k8s::status::gather_scheduler_metrics(&client).await {
                Some(metrics) => tracing::info!(
                    derivations_queued = metrics.derivations_queued,
                    builder_queue = metrics.builder_queue_depth,
                    builder_util = metrics.builder_utilization,
                    fetcher_queue = metrics.fetcher_queue_depth,
                    fetcher_util = metrics.fetcher_utilization,
                    possible_freeze = metrics.possible_freeze,
                    "scheduler metrics"
                ),
                None => tracing::info!("scheduler metrics unavailable"),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Harness {
        #[command(flatten)]
        args: ReplayArgs,
    }

    #[test]
    fn defaults_parse() {
        let h = Harness::parse_from(["x", "--archive", "/tmp/a"]);
        assert_eq!(h.args.speedup, 1.0);
        assert_eq!(h.args.max_sessions, 32);
        assert!(h.args.connections.is_none());
        assert_eq!(h.args.target_substituters, vec!["https://cache.nixos.org"]);
        assert_eq!(h.args.confirm_regressions, 3);
        assert_eq!(h.args.fail_on, FailOn::None);
        assert!(!h.args.no_prewarm);
        assert!(!h.args.dry_run);
        assert!(h.args.limit.is_none());
    }

    #[test]
    fn fail_on_values_parse() {
        for (s, v) in [
            ("none", FailOn::None),
            ("regression", FailOn::Regression),
            ("divergence", FailOn::Divergence),
        ] {
            let h = Harness::parse_from(["x", "--archive", "/tmp/a", "--fail-on", s]);
            assert_eq!(h.args.fail_on, v);
            // Display round-trips to the CLI spelling (used in the summary
            // and the failure message).
            assert_eq!(v.to_string(), s);
        }
    }

    #[test]
    fn validate_args_rejects_bad_combinations() {
        let parse = |extra: &[&str]| {
            let mut argv = vec!["x", "--archive", "/tmp/a"];
            argv.extend_from_slice(extra);
            Harness::parse_from(argv).args
        };
        assert!(validate_args(&parse(&[])).is_ok());
        assert!(validate_args(&parse(&["--speedup", "0"])).is_err());
        assert!(validate_args(&parse(&["--max-sessions", "0"])).is_err());
        assert!(validate_args(&parse(&["--connections", "0"])).is_err());
        assert!(validate_args(&parse(&["--limit", "0"])).is_err());
        // --ssh-key / --ssh-host-key require --store.
        assert!(validate_args(&parse(&["--ssh-key", "/tmp/key"])).is_err());
        assert!(validate_args(&parse(&["--ssh-host-key", "SHA256:abc"])).is_err());
        assert!(
            validate_args(&parse(&[
                "--store",
                "ssh-ng://gw.example:2222",
                "--ssh-key",
                "/tmp/key",
                "--ssh-host-key",
                "SHA256:abc",
            ]))
            .is_ok()
        );
    }

    #[test]
    fn store_url_parsing() {
        let endpoint = parse_store_url("ssh-ng://gw.example.org:2222").unwrap();
        assert_eq!(endpoint.host, "gw.example.org");
        assert_eq!(endpoint.port, 2222);
        // Port defaults to 22; a user component is tolerated (and ignored).
        let endpoint = parse_store_url("ssh-ng://rio@10.0.0.5").unwrap();
        assert_eq!(endpoint.host, "10.0.0.5");
        assert_eq!(endpoint.port, 22);
        // Everything else is rejected.
        assert!(parse_store_url("ssh://gw.example.org:22").is_err());
        assert!(parse_store_url("ssh-ng://gw.example.org:22/path").is_err());
        assert!(parse_store_url("ssh-ng://gw.example.org:22?ssh-key=/k").is_err());
        assert!(parse_store_url("not a url").is_err());

        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("localhost"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("10.0.0.5"));
        assert!(!is_loopback_host("gw.example.org"));
    }

    /// `--dry-run` end to end on the committed fixture: completes fully
    /// offline, writes a parseable summary.json, and reports zero verdicts.
    #[tokio::test]
    async fn dry_run_on_fixture_completes_offline() {
        // Runtime env var, not compile-time env!() — see `fixture()` in archive.rs's tests.
        let fixture = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest"),
        )
        .join("tests/fixtures/replay/basic");
        let report_dir = tempfile::tempdir().unwrap();
        let h = Harness::parse_from([
            "x",
            "--archive",
            fixture.to_str().unwrap(),
            "--dry-run",
            "--report-dir",
            report_dir.path().to_str().unwrap(),
        ]);

        let archive = Arc::new(ReplayArchive::open(&fixture).unwrap());
        let summary = run_dry(&h.args, &archive, report_dir.path()).await.unwrap();

        assert!(summary.dry_run);
        assert_eq!(summary.requests_total, 4);
        assert_eq!(summary.counts.total(), 0);

        let summary_path = report_dir.path().join("summary.json");
        assert!(summary_path.exists(), "summary.json must be written");
        let parsed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&summary_path).unwrap()).unwrap();
        assert_eq!(parsed["requests_total"], 4);
        assert_eq!(parsed["dry_run"], true);
        assert_eq!(parsed["requests_replayed"], 0);
    }

    /// Documents how `tests/fixtures/replay/basic/` was produced and keeps it
    /// honest against the production parsers:
    ///
    /// - NAR-serializes the embedded source store path and prints the
    ///   `NarHash`/`NarSize` values that `narinfo/<hash>.narinfo` must carry
    ///   (`sha256:<nixbase32>` — the encoding `NarInfo` stores verbatim).
    /// - Parses all four fixture `.drv` files with the rio-nix ATerm parser.
    /// - Parses the committed narinfo and asserts it matches the recomputed
    ///   hash/size.
    ///
    /// Run with:
    /// `cargo nextest run -p xtask --run-ignored all -E 'test(fixture)'`
    #[test]
    #[ignore = "fixture generator"]
    fn fixture_archive_matches_rio_nix_parsers() {
        use sha2::{Digest, Sha256};

        // Runtime env var, not compile-time env!() — see `fixture()` in archive.rs's tests.
        let basic = std::path::PathBuf::from(
            std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest"),
        )
        .join("tests/fixtures/replay/basic");

        // Embedded source store path → NAR hash/size for the narinfo.
        let src = basic.join("nix/store/b1111111111111111111111111111111-src.txt");
        let mut nar = Vec::new();
        let nar_size =
            rio_nix::nar::dump_path_streaming(&src, &mut nar).expect("NAR-serialize fixture src");
        let digest: [u8; 32] = Sha256::digest(&nar).into();
        let nar_hash = format!("sha256:{}", rio_nix::store_path::nixbase32::encode(&digest));
        println!("NarHash: {nar_hash}");
        println!("NarSize: {nar_size}");

        // The fixture .drv files must parse with the production parser.
        for drv in [
            "a1111111111111111111111111111111-dep.drv",
            "a2222222222222222222222222222222-app.drv",
            "a3333333333333333333333333333333-impure.drv",
            "a4444444444444444444444444444444-cached.drv",
        ] {
            let text = std::fs::read_to_string(basic.join("nix/store").join(drv))
                .unwrap_or_else(|e| panic!("read {drv}: {e}"));
            rio_nix::derivation::Derivation::parse(&text)
                .unwrap_or_else(|e| panic!("{drv} must parse: {e}"));
        }

        // The committed narinfo must parse and carry the real hash/size.
        let narinfo_text =
            std::fs::read_to_string(basic.join("narinfo/b1111111111111111111111111111111.narinfo"))
                .expect("read fixture narinfo");
        let narinfo =
            rio_nix::narinfo::NarInfo::parse(&narinfo_text).expect("fixture narinfo must parse");
        assert_eq!(narinfo.nar_hash, nar_hash);
        assert_eq!(narinfo.nar_size, nar_size);
    }
}
