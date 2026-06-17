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

use crate::k8s::eks::smoke::{self, CliCtx, step_tenant, step_upstream};
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

    /// Tunnel gateway:22 and return the ssh-ng store URL for
    /// `tenants[tenant_idx]`, plus the guard. For scenarios that need
    /// to run arbitrary nix commands (`copy --from`, `path-info`,
    /// `store ping`) under a specific tenant's identity rather than
    /// the busybox-build helpers.
    pub async fn gateway_tunnel(&self, tenant_idx: usize) -> Result<(String, ProcessGuard)> {
        let key = self.tenant(tenant_idx).key.clone();
        let (port, guard) = smoke::gateway_tunnel(0).await?;
        Ok((
            format!(
                "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
                key.display()
            ),
            guard,
        ))
    }

    /// Resolve `tenants[tenant_idx]`'s name → UUID via `rio-cli
    /// list-tenants --json`. Cross-tenant scenarios need the UUID to
    /// match against `BuildInfo.tenant_id`.
    pub fn tenant_uuid(&self, tenant_idx: usize) -> Result<String> {
        let name = &self.tenant(tenant_idx).name;
        let ts: Vec<serde_json::Value> = self.cli_json(&["list-tenants"])?;
        ts.iter()
            .find(|t| t.get("tenant_name").and_then(|v| v.as_str()) == Some(name))
            .and_then(|t| t.get("tenant_id").and_then(|v| v.as_str()))
            .map(str::to_owned)
            .with_context(|| format!("tenant '{name}' not in list-tenants"))
    }

    /// Scheduler-leader pod name. Several scenarios need this for log
    /// inspection independent of metric scraping.
    pub async fn scheduler_leader(&self) -> Result<String> {
        kube::scheduler_leader(&self.kube, NS).await
    }

    pub const BUILDER_LABEL: &str = "app.kubernetes.io/name=rio-builder";
    pub const NS_BUILDERS: &str = NS_BUILDERS;
}

