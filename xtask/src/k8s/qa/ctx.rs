//! Per-scenario execution context + ephemeral tenant pool + PG handle.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::try_join_all;
use serde::de::DeserializeOwned;
use sqlx::PgPool;
use tempfile::TempDir;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tracing::info;

use crate::k8s::eks::smoke::{self, CliCtx, step_restart_gateway, step_tenant, step_upstream};
use crate::k8s::shared::{self, ProcessGuard};
use crate::k8s::status::{SCHED_METRICS_PORT, Scrape, scrape_pod};
use crate::k8s::{NS, NS_BUILDERS, client as kube};
use crate::sh::{self, cmd};
use crate::ssh;

// ─── per-scenario context ──────────────────────────────────────────────

/// What every `Scenario::run()` receives. `cli`/`kube`/`cfg`/`pg` are
/// shared across the whole run (cheap clones); `tenants` is per-scenario
/// for `Isolation::Tenant { count }` — `count` slots allocated from the
/// pool.
pub struct QaCtx {
    pub kube: kube::Client,
    pub cli: Arc<CliCtx>,
    pub pg: Arc<PgPool>,
    /// `count` tenant slots for `Isolation::Tenant`; empty otherwise.
    /// Index 0 is the "primary" — single-tenant scenarios use
    /// `ctx.tenant(0)` / `ctx.nix_build_via_gateway(0, ...)`.
    pub tenants: Vec<Tenant>,
}

impl QaCtx {
    /// Tenant at `idx`. Panics if out of range — that's a scenario-author
    /// bug (asked for fewer in `Isolation::Tenant{count}` than indexed),
    /// not a runtime condition.
    pub fn tenant(&self, idx: usize) -> &Tenant {
        self.tenants
            .get(idx)
            .expect("tenant(idx) out of range for declared Isolation::Tenant{count}")
    }

    /// Direct PG pool. Runtime queries only — `sqlx::query!` macros
    /// can't be used (the cluster's schema isn't known at xtask build
    /// time). See [`PgHandle::open`] for connection setup.
    pub fn pg(&self) -> &PgPool {
        &self.pg
    }

    /// One scrape of the scheduler-leader's `/metrics`. Covers every
    /// `rio_scheduler_*` gauge/counter. The leader is resolved per
    /// call — if a scenario causes failover, subsequent scrapes follow
    /// the new leader.
    pub async fn scrape_scheduler(&self) -> Result<Scrape> {
        let leader = kube::scheduler_leader(&self.kube, NS).await?;
        let body = scrape_pod(&self.kube, NS, &leader, SCHED_METRICS_PORT).await?;
        Ok(Scrape::parse(&body))
    }

    /// `rio-cli --json <args>` parsed into `T`.
    pub fn cli_json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.cli.run(&full)?;
        serde_json::from_str(&out).with_context(|| format!("rio-cli {args:?}: {out}"))
    }

    /// Shell out to `kubectl`. Thin escape hatch for asserts the kube
    /// crate doesn't cover (pod exec, logs --since).
    pub fn kubectl(&self, args: &[&str]) -> Result<String> {
        let s = sh::shell()?;
        sh::try_read(cmd!(s, "kubectl {args...}"))
    }

    /// List running pods in `ns` matching `label_selector`.
    pub fn running_pods(&self, ns: &str, label_selector: &str) -> Result<Vec<String>> {
        let out = self.kubectl(&[
            "-n",
            ns,
            "get",
            "pods",
            "-l",
            label_selector,
            "--field-selector=status.phase=Running",
            "-o",
            "jsonpath={.items[*].metadata.name}",
        ])?;
        Ok(out.split_whitespace().map(String::from).collect())
    }

    /// Submit a trivial busybox build via the gateway as
    /// `self.tenants[tenant_idx]`, block until it completes. The build
    /// authenticates with that tenant's ephemeral SSH key, so it's
    /// attributed to the right tenant.
    pub async fn nix_build_via_gateway(
        &self,
        tenant_idx: usize,
        tag: &str,
        secs: u32,
        out_kb: u32,
    ) -> Result<()> {
        let key = self.tenant(tenant_idx).key.clone();
        gateway_build(key, smoke::smoke_expr(tag, secs, out_kb)).await
    }

    /// Spawn `nix_build_via_gateway` in the background. For asserts that
    /// observe scheduler state WHILE a build runs (i095, i163).
    pub fn nix_build_via_gateway_bg(
        &self,
        tenant_idx: usize,
        tag: &str,
        secs: u32,
        out_kb: u32,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let key = self.tenant(tenant_idx).key.clone();
        tokio::spawn(gateway_build(key, smoke::smoke_expr(tag, secs, out_kb)))
    }

    /// Submit an arbitrary Nix expression via the gateway as
    /// `self.tenants[tenant_idx]`. The expression must evaluate to a
    /// single derivation (`nix-instantiate --expr` is the front end).
    /// Unlocks scenarios that need `requiredSystemFeatures`, multi-
    /// output, custom name, etc. — everything `smoke_expr` can't shape.
    pub async fn nix_build_expr_via_gateway(&self, tenant_idx: usize, expr: &str) -> Result<()> {
        let key = self.tenant(tenant_idx).key.clone();
        gateway_build(key, expr.to_owned()).await
    }

    /// Background variant of `nix_build_expr_via_gateway`.
    #[allow(dead_code)] // sibling of _bg above; first user lands with i181-style asserts
    pub fn nix_build_expr_via_gateway_bg(
        &self,
        tenant_idx: usize,
        expr: &str,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let key = self.tenant(tenant_idx).key.clone();
        tokio::spawn(gateway_build(key, expr.to_owned()))
    }

    /// Scheduler-leader pod name. Several scenarios need this for log
    /// inspection independent of metric scraping.
    pub async fn scheduler_leader(&self) -> Result<String> {
        kube::scheduler_leader(&self.kube, NS).await
    }

    pub const BUILDER_LABEL: &str = "app.kubernetes.io/name=rio-builder";
    pub const NS_BUILDERS: &str = NS_BUILDERS;
}

