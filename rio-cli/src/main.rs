//! rio-cli — thin admin CLI over the scheduler's `AdminService`.
//!
//! Intended for `kubectl exec deploy/rio-scheduler -- rio-cli <cmd>`:
//! the scheduler pod already has `RIO_TLS__*` env vars set and certs
//! mounted at `/etc/rio/tls/`, so rio-cli picks up mTLS config for free
//! and talks to `localhost:9001`. Standalone use (from a laptop via
//! port-forward) also works — just set `RIO_SCHEDULER_ADDR` and, if
//! the scheduler has mTLS on, point `RIO_TLS__{CERT,KEY,CA}_PATH` at
//! a client cert signed by the same CA.

use std::future::Future;
use std::io::Write;
use std::time::Duration;

use anyhow::anyhow;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use rio_proto::types::{
    BuildInfo, ClearPoisonRequest, ClusterStatusResponse, CreateTenantRequest, GcProgress,
    GcRequest, GetBuildLogsRequest, ListBuildsRequest, ListBuildsResponse, ListWorkersRequest,
    ListWorkersResponse, TenantInfo, WorkerInfo,
};

mod cutoffs;
mod wps;

/// Per-RPC deadline. AdminService RPCs used by rio-cli are all unary
/// and cheap (ClusterStatus, ListWorkers, ListBuilds, CreateTenant,
/// ListTenants) — 30s covers a scheduler under load. Unbounded `.await?`
/// here hangs the CLI forever when the scheduler is wedged (recovery
/// stuck, lease contention). Observed: `vm-test-run-rio-cli` 10-min
/// hang after remediations 01/08 shifted scheduler startup timing.
///
/// `connect_admin` already has a 10s CONNECT timeout (rio-proto
/// client/mod.rs CONNECT_TIMEOUT) — that bounds TCP SYN / handshake.
/// This bounds the RPC itself (scheduler accepted the connection but
/// the handler is blocked on something).
const RPC_TIMEOUT: Duration = Duration::from_secs(30);

/// Wrap an AdminService RPC call with timeout + error context.
///
/// Combines three fixes in one helper:
///   - RPC deadline (`RPC_TIMEOUT`) — bounds a wedged scheduler
///   - tonic::Status → anyhow with the RPC name + gRPC code (bare `?`
///     on a Status gives just "status: Unavailable" with no context)
///   - `.into_inner()` hoisted — callers get `T` directly
///
/// NOT used for streaming AdminService RPCs (TriggerGC, GetBuildLogs)
/// — those need per-message progress, not a whole-call deadline. If
/// a future subcommand adds one, wrap the stream-drain loop instead.
pub(crate) async fn rpc<T>(
    what: &str,
    fut: impl Future<Output = Result<tonic::Response<T>, tonic::Status>>,
) -> anyhow::Result<T> {
    match tokio::time::timeout(RPC_TIMEOUT, fut).await {
        Ok(Ok(resp)) => Ok(resp.into_inner()),
        Ok(Err(s)) => Err(anyhow!("{what}: {} ({:?})", s.message(), s.code())),
        Err(_elapsed) => Err(anyhow!(
            "{what}: timed out after {RPC_TIMEOUT:?} — scheduler wedged? \
             (connect succeeded; the RPC itself never completed)"
        )),
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    scheduler_addr: String,
    tls: rio_common::tls::TlsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            // In-pod default. `kubectl exec` into the scheduler pod →
            // AdminService is on the same port as SchedulerService.
            scheduler_addr: "localhost:9001".into(),
            tls: rio_common::tls::TlsConfig::default(),
        }
    }
}

// rio-cli's CLI surface is the subcommand enum; the cross-cutting
// flags are `--scheduler-addr` and `--json`. TLS is env-only
// (RIO_TLS__*) — no flags, because the in-pod case (the common case)
// sets it via env anyway and three cert-path flags would clutter --help.
#[derive(Parser, Serialize, Default)]
#[command(name = "rio-cli", about = "Admin CLI for rio-build")]
struct CliArgs {
    /// Scheduler gRPC address (host:port). Defaults to localhost:9001
    /// for in-pod use via `kubectl exec`.
    #[arg(long, global = true)]
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduler_addr: Option<String>,