/// Tunnel gateway:22, wait for SSH banner, run `build_expr`. Shared
/// body of all four `nix_build[_expr]_via_gateway[_bg]`.
async fn gateway_build(key: PathBuf, expr: String) -> Result<()> {
    // Mechanism steps (`step_debug`) — repeated ~50× per QA run, only
    // useful under `-v`. Failures still propagate via `?`; the
    // *scenario* verdict (PASS/FAIL) surfaces them at default
    // verbosity. The `_bg` variants `tokio::spawn` this fn and the
    // caller may never await the JoinHandle (i048c only `bg.abort()`s
    // on its own timeout) — those callers must check `bg.is_finished()`
    // / `bg.await` if a silent bg failure would matter, since a
    // step_debug error is not loud enough to be the only signal.
    let (port, _guard) =
        crate::ui::step_debug("tunnel gateway:22", || smoke::gateway_tunnel(0)).await?;
    let store = format!(
        "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
        key.display()
    );
    // Retry on transient gateway→scheduler reconnect: phase-2 scenarios
    // kill the scheduler-leader; the gateway's BalancedChannel needs a
    // few probe ticks to find the new leader, during which ResolveTenant
    // fails → no JWT minted → SubmitBuild rejected with
    // "requires x-rio-tenant-token in JWT mode". The component-disjoint
    // scheduler serializes scheduler-mutators with each other but a
    // *finished* scenario's after-effects (gateway reconnect lag) can
    // bleed into the next one's first build.
    let mut attempts = 0;
    loop {
        match smoke::build_expr(&expr, &store).await {
            Ok(()) => return Ok(()),
            Err(e) if attempts < 5 && is_transient_gateway_err(&e) => {
                attempts += 1;
                tracing::info!(
                    "gateway_build: transient ({}); retry {}/5 in 5s",
                    one_line(&e),
                    attempts
                );
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
            Err(e) => return Err(e),
        }
    }
}

fn is_transient_gateway_err(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("requires x-rio-tenant-token")
        || s.contains("Connection refused")
        || s.contains("transport error")
}

fn one_line(e: &anyhow::Error) -> String {
    format!("{e:#}").lines().next().unwrap_or("").to_owned()
}

// ─── PG handle ─────────────────────────────────────────────────────────

/// Process-lifetime PG connection. Built once in `scheduler::run()`;
/// scenarios get `Arc<PgPool>` via `QaCtx`. Field drop order: pool
/// closes → tunnel killed.
pub struct PgHandle {
    pub pool: PgPool,
    _guard: ProcessGuard,
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

    /// Connect to the cluster's PG. Neither RDS (private subnets, node
    /// SG only) nor k3s bitnami PG (ClusterIP) is reachable from the
    /// operator host — SSM-tunnel to the RDS endpoint (eks) or
    /// port-forward `svc/rio-postgresql` (k3s), then sqlx connects to
    /// `localhost:{bound}`.
    ///
    /// `up --wipe` passes a URL captured BEFORE `helm uninstall`
    /// (which removes the ExternalSecret-backed Secret).
    pub async fn open_with_url(url: &str) -> Result<Self> {
        let (host, port) = pg_host_port(url)?;
        let in_cluster = host.ends_with(&format!(".{NS}")) || host == "rio-postgresql";
        let (bound, guard) = if in_cluster {
            shared::port_forward(NS, "svc/rio-postgresql", 0, 5432).await?
        } else {
            crate::k8s::ssm::tunnel_host(&host, port, 0).await?
        };
        let url = rewrite_pg_host(url, &format!("localhost:{bound}"));
        let pool = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .connect(&url)
            .await
            .context("connect to cluster PG via tunnel")?;
        info!(
            "qa pg handle open ({}, {host})",
            if in_cluster {
                "port-forward svc"
            } else {
                "ssm"
            }
        );
        Ok(Self {
            pool,
            _guard: guard,
        })
    }
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

/// Rewrite the URL's host to the local tunnel endpoint AND downgrade
/// its TLS posture to `sslmode=require`, dropping `sslrootcert`.
/// verify-full through a localhost tunnel is structurally impossible
/// (the server cert's SAN names the RDS endpoint, not `localhost`)
/// and the bundle path in the URL is a pod mount path that does not
/// exist on the operator machine. `require` (encrypt, don't verify)
/// is the correct tunnel posture: the threat model for an
/// operator-laptop port-forward differs from in-VPC pods, and the
/// credential is the master password either way.
fn rewrite_pg_host(url: &str, new_host_port: &str) -> String {
    let (pre, post) = url.split_once('@').expect("validated by pg_host_port");
    let (_, db) = post.split_once('/').unwrap_or((post, ""));
    let (path, query) = db.split_once('?').unwrap_or((db, ""));
    let kept: Vec<&str> = query
        .split('&')
        .filter(|p| !p.is_empty() && !p.starts_with("sslmode=") && !p.starts_with("sslrootcert="))
        .collect();
    let mut q = kept.join("&");
    if query.contains("sslmode=") {
        if !q.is_empty() {
            q.push('&');
        }
        q.push_str("sslmode=require");
    }
    if q.is_empty() {
        format!("{pre}@{new_host_port}/{path}")
    } else {
        format!("{pre}@{new_host_port}/{path}?{q}")
    }
}

#[cfg(test)]
mod rewrite_tests {
    use super::rewrite_pg_host;

    /// The ESO-templated Aurora URL carries verify-full + a pod-mount
    /// sslrootcert path; both must be replaced for the tunnel or the
    /// connect fails twice over (missing bundle file, SAN mismatch
    /// against localhost).
    #[test]
    fn verify_full_url_downgrades_to_require_for_tunnel() {
        let url = "postgres://rio:p%40ss@rio-pg.cluster-x.rds.amazonaws.com:5432/rio\
                   ?sslmode=verify-full&sslrootcert=/etc/rio/rds-ca/global-bundle.pem";
        let got = rewrite_pg_host(url, "localhost:15432");
        assert_eq!(
            got,
            "postgres://rio:p%40ss@localhost:15432/rio?sslmode=require"
        );
    }

    /// k3s bitnami URLs have no ssl params — pass through untouched.
    #[test]
    fn plain_url_keeps_no_query() {
        let url = "postgres://rio:secret@rio-postgresql:5432/rio";
        assert_eq!(
            rewrite_pg_host(url, "localhost:6000"),
            "postgres://rio:secret@localhost:6000/rio"
        );
    }

    /// Unrelated query params survive the rewrite.
    #[test]
    fn unrelated_params_survive() {
        let url = "postgres://u:p@h:5432/db?application_name=qa&sslmode=verify-full";
        assert_eq!(
            rewrite_pg_host(url, "localhost:7000"),
            "postgres://u:p@localhost:7000/db?application_name=qa&sslmode=require"
        );
    }
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
        Self::sweep_stale_keys(kube_client, nonce).await?;
        // Batch-install: one Secret read + one write for all N keys.
        shared::merge_authorized_keys_batch(
            kube_client,
            &pubkeys.iter().map(String::as_str).collect::<Vec<_>>(),
        )
        .await?;
        // Wait for the gateway's `r[gw.keys.hot-reload]` (~70s ceiling:
        // kubelet ≤60s Secret projection + gateway 10s poll) instead of
        // `step_restart_gateway`. The health stage's smoke test restarts
        // the gateway ~3 min before this point; a second back-to-back
        // rollout leaves the gateway churning while phase-1 scenarios
        // start submitting builds. The functional probe (mirroring i109
        // `r[verify gw.keys.hot-reload]`): poll until tenant 0's key
        // authenticates via `nix store ping`. "Key accepted" is the
        // user-observable behavior the QA scenarios depend on; a fixed
        // sleep would race kubelet projection lag, and a Secret-vs-log
        // count comparison races with prior changes.
        Self::wait_keys_hot_reload(&tenants[0]).await?;
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

    /// Poll until the gateway accepts an SSH connection authenticated
    /// with `tenant`'s key — the user-observable signal that
    /// `r[gw.keys.hot-reload]` has propagated the
    /// `merge_authorized_keys_batch` write. Same probe shape as i109
    /// (`r[verify gw.keys.hot-reload]`): `nix store ping` does the SSH
    /// auth + ssh-ng handshake but no build, and `BatchMode` /
    /// `IdentitiesOnly` make a key-reject fail fast instead of falling
    /// through to other auth methods.
    ///
    /// Replaces a `step_restart_gateway` here: the health stage's smoke
    /// test already restarts the gateway ~3 min before this point, and
    /// a second back-to-back rollout — with `podAntiAffinity` between
    /// gateway replicas and `nodeAffinity` to system nodes — leaves the
    /// Deployment churning while phase-1 scenarios start submitting.
    /// 2026-05-14 round 4: 13 phase-1 timeouts with the gateway
    /// `FailedScheduling` (`Insufficient cpu`) on system nodes for the
    /// whole window. The hot-reload window costs ~70s of wall-clock vs
    /// ~80s for a rollout, so the swap is roughly net-zero on time and
    /// strictly less cluster disruption.
    async fn wait_keys_hot_reload(probe_tenant: &Tenant) -> Result<()> {
        let (port, _guard) = smoke::gateway_tunnel(0).await?;
        let key = probe_tenant.key.display().to_string();
        let store = format!("ssh-ng://rio@localhost:{port}?ssh-key={key}");
        let sshopts = format!("{} -o ConnectTimeout=5", shared::NIX_SSHOPTS_BASE);
        // r[gw.keys.hot-reload]: kubelet ≤60s + gateway 10s poll = ~70s
        // ceiling. 120s gives 50s slack for builder-disk I/O variance.
        crate::ui::poll(
            "QA tenant keys hot-reload",
            Duration::from_secs(5),
            24,
            || {
                let store = store.clone();
                let sshopts = sshopts.clone();
                async move {
                    let s = crate::sh::shell()?;
                    let ok = crate::sh::try_read(
                        crate::sh::cmd!(s, "timeout 10 nix store ping --store {store}")
                            .env("NIX_SSHOPTS", &sshopts),
                    )
                    .is_ok();
                    Ok(ok.then_some(()))
                }
            },
        )
        .await
        .context(
            "gateway did not accept the QA tenant pool's keys within 120s — \
             authorized_keys hot-reload not firing (see i109 / \
             r[gw.keys.hot-reload]); verify the gateway is watching the \
             rio-gateway-ssh Secret and the kubelet sync period is ≤60s",
        )
    }

    /// Delete every tenant the pool created, strip their keys from the
    /// gateway Secret, drop the privkey tempdir. Called once after both
    /// scheduler phases drain. NotFound is swallowed (scenario may have
    /// deleted its own tenant).
    pub async fn cleanup(self, kube_client: &kube::Client, cli: &CliCtx) -> Result<()> {
        let tenants = Arc::into_inner(self.slots)
            .expect("all leases released before cleanup")
            .into_inner();
        // Best-effort per-tenant: a delete-tenant failure (cli-tunnel
        // race, scheduler still recovering from phase-2) MUST NOT skip
        // key removal — bailing here is what let stale keys accumulate
        // across runs and broke i109's count expectations.
        let mut deleted = 0usize;
        for t in &tenants {
            match cli.run(&["delete-tenant", &t.name]) {
                Ok(_) => deleted += 1,
                Err(e) if format!("{e:#}").to_lowercase().contains("not found") => {}
                Err(e) => tracing::warn!("delete-tenant {}: {e:#} (continuing)", t.name),
            }
        }
        shared::remove_authorized_keys_by_comment_prefix(
            kube_client,
            &format!("qa-{}-", self.nonce),
        )
        .await?;
        info!(
            "cleaned up {}/{} ephemeral tenants + keys (nonce={})",
            deleted,
            tenants.len(),
            self.nonce
        );
        Ok(())
    }

    /// Sweep stale `qa-{ts}-*` keys from PRIOR crashed runs. Called at
    /// the start of `new()` so accumulation self-heals. A key is stale
    /// if its `qa-{ts}-` prefix has `ts < this run's nonce` (nonce is
    /// unix-secs, monotone). Concurrent qa runs would have ts ≥ ours;
    /// those are left alone.
    async fn sweep_stale_keys(kube_client: &kube::Client, nonce: u64) -> Result<()> {
        let existing = kube::get_secret_key(kube_client, NS, "rio-gateway-ssh", "authorized_keys")
            .await?
            .unwrap_or_default();
        let stale: Vec<_> = existing
            .lines()
            .filter_map(|l| l.split_whitespace().nth(2))
            .filter_map(|c| {
                c.strip_prefix("qa-")
                    .and_then(|s| s.split('-').next())
                    .and_then(|ts| ts.parse::<u64>().ok())
                    .filter(|&ts| ts < nonce)
                    .map(|ts| format!("qa-{ts}-"))
            })
            .collect::<std::collections::HashSet<_>>()
            .into_iter()
            .collect();
        for prefix in &stale {
            shared::remove_authorized_keys_by_comment_prefix(kube_client, prefix).await?;
        }
        if !stale.is_empty() {
            info!("swept {} stale qa key prefix(es): {:?}", stale.len(), stale);
        }
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