/// Port-forward gateway:22, wait for SSH banner, run `build_expr`.
/// Shared body of all four `nix_build[_expr]_via_gateway[_bg]` so each
/// variant is a one-liner.
async fn gateway_build(key: PathBuf, expr: String) -> Result<()> {
    let (port, _guard) = shared::port_forward(NS, "svc/rio-gateway", 0, 22).await?;
    crate::ui::poll(
        "gateway SSH banner",
        Duration::from_secs(2),
        20,
        || async move {
            Ok(
                tokio::time::timeout(Duration::from_secs(2), smoke::ssh_banner(port))
                    .await
                    .ok()
                    .flatten(),
            )
        },
    )
    .await?;
    let store = format!(
        "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
        key.display()
    );
    smoke::build_expr(&expr, &store).await
}

// ─── PG handle ─────────────────────────────────────────────────────────

/// Process-lifetime PG connection. Built once in `scheduler::run()`;
/// scenarios get `Arc<PgPool>` via `QaCtx`. Guard (k3s in-cluster PG
/// only) drops with the handle.
pub struct PgHandle {
    pub pool: PgPool,
    _guard: Option<ProcessGuard>,
}

impl PgHandle {
    /// Fetch the `rio-postgres` Secret URL from `rio-system`, then
    /// [`Self::open_with_url`].
    pub async fn open(kube_client: &kube::Client) -> Result<Self> {
        let url = kube::get_secret_key(kube_client, NS, "rio-postgres", "url")
            .await?
            .context("Secret rio-postgres/url not found — was `up --deploy` run?")?;
        Self::open_with_url(&url).await
    }

    /// Connect to the cluster's PG via an in-cluster relay.
    ///
    /// RDS is in private VPC subnets and its SG only admits the EKS
    /// node SG — the operator's machine can't reach it directly. On
    /// k3s, bitnami PG is in-cluster. Either way: port-forward to
    /// something in `rio-system` that listens on 5432. On k3s that's
    /// `svc/rio-postgresql`. On EKS, we spawn an `alpine/socat` relay
    /// pod (5432 → RDS endpoint) and port-forward to THAT, then
    /// sqlx connects to `localhost:{bound}`.
    ///
    /// `wipe` calls this with a URL captured BEFORE `helm uninstall`
    /// (which removes the ExternalSecret-backed `rio-postgres` Secret).
    pub async fn open_with_url(url: &str) -> Result<Self> {
        let (host, port) = pg_host_port(url)?;
        let in_cluster = host.ends_with(&format!(".{NS}")) || host == "rio-postgresql";

        let (bound, guard) = if in_cluster {
            // k3s: bitnami PG is a Service in rio-system.
            shared::port_forward(NS, "svc/rio-postgresql", 0, 5432).await?
        } else {
            // EKS: RDS is VPC-private. Spawn a socat relay pod that
            // listens on 5432 and forwards to the RDS endpoint, then
            // port-forward to THAT pod.
            spawn_socat_relay(&host, port).await?
        };

        let url = rewrite_pg_host(url, &format!("localhost:{bound}"));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .context("connect to cluster PG via port-forward")?;
        info!(
            "qa pg handle open ({}, {})",
            if in_cluster {
                "port-forward svc"
            } else {
                "socat-relay"
            },
            host
        );
        Ok(Self {
            pool,
            _guard: Some(guard),
        })
    }
}