    /// Machine-readable JSON output (one document per invocation). With
    /// `--json`, handlers print a single JSON object/array to stdout
    /// and nothing else — `| jq` pipelines are the intended consumer.
    /// Streaming subcommands (`logs`, `gc`) ignore this flag: logs are
    /// raw bytes anyway, and gc progress is line-oriented.
    #[arg(long, global = true)]
    #[serde(skip)]
    json: bool,

    #[command(subcommand)]
    #[serde(skip)]
    cmd: Option<Cmd>,
}

#[derive(Subcommand, Clone)]
enum Cmd {
    /// Create a tenant. The name maps to the SSH authorized_keys comment
    /// field — builds from keys with this comment are attributed here.
    CreateTenant {
        /// Tenant name (unique, non-empty after trim).
        name: String,
        /// GC retention period in hours. Build artifacts for this tenant
        /// are eligible for sweep after this many hours without access.
        #[arg(long)]
        gc_retention_hours: Option<u32>,
        /// Storage cap in bytes. Soft limit — GC targets this tenant
        /// more aggressively when exceeded.
        #[arg(long)]
        gc_max_store_bytes: Option<u64>,
        /// Bearer token for binary-cache HTTP access. Unset = no cache
        /// access for this tenant.
        #[arg(long)]
        cache_token: Option<String>,
    },
    /// List all tenants.
    ListTenants,
    /// Cluster status summary: workers, builds, queue depth.
    Status,
    /// Detailed worker list. `Status` shows a one-line summary per
    /// worker; this shows everything `ListWorkers` returns — features,
    /// size class, running/max slots — for operational drill-down.
    Workers {
        /// Filter by worker status: "alive", "draining", or empty for all.
        #[arg(long)]
        status: Option<String>,
    },
    /// List builds with optional filtering.
    Builds {
        /// Filter by build status: "pending", "active", "succeeded",
        /// "failed", "cancelled".
        #[arg(long)]
        status: Option<String>,
        /// Max results (server capped).
        #[arg(long, default_value = "50")]
        limit: u32,
    },
    /// Stream build logs for a derivation.
    ///
    /// The server keys its ring buffer on `derivation_path`, not
    /// `build_id` — the positional here is the drv path. `--build-id`
    /// is needed only for completed builds (S3 lookup path); for
    /// active builds the ring buffer serves logs by drv path alone.
    Logs {
        /// Full derivation store path (e.g. `/nix/store/abc-foo.drv`).
        drv_path: String,
        /// Build UUID. Only needed if the derivation is no longer in
        /// the active-build ring buffer (S3 archive lookup).
        #[arg(long)]
        build_id: Option<String>,
    },
    /// Trigger garbage collection. Scheduler proxies to the store
    /// after populating `extra_roots` from live builds, so in-flight
    /// outputs aren't swept. Progress is streamed line-by-line.
    Gc {
        /// Report what would be collected without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
    /// Clear poison state for a derivation so it can be re-scheduled.
    /// Idempotent: exits 0 on a non-poisoned hash (cleared=false),
    /// but exits nonzero on an empty/invalid hash (server rejects).
    PoisonClear {
        /// Derivation hash (the `drv_hash`, not the full store path).
        drv_hash: String,
    },
    /// Size-class cutoff status: configured vs effective (post-rebalancer
    /// EMA drift), per-class queue/running counts, sample-window size.
    /// Surfaces how far SITA-E has drifted from the static TOML config
    /// and whether each class has enough samples to trust its cutoff.
    Cutoffs,
    /// Inspect WorkerPoolSet CRs via the K8s apiserver (not gRPC).
    /// `get` lists WPSes; `describe` joins spec classes with live
    /// child WorkerPool replica counts + effective-cutoff status —
    /// the spec→child→replica chain kubectl can't show in one place.
    Wps(wps::WpsArgs),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = CliArgs::parse();
    let cmd = cli
        .cmd
        .clone()
        .ok_or_else(|| anyhow::anyhow!("no subcommand given (try --help)"))?;
    let as_json = cli.json;

    // kube-only subcommands — dispatched BEFORE the gRPC connect
    // below. `wps` talks to the K8s apiserver directly (via
    // KUBECONFIG / in-cluster config) and has no dependency on
    // the scheduler being reachable. Running `rio-cli wps describe`
    // when the scheduler is down (e.g., to diagnose why) must
    // work — it would not if we'd already `?`'d on connect_admin.
    //
    // Single-variant match so the `other => other` fallthrough
    // is exhaustive (adding a second kube-only subcommand = add
    // another arm here; doesn't disturb the gRPC match below).
    let cmd = match cmd {
        Cmd::Wps(args) => return wps::run(as_json, args).await,
        other => other,
    };

    let cfg: Config = rio_common::config::load("cli", cli)?;

    rio_proto::client::init_client_tls(
        rio_common::tls::load_client_tls(&cfg.tls)
            .map_err(|e| anyhow::anyhow!("TLS config: {e}"))?,
    );

    let mut client = rio_proto::client::connect_admin(&cfg.scheduler_addr)
        .await
        .map_err(|e| anyhow::anyhow!("connect to scheduler at {}: {e}", cfg.scheduler_addr))?;

    match cmd {
        Cmd::CreateTenant {
            name,
            gc_retention_hours,
            gc_max_store_bytes,
            cache_token,
        } => {
            let resp = rpc(
                "CreateTenant",
                client.create_tenant(CreateTenantRequest {
                    tenant_name: name,
                    gc_retention_hours,
                    gc_max_store_bytes,
                    cache_token,
                }),
            )
            .await?;
            let t = resp
                .tenant
                .ok_or_else(|| anyhow!("CreateTenant returned no TenantInfo"))?;
            if as_json {
                json(&TenantJson::from(&t))?;
            } else {
                print_tenant(&t);
            }
        }
        Cmd::ListTenants => {
            let resp = rpc("ListTenants", client.list_tenants(())).await?;
            if as_json {
                json(
                    &resp
                        .tenants
                        .iter()
                        .map(TenantJson::from)
                        .collect::<Vec<_>>(),
                )?;
            } else if resp.tenants.is_empty() {
                println!("(no tenants)");
            } else {
                for t in &resp.tenants {
                    print_tenant(t);
                }
            }
        }
        Cmd::Status => {
            // Three sequential RPCs. Gather ALL results before printing
            // anything — if `list_workers` or `list_builds` fails after
            // `cluster_status` succeeds, we'd otherwise print the summary
            // header and then bail, leaving output that looks truncated
            // rather than failed. `?` on each gather is fine: nothing
            // has been printed yet, so the error message is the only
            // output.
            let cs = rpc("ClusterStatus", client.cluster_status(())).await?;
            let workers = rpc(
                "ListWorkers",
                client.list_workers(ListWorkersRequest::default()),
            )
            .await?;
            let builds = rpc(
                "ListBuilds",
                client.list_builds(ListBuildsRequest {
                    limit: 10,
                    ..Default::default()
                }),
            )
            .await?;

            // All data in hand — now print. No `?` below this line in
            // the human path (the JSON path's one `?` is serde encode,
            // which only fails on unrepresentable floats — not a
            // partial-output hazard).
            if as_json {
                #[derive(Serialize)]
                struct StatusFull<'a> {
                    #[serde(flatten)]
                    summary: StatusJson,
                    workers: Vec<WorkerJson<'a>>,
                    builds: Vec<BuildJson<'a>>,
                }
                json(&StatusFull {
                    summary: StatusJson::from(&cs),
                    workers: workers.workers.iter().map(WorkerJson::from).collect(),
                    builds: builds.builds.iter().map(BuildJson::from).collect(),
                })?;
            } else {
                print_status(&cs);
                // Worker and build detail lines below the summary — this is
                // what `docs/src/phases/phase4.md` means by "rio-cli status":
                // enough to eyeball that workers registered and builds landed.
                for w in &workers.workers {
                    println!(
                        "  worker {} [{}] {}/{} builds, systems={}",
                        w.worker_id,
                        w.status,
                        w.running_builds,
                        w.max_builds,
                        w.systems.join(",")
                    );
                }
                if builds.total_count > 0 {
                    println!("recent builds ({} total):", builds.total_count);
                    for b in &builds.builds {
                        println!(
                            "  build {} [{:?}] {}/{} drv ({} cached)",
                            b.build_id,
                            b.state(),
                            b.completed_derivations,
                            b.total_derivations,
                            b.cached_derivations
                        );
                    }
                }
            }
        }
        Cmd::Workers { status } => {
            let resp = rpc(
                "ListWorkers",
                client.list_workers(ListWorkersRequest {
                    status_filter: status.unwrap_or_default(),
                }),
            )
            .await?;
            if as_json {
                json(&WorkersJson::from(&resp))?;
            } else if resp.workers.is_empty() {
                println!("(no workers)");
            } else {
                for w in &resp.workers {
                    print_worker(w);
                }
            }
        }
        Cmd::Builds { status, limit } => {
            let resp = rpc(
                "ListBuilds",
                client.list_builds(ListBuildsRequest {
                    status_filter: status.unwrap_or_default(),
                    limit,
                    ..Default::default()
                }),
            )
            .await?;
            if as_json {
                json(&BuildsJson::from(&resp))?;
            } else if resp.builds.is_empty() {
                println!("(no builds — {} total matching filter)", resp.total_count);
            } else {
                println!("{} builds ({} total):", resp.builds.len(), resp.total_count);
                for b in &resp.builds {
                    print_build(b);
                }
            }
        }
        Cmd::Logs { drv_path, build_id } => {
            // STREAMING — NOT via rpc() helper. The helper's 30s whole-
            // call deadline is wrong for log tails (a build can run for
            // an hour). Wrap just the initial call in the timeout —
            // once the stream is open, per-message receives have no
            // deadline (an active build may go minutes between log
            // lines; that's not a hang, that's a slow build).
            let mut stream = tokio::time::timeout(
                RPC_TIMEOUT,
                client.get_build_logs(GetBuildLogsRequest {
                    build_id: build_id.unwrap_or_default(),
                    derivation_path: drv_path,
                    since_line: 0,
                }),
            )
            .await
            .map_err(|_| anyhow!("GetBuildLogs: open timed out after {RPC_TIMEOUT:?}"))?
            .map_err(|s| anyhow!("GetBuildLogs: {} ({:?})", s.message(), s.code()))?
            .into_inner();

            // Drain. `lines` is `repeated bytes` — may be non-UTF-8
            // (build output can be arbitrary). Write raw bytes to
            // stdout, newline-terminated (the proto `lines` field
            // strips trailing newlines; re-add for human readability).
            // Lock stdout once — per-line `println!` would flush each
            // line through the global lock, and a verbose build can
            // emit thousands.
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            while let Some(chunk) = stream
                .message()
                .await
                .map_err(|s| anyhow!("GetBuildLogs: stream: {} ({:?})", s.message(), s.code()))?
            {
                for line in &chunk.lines {
                    out.write_all(line)?;
                    out.write_all(b"\n")?;
                }
            }
            out.flush()?;
        }
        Cmd::Gc { dry_run } => {
            // STREAMING — same open-timeout-only shape as Logs. A store
            // sweep can legitimately go silent for minutes between
            // progress messages (mark phase on a large store).
            let mut stream = tokio::time::timeout(
                RPC_TIMEOUT,
                client.trigger_gc(GcRequest {
                    dry_run,
                    ..Default::default()
                }),
            )
            .await
            .map_err(|_| anyhow!("TriggerGC: open timed out after {RPC_TIMEOUT:?}"))?
            .map_err(|s| anyhow!("TriggerGC: {} ({:?})", s.message(), s.code()))?
            .into_inner();

            // Drain progress. Print each message as it arrives so the
            // operator sees live progress during long sweeps. Terminal
            // message carries `is_complete=true` with final counts.
            let mut last: Option<GcProgress> = None;
            while let Some(p) = stream
                .message()
                .await
                .map_err(|s| anyhow!("TriggerGC: stream: {} ({:?})", s.message(), s.code()))?
            {
                if p.is_complete {
                    println!(
                        "GC {}: {} scanned, {} collected, {} bytes freed",
                        if dry_run {
                            "dry-run complete"
                        } else {
                            "complete"
                        },
                        p.paths_scanned,
                        p.paths_collected,
                        p.bytes_freed
                    );
                } else {
                    println!(
                        "  scanned={} collected={} freed={}B current={}",
                        p.paths_scanned, p.paths_collected, p.bytes_freed, p.current_path
                    );
                }
                last = Some(p);
            }
            // If the stream closed without an `is_complete` frame,
            // surface that — scheduler shutdown or store disconnect
            // mid-sweep. Not an error (the sweep may have completed
            // store-side; we just don't know), but worth flagging.
            if !last.map(|p| p.is_complete).unwrap_or(false) {
                eprintln!(
                    "warning: GC stream closed without is_complete — \
                     scheduler or store disconnected mid-sweep"
                );
            }
        }
        Cmd::PoisonClear { drv_hash } => {
            // Unary — rpc() helper applies. Server returns cleared=false
            // for non-poisoned or non-existent hashes (idempotent, per
            // spec). An empty hash is InvalidArgument; anything else
            // is Ok with a boolean.
            let resp = rpc(
                "ClearPoison",
                client.clear_poison(ClearPoisonRequest {
                    derivation_hash: drv_hash.clone(),
                }),
            )
            .await?;
            if as_json {
                #[derive(Serialize)]
                struct ClearedJson<'a> {
                    drv_hash: &'a str,
                    cleared: bool,
                }
                json(&ClearedJson {
                    drv_hash: &drv_hash,
                    cleared: resp.cleared,
                })?;
            } else if resp.cleared {
                println!("cleared poison for {drv_hash}");
            } else {
                println!("{drv_hash}: not poisoned (nothing to clear)");
            }
        }
        Cmd::Cutoffs => cutoffs::run(as_json, &mut client).await?,
        // Dispatched above (before gRPC connect). The early match
        // consumes the Wps variant and returns; this arm is reached
        // only if the dispatch order is broken — fail loud.
        //
        // NOT splitting Cmd into KubeCmd|GrpcCmd yet: single kube-
        // only variant, and the unreachable! fails loud on first
        // invocation (not silent). When a SECOND kube-only
        // subcommand lands (e.g., a WorkerPool describe that
        // doesn't need admin RPC), reconsider the split.
        Cmd::Wps(_) => unreachable!("Wps handled before gRPC connect"),
    }
    Ok(())
}

