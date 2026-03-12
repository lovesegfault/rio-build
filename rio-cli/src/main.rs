//! rio-cli — thin admin CLI over the scheduler's `AdminService`.
//!
//! Intended for `kubectl exec deploy/rio-scheduler -- rio-cli <cmd>`:
//! the scheduler pod already has `RIO_TLS__*` env vars set and certs
//! mounted at `/etc/rio/tls/`, so rio-cli picks up mTLS config for free
//! and talks to `localhost:9001`. Standalone use (from a laptop via
//! port-forward) also works — just set `RIO_SCHEDULER_ADDR` and, if
//! the scheduler has mTLS on, point `RIO_TLS__{CERT,KEY,CA}_PATH` at
//! a client cert signed by the same CA.

use clap::{Parser, Subcommand};
use serde::{Deserialize, Serialize};

use rio_proto::types::{
    ClusterStatusResponse, CreateTenantRequest, ListBuildsRequest, ListWorkersRequest, TenantInfo,
};

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

// rio-cli's CLI surface is the subcommand enum; the only cross-cutting
// flag is `--scheduler-addr`. TLS is env-only (RIO_TLS__*) — no flags,
// because the in-pod case (the common case) sets it via env anyway and
// three cert-path flags would clutter --help.
#[derive(Parser, Serialize, Default)]
#[command(name = "rio-cli", about = "Admin CLI for rio-build")]
struct CliArgs {
    /// Scheduler gRPC address (host:port). Defaults to localhost:9001
    /// for in-pod use via `kubectl exec`.
    #[arg(long, global = true)]
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduler_addr: Option<String>,

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
            let resp = client
                .create_tenant(CreateTenantRequest {
                    tenant_name: name,
                    gc_retention_hours,
                    gc_max_store_bytes,
                    cache_token,
                })
                .await?
                .into_inner();
            let t = resp
                .tenant
                .ok_or_else(|| anyhow::anyhow!("CreateTenant returned no TenantInfo"))?;
            print_tenant(&t);
        }
        Cmd::ListTenants => {
            let resp = client.list_tenants(()).await?.into_inner();
            if resp.tenants.is_empty() {
                println!("(no tenants)");
            } else {
                for t in &resp.tenants {
                    print_tenant(t);
                }
            }
        }
        Cmd::Status => {
            let cs = client.cluster_status(()).await?.into_inner();
            print_status(&cs);
            // Worker and build detail lines below the summary — this is
            // what `docs/src/phases/phase4.md` means by "rio-cli status":
            // enough to eyeball that workers registered and builds landed.
            let workers = client
                .list_workers(ListWorkersRequest::default())
                .await?
                .into_inner();
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
            let builds = client
                .list_builds(ListBuildsRequest {
                    limit: 10,
                    ..Default::default()
                })
                .await?
                .into_inner();
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
    Ok(())
}

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