/// Spawn an `alpine/socat` pod in `rio-system` that listens on 5432 and
/// forwards to `host:port` (the RDS endpoint), wait for it to be
/// Running, port-forward to it. Named `rio-qa-pg-relay`; pre-deletes
/// any prior instance so re-runs are idempotent. The pod is left
/// behind when the run ends — it's a 5MB image listening on a socket,
/// and `wipe` clears it (it's in `rio-system` which `wipe` keeps, but
/// the next qa run's pre-delete handles it).
async fn spawn_socat_relay(host: &str, port: u16) -> Result<(u16, ProcessGuard)> {
    const POD: &str = "rio-qa-pg-relay";
    let s = sh::shell()?;
    let _ = sh::try_read(cmd!(
        s,
        "kubectl -n {NS} delete pod {POD} --ignore-not-found --wait=true"
    ));
    let target = format!("TCP:{host}:{port}");
    sh::try_read(cmd!(
        s,
        "kubectl -n {NS} run {POD} --image=alpine/socat --restart=Never -- TCP-LISTEN:5432,fork,reuseaddr {target}"
    ))?;
    // Wait Running (alpine/socat is small; usually <10s).
    sh::try_read(cmd!(
        s,
        "kubectl -n {NS} wait --for=condition=Ready pod/{POD} --timeout=60s"
    ))
    .context("socat relay pod never became Ready")?;
    // Port-forward to the socat pod's 5432.
    shared::port_forward(NS, &format!("pod/{POD}"), 0, 5432).await
}

/// Parse `host` and `port` from a `postgres://user:pass@host:port/db`
/// URL. Hand-rolled (no `url` crate dep) — the format is fixed
/// (`ensure_pg_secrets` / RDS).
fn pg_host_port(url: &str) -> Result<(String, u16)> {
    let after_at = url.split_once('@').map(|(_, r)| r).unwrap_or(url);
    let host_port = after_at.split_once('/').map(|(l, _)| l).unwrap_or(after_at);
    let (host, port) = host_port
        .rsplit_once(':')
        .context("PG URL missing host:port")?;
    Ok((host.to_owned(), port.parse().context("PG URL bad port")?))
}

fn rewrite_pg_host(url: &str, new_host_port: &str) -> String {
    let (pre, post) = url.split_once('@').expect("validated by pg_host_port");
    let (_, db) = post.split_once('/').unwrap_or((post, ""));
    format!("{pre}@{new_host_port}/{db}")
}

// ─── ephemeral tenants ─────────────────────────────────────────────────

/// One ephemeral tenant: name + path to its private key. The key's
/// `authorized_keys` line has comment = `name`, so the gateway maps
/// builds with this key to this tenant.
#[derive(Clone, Debug)]
pub struct Tenant {
    pub name: String,
    pub key: PathBuf,
}

/// Ephemeral tenant pool. `new()` creates `size` fresh tenants
/// (`qa-{nonce}-{i}`) each with its own ed25519 keypair, batch-installs
/// the keys into the gateway's `authorized_keys`, restarts the gateway
/// once. `acquire(n)` hands out `n` slots; `cleanup()` removes the keys
/// and tenants.
pub struct TenantPool {
    sem: Arc<Semaphore>,
    slots: Arc<Mutex<Vec<Tenant>>>,
    nonce: u64,
    /// Privkey tempdir. Held until cleanup; drop deletes the keys.
    _key_dir: TempDir,
}