// ===========================================================================
// JSON output
// ===========================================================================
//
// Prost-generated types don't derive `serde::Serialize` (enabling it
// workspace-wide would be a blast-radius change: every rio crate's
// serialization surface shifts, `google.protobuf.Timestamp` needs
// `prost-wkt-types`, enum repr changes). Thin per-subcommand wrappers
// project just the fields a CLI consumer cares about — stable JSON
// surface under our control, not coupled to proto evolution.
//
// Timestamps (`prost_types::Timestamp`) and nested `ResourceUsage`
// are flattened / stringified rather than projected structurally —
// `rio-cli --json | jq` consumers want readable scalars, not
// `{seconds: N, nanos: M}` objects.

/// Print a serde value as pretty JSON. Single emission point so all
/// `--json` output is consistent (pretty, trailing newline, errors
/// propagate).
pub(crate) fn json<T: Serialize>(v: &T) -> anyhow::Result<()> {
    println!("{}", serde_json::to_string_pretty(v)?);
    Ok(())
}

#[derive(Serialize)]
struct TenantJson<'a> {
    tenant_id: &'a str,
    tenant_name: &'a str,
    gc_retention_hours: u32,
    gc_max_store_bytes: Option<u64>,
    has_cache_token: bool,
}
impl<'a> From<&'a TenantInfo> for TenantJson<'a> {
    fn from(t: &'a TenantInfo) -> Self {
        Self {
            tenant_id: &t.tenant_id,
            tenant_name: &t.tenant_name,
            gc_retention_hours: t.gc_retention_hours,
            gc_max_store_bytes: t.gc_max_store_bytes,
            has_cache_token: t.has_cache_token,
        }
    }
}

