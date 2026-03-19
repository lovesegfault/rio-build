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
use std::time::Duration;

use anyhow::anyhow;
use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use rio_proto::types::{
    BuildInfo, ClusterStatusResponse, CreateTenantRequest, ListBuildsRequest, ListWorkersRequest,
    TenantInfo, WorkerInfo,
};

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
async fn rpc<T>(
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
fn json<T: Serialize>(v: &T) -> anyhow::Result<()> {
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