impl TenantPool {
    pub async fn new(kube_client: &kube::Client, cli: &CliCtx, size: usize) -> Result<Self> {
        let (nonce, key_dir, tenants, pubkeys) = alloc_tenants(cli, size).await?;
        // Batch-install: one Secret read + one write for all N keys, then
        // ONE gateway rollout-restart so they take effect immediately
        // (vs ~70s hot-reload × 1).
        shared::merge_authorized_keys_batch(
            kube_client,
            &pubkeys.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .await?;
        step_restart_gateway(kube_client).await?;
        info!(
            "provisioned {} ephemeral tenants (qa-{nonce}-*) with per-tenant keys",
            tenants.len()
        );
        Ok(Self {
            sem: Arc::new(Semaphore::new(tenants.len())),
            slots: Arc::new(Mutex::new(tenants)),
            nonce,
            _key_dir: key_dir,
        })
    }

    /// Delete every tenant the pool created, strip their keys from the
    /// gateway Secret, drop the privkey tempdir. Called once after both
    /// scheduler phases drain. NotFound is swallowed (scenario may have
    /// deleted its own tenant).
    pub async fn cleanup(self, kube_client: &kube::Client, cli: &CliCtx) -> Result<()> {
        let tenants = Arc::into_inner(self.slots)
            .expect("all leases released before cleanup")
            .into_inner();
        for t in &tenants {
            match cli.run(&["delete-tenant", &t.name]) {
                Ok(_) => {}
                Err(e) if format!("{e:#}").to_lowercase().contains("not found") => {}
                Err(e) => return Err(e.context(format!("delete-tenant {}", t.name))),
            }
        }
        shared::remove_authorized_keys_by_comment_prefix(
            kube_client,
            &format!("qa-{}-", self.nonce),
        )
        .await?;
        info!("cleaned up {} ephemeral tenants + keys", tenants.len());
        Ok(())
    }

    pub async fn acquire(&self, n: usize) -> TenantLease {
        let permit = self
            .sem
            .clone()
            .acquire_many_owned(n as u32)
            .await
            .expect("never closed");
        let mut slots = self.slots.lock().await;
        let at = slots.len() - n;
        let tenants = slots.split_off(at);
        TenantLease {
            slots: self.slots.clone(),
            tenants,
            _permit: permit,
        }
    }
}

pub struct TenantLease {
    slots: Arc<Mutex<Vec<Tenant>>>,
    tenants: Vec<Tenant>,
    _permit: OwnedSemaphorePermit,
}

impl TenantLease {
    pub fn tenants(&self) -> &[Tenant] {
        &self.tenants
    }

    pub async fn release(mut self) {
        self.slots.lock().await.append(&mut self.tenants);
    }
}

/// Create `size` fresh tenants, each with an ed25519 keypair written to
/// a tempdir. Returns (nonce, tempdir, tenants, pubkey-lines).
async fn alloc_tenants(
    cli: &CliCtx,
    size: usize,
) -> Result<(u64, TempDir, Vec<Tenant>, Vec<String>)> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("post-1970")
        .as_secs();

    let key_dir = tempfile::Builder::new().prefix("rio-qa-keys-").tempdir()?;
    let mut tenants = Vec::with_capacity(size);
    let mut pubkeys = Vec::with_capacity(size);

    for i in 0..size {
        let name = format!("qa-{nonce}-{i}");
        let (priv_pem, pub_line) = ssh::generate(&name)?;
        let key = key_dir.path().join(&name);
        std::fs::write(&key, priv_pem)?;
        // ssh refuses keys with group/other-readable perms.
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
        tenants.push(Tenant {
            name: name.clone(),
            key,
        });
        pubkeys.push(pub_line);
    }

    // CreateTenant + upstream concurrently (independent of key install).
    try_join_all(tenants.iter().map(|t| async {
        step_tenant(cli, &t.name).await?;
        step_upstream(cli, &t.name).await
    }))
    .await?;

    Ok((nonce, key_dir, tenants, pubkeys))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pg_url_roundtrip() {
        let u = "postgres://rio:secret@rio-postgresql.rio-system:5432/rio";
        let (h, p) = pg_host_port(u).unwrap();
        assert_eq!(h, "rio-postgresql.rio-system");
        assert_eq!(p, 5432);
        assert_eq!(
            rewrite_pg_host(u, "localhost:54321"),
            "postgres://rio:secret@localhost:54321/rio"
        );
    }

    #[test]
    fn pg_url_rds() {
        let u = "postgres://rio:x@rio-abc.cluster-xyz.us-west-2.rds.amazonaws.com:5432/rio";
        let (h, _) = pg_host_port(u).unwrap();
        assert!(h.contains("rds.amazonaws.com"));
        assert!(!h.ends_with(".rio-system"));
    }

    #[test]
    fn pg_url_bad() {
        assert!(pg_host_port("postgres://rio@hostonly/db").is_err());
    }
}