#[derive(Serialize)]
struct StatusJson {
    total_workers: u32,
    active_workers: u32,
    draining_workers: u32,
    pending_builds: u32,
    active_builds: u32,
    queued_derivations: u32,
    running_derivations: u32,
    store_size_bytes: u64,
}
impl From<&ClusterStatusResponse> for StatusJson {
    fn from(s: &ClusterStatusResponse) -> Self {
        Self {
            total_workers: s.total_workers,
            active_workers: s.active_workers,
            draining_workers: s.draining_workers,
            pending_builds: s.pending_builds,
            active_builds: s.active_builds,
            queued_derivations: s.queued_derivations,
            running_derivations: s.running_derivations,
            store_size_bytes: s.store_size_bytes,
        }
    }
}

#[derive(Serialize)]
struct WorkerJson<'a> {
    worker_id: &'a str,
    status: &'a str,
    systems: &'a [String],
    supported_features: &'a [String],
    max_builds: u32,
    running_builds: u32,
    size_class: &'a str,
}
impl<'a> From<&'a WorkerInfo> for WorkerJson<'a> {
    fn from(w: &'a WorkerInfo) -> Self {
        Self {
            worker_id: &w.worker_id,
            status: &w.status,
            systems: &w.systems,
            supported_features: &w.supported_features,
            max_builds: w.max_builds,
            running_builds: w.running_builds,
            size_class: &w.size_class,
        }
    }
}

