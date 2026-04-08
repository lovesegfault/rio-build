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
use rio_proto::types::{AddUpstreamRequest, ListUpstreamsRequest, RemoveUpstreamRequest};

use crate::{json, rpc};

/// Validate a `name:base64pubkey` entry in nix `trusted-public-keys`
/// shape. Format-only check: a 32-byte ed25519 key base64-encodes to 44
/// chars (with `=` pad) or 43 (without). Full decode happens
/// server-side; this catches the common typos (missing colon,
/// truncated paste) before the RPC so the operator sees the error
/// immediately instead of every future substitution silently failing.
fn validate_pubkey_entry(entry: &str) -> anyhow::Result<()> {
    let (name, b64) = entry.split_once(':').ok_or_else(|| {
        anyhow::anyhow!(
            "trusted-key '{entry}': expected `name:base64pubkey` \
             (e.g. `cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=`)"
        )
    })?;
    if name.is_empty() {
        anyhow::bail!("trusted-key '{entry}': empty key name before ':'");
    }
    let b64 = b64.trim();
    if !matches!(b64.len(), 43 | 44) {
        anyhow::bail!(
            "trusted-key '{name}': pubkey part is {} chars, expected 43–44 \
             (32-byte ed25519 key, base64). Check for truncation.",
            b64.len()
        );
    }
    if let Some(bad) = b64
        .bytes()
        .find(|c| !(c.is_ascii_alphanumeric() || matches!(c, b'+' | b'/' | b'=')))
    {
        anyhow::bail!("trusted-key '{name}': non-base64 byte {bad:#04x} in pubkey part");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_pubkey_entry;

    #[test]
    fn pubkey_valid_nixos_org() {
        validate_pubkey_entry("cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=")
            .unwrap();
    }

    #[test]
    fn pubkey_rejects_missing_colon() {
        assert!(validate_pubkey_entry("cache.nixos.org-1").is_err());
    }

    #[test]
    fn pubkey_rejects_truncated() {
        assert!(validate_pubkey_entry("foo:6NCHdD59X431").is_err());
    }

    #[test]
    fn pubkey_rejects_non_base64() {
        assert!(validate_pubkey_entry("foo:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDS!jY=").is_err());
    }
}

/// I-093: accept either a tenant name or a UUID. UUID is passed through
/// unchanged (no scheduler dependency). A non-UUID is resolved against
/// `AdminService::ListTenants` — keeps the store-only fast path
/// scheduler-free, only requires scheduler reachability when the
/// operator passes a name.
async fn resolve_tenant(tenant: String, scheduler_addr: &str) -> anyhow::Result<String> {
    if uuid::Uuid::parse_str(&tenant).is_ok() {
        return Ok(tenant);
    }
    let mut ac = rio_proto::client::connect_admin(scheduler_addr)
        .await
        .map_err(|e| {
            anyhow::anyhow!(
                "tenant '{tenant}' is not a UUID; name lookup needs scheduler at \
                 {scheduler_addr}: {e}"
            )
        })?;
    let resp = rpc("ListTenants", async || ac.list_tenants(()).await).await?;
    resp.tenants
        .into_iter()
        .find(|t| t.tenant_name == tenant)
        .map(|t| t.tenant_id)
        .ok_or_else(|| anyhow::anyhow!("no tenant named '{tenant}' (try `rio-cli list-tenants`)"))
}

#[derive(Args, Clone)]
pub struct UpstreamArgs {
    #[command(subcommand)]
    pub cmd: UpstreamCmd,
}

// r[impl store.substitute.upstream]
// r[impl cli.cmd.upstream]
#[derive(Subcommand, Clone)]
pub enum UpstreamCmd {
    /// List configured upstream caches for a tenant (priority order).
    List {
        /// Tenant name or UUID.
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
        /// Tenant name or UUID.
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
        /// Tenant name or UUID.
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
    scheduler_addr: &str,
    cmd: UpstreamCmd,
) -> anyhow::Result<()> {
    match cmd {
        UpstreamCmd::List { tenant } => {
            let tenant = resolve_tenant(tenant, scheduler_addr).await?;
            let req = ListUpstreamsRequest { tenant_id: tenant };
            let resp = rpc("ListUpstreams", async || {
                client.list_upstreams(req.clone()).await
            })
            .await?;
            if as_json {
                json(&resp.upstreams)?;
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
            for k in &trusted_keys {
                validate_pubkey_entry(k)?;
            }
            let tenant = resolve_tenant(tenant, scheduler_addr).await?;
            let req = AddUpstreamRequest {
                tenant_id: tenant,
                url,
                priority,
                trusted_keys,
                sig_mode,
            };
            let info = rpc("AddUpstream", async || {
                client.add_upstream(req.clone()).await
            })
            .await?;
            if as_json {
                json(&info)?;
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
            let tenant = resolve_tenant(tenant, scheduler_addr).await?;
            let req = RemoveUpstreamRequest {
                tenant_id: tenant,
                url: url.clone(),
            };
            rpc("RemoveUpstream", async || {
                client.remove_upstream(req.clone()).await
            })
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
