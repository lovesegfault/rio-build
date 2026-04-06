//! `rio-cli upstream list|add|remove` — per-tenant upstream cache CRUD.
//!
//! Calls `StoreAdminService.{ListUpstreams,AddUpstream,RemoveUpstream}`
//! — the ONLY rio-cli subcommand that talks gRPC to the store instead
//! of the scheduler. Scheduler-proxying would add zero value here
//! (TriggerGC proxies because the scheduler populates `extra_roots`
//! from live-build state; upstream CRUD has no scheduler state to
//! merge).
//!
//! Separate module (not inline in `main.rs`) — same convention as
//! `gc.rs`/`status.rs`/`wps.rs`: keep `main.rs` deltas to enum
//! variant + match arm + mod decl only.

use clap::{Args, Subcommand};
use serde::Serialize;
use tonic::transport::Channel;

use rio_proto::StoreAdminServiceClient;
use rio_proto::types::{
    AddUpstreamRequest, ListUpstreamsRequest, RemoveUpstreamRequest, UpstreamInfo,
};

use crate::{json, rpc};

#[derive(Args, Clone)]
pub struct UpstreamArgs {
    #[command(subcommand)]
    pub cmd: UpstreamCmd,
}

// r[impl store.substitute.upstream]
#[derive(Subcommand, Clone)]
pub enum UpstreamCmd {
    /// List configured upstream caches for a tenant (priority order).
    List {
        /// Tenant UUID (from `rio-cli list-tenants`).
        #[arg(long)]
        tenant: String,
    },
    /// Add an upstream binary cache for a tenant.
    ///
    /// On `QueryPathInfo`/`GetPath` miss, rio-store tries each
    /// upstream in priority order, verifies at least one `Sig:` line
    /// against `--trusted-key`, and ingests the NAR. The Helm chart's
    /// `store.upstreamCaches` NetworkPolicy allowlist must cover the
    /// upstream's CIDR or the fetch is DENIED at the pod network layer.
    Add {
        /// Tenant UUID.
        #[arg(long)]
        tenant: String,
        /// Cache URL (e.g. `https://cache.nixos.org`).
        #[arg(long)]
        url: String,
        /// Lower tried first. Default 50 leaves room above and below
        /// for later re-ordering without renumbering everything.
        #[arg(long, default_value = "50")]
        priority: i32,
        /// Trusted public key (`name:base64pubkey`, nix
        /// `trusted-public-keys` shape). Repeatable. At least one
        /// `Sig:` line on the upstream narinfo must verify against
        /// one of these or the substitution is rejected.
        #[arg(long = "trusted-key")]
        trusted_keys: Vec<String>,
        /// Post-substitution signature handling: `keep` (store
        /// upstream sigs unchanged), `add` (upstream sigs + fresh
        /// rio signature), `replace` (rio signature only).
        #[arg(long, default_value = "keep")]
        sig_mode: String,
    },
    /// Remove an upstream cache for a tenant.
    Remove {
        /// Tenant UUID.
        #[arg(long)]
        tenant: String,
        /// Cache URL (exact match against `upstream list`).
        #[arg(long)]
        url: String,
    },
}

/// Run the `upstream` subcommand.
///
/// Takes a `StoreAdminServiceClient` (not `AdminServiceClient`) — the
/// caller connects to the store directly. `as_json` follows the same
/// convention as other subcommands: one JSON document to stdout.
pub(crate) async fn run(
    as_json: bool,
    client: &mut StoreAdminServiceClient<Channel>,
    cmd: UpstreamCmd,
) -> anyhow::Result<()> {
    match cmd {
        UpstreamCmd::List { tenant } => {
            let resp = rpc(
                "ListUpstreams",
                client.list_upstreams(ListUpstreamsRequest { tenant_id: tenant }),
            )
            .await?;
            if as_json {
                json(
                    &resp
                        .upstreams
                        .iter()
                        .map(UpstreamJson::from)
                        .collect::<Vec<_>>(),
                )?;
            } else if resp.upstreams.is_empty() {
                println!("(no upstreams configured)");
            } else {
                // Table header — fixed-width columns sized for typical
                // cache URLs. `priority` first (sort order), then url
                // (the identity), then the tuning knobs.
                println!("PRIO  {:<40}  SIG_MODE  KEYS", "URL");
                for u in &resp.upstreams {
                    println!(
                        "{:>4}  {:<40}  {:<8}  {}",
                        u.priority,
                        u.url,
                        u.sig_mode,
                        u.trusted_keys.len(),
                    );
                }
            }
        }
        UpstreamCmd::Add {
            tenant,
            url,
            priority,
            trusted_keys,
            sig_mode,
        } => {
            let info = rpc(
                "AddUpstream",
                client.add_upstream(AddUpstreamRequest {
                    tenant_id: tenant,
                    url,
                    priority,
                    trusted_keys,
                    sig_mode,
                }),
            )
            .await?;
            if as_json {
                json(&UpstreamJson::from(&info))?;
            } else {
                println!(
                    "added upstream {} (id={}, priority={}, sig_mode={}, {} key{})",
                    info.url,
                    info.id,
                    info.priority,
                    info.sig_mode,
                    info.trusted_keys.len(),
                    if info.trusted_keys.len() == 1 {
                        ""
                    } else {
                        "s"
                    },
                );
            }
        }
        UpstreamCmd::Remove { tenant, url } => {
            rpc(
                "RemoveUpstream",
                client.remove_upstream(RemoveUpstreamRequest {
                    tenant_id: tenant,
                    url: url.clone(),
                }),
            )
            .await?;
            if as_json {
                #[derive(Serialize)]
                struct RemovedJson<'a> {
                    url: &'a str,
                    removed: bool,
                }
                json(&RemovedJson {
                    url: &url,
                    removed: true,
                })?;
            } else {
                println!("removed upstream {url}");
            }
        }
    }
    Ok(())
}

/// JSON projection of `UpstreamInfo`. Same thin-wrapper pattern as
/// `TenantJson`/`ExecutorJson` in `main.rs` — stable CLI JSON surface
/// decoupled from proto evolution.
#[derive(Serialize)]
struct UpstreamJson<'a> {
    id: i32,
    tenant_id: &'a str,
    url: &'a str,
    priority: i32,
    trusted_keys: &'a [String],
    sig_mode: &'a str,
}
impl<'a> From<&'a UpstreamInfo> for UpstreamJson<'a> {
    fn from(u: &'a UpstreamInfo) -> Self {
        Self {
            id: u.id,
            tenant_id: &u.tenant_id,
            url: &u.url,
            priority: u.priority,
            trusted_keys: &u.trusted_keys,
            sig_mode: &u.sig_mode,
        }
    }
}