#[derive(Serialize)]
struct BuildJson<'a> {
    build_id: &'a str,
    tenant_id: &'a str,
    /// `BuildState` enum stringified via prost's `as_str_name` — gives
    /// `"BUILD_STATE_ACTIVE"` etc. Proto-native naming so consumers
    /// can round-trip it through the proto enum if they need to.
    state: &'a str,
    priority_class: &'a str,
    total_derivations: u32,
    completed_derivations: u32,
    cached_derivations: u32,
    error_summary: &'a str,
}
impl<'a> From<&'a BuildInfo> for BuildJson<'a> {
    fn from(b: &'a BuildInfo) -> Self {
        Self {
            build_id: &b.build_id,
            tenant_id: &b.tenant_id,
            state: b.state().as_str_name(),
            priority_class: &b.priority_class,
            total_derivations: b.total_derivations,
            completed_derivations: b.completed_derivations,
            cached_derivations: b.cached_derivations,
            error_summary: &b.error_summary,
        }
    }
}

/// Top-level wrapper so `rio-cli workers --json | jq '.workers | length'`
/// works. A bare array would work too, but the named key future-proofs
/// for adding metadata (e.g. snapshot time) without a breaking change.
#[derive(Serialize)]
struct WorkersJson<'a> {
    workers: Vec<WorkerJson<'a>>,
}
impl<'a> From<&'a ListWorkersResponse> for WorkersJson<'a> {
    fn from(r: &'a ListWorkersResponse) -> Self {
        Self {
            workers: r.workers.iter().map(WorkerJson::from).collect(),
        }
    }
}

#[derive(Serialize)]
struct BuildsJson<'a> {
    builds: Vec<BuildJson<'a>>,
    total_count: u32,
}
impl<'a> From<&'a ListBuildsResponse> for BuildsJson<'a> {
    fn from(r: &'a ListBuildsResponse) -> Self {
        Self {
            builds: r.builds.iter().map(BuildJson::from).collect(),
            total_count: r.total_count,
        }
    }
}

// ===========================================================================
// Human-readable output
// ===========================================================================

fn print_tenant(t: &TenantInfo) {
    println!(
        "tenant {} ({})  gc_retention={}h  max_store={}  cache_token={}",
        t.tenant_name,
        t.tenant_id,
        t.gc_retention_hours,
        t.gc_max_store_bytes
            .map(|b| format!("{b}B"))
            .unwrap_or_else(|| "unlimited".into()),
        if t.has_cache_token { "yes" } else { "no" }
    );
}

fn print_status(s: &ClusterStatusResponse) {
    println!(
        "workers: {} total, {} active, {} draining",
        s.total_workers, s.active_workers, s.draining_workers
    );
    println!(
        "builds:  {} pending, {} active",
        s.pending_builds, s.active_builds
    );
    println!(
        "queue:   {} queued derivations, {} running",
        s.queued_derivations, s.running_derivations
    );
    println!("store:   {} bytes", s.store_size_bytes);
}

/// Detailed per-worker view for `rio-cli workers`. `Status` prints a
/// compact one-liner; this is the "show me everything" form for
/// debugging a specific worker's registration or feature advertisement.
fn print_worker(w: &WorkerInfo) {
    println!("worker {} [{}]", w.worker_id, w.status);
    println!("  slots:    {}/{} running", w.running_builds, w.max_builds);
    println!("  systems:  {}", w.systems.join(", "));
    if !w.supported_features.is_empty() {
        println!("  features: {}", w.supported_features.join(", "));
    }
    if !w.size_class.is_empty() {
        println!("  size:     {}", w.size_class);
    }
    if let Some(r) = &w.resources {
        println!(
            "  cpu={:.2}  mem={}/{}  disk={}/{}",
            r.cpu_fraction,
            r.memory_used_bytes,
            r.memory_total_bytes,
            r.disk_used_bytes,
            r.disk_total_bytes
        );
    }
}

fn print_build(b: &BuildInfo) {
    println!(
        "  build {} [{:?}] {}/{} drv ({} cached) tenant={} prio={}",
        b.build_id,
        b.state(),
        b.completed_derivations,
        b.total_derivations,
        b.cached_derivations,
        b.tenant_id,
        b.priority_class,
    );
    if !b.error_summary.is_empty() {
        println!("    error: {}", b.error_summary);
    }
}
