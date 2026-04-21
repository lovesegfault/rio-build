//! Per-scenario execution context + ephemeral tenant pool.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use futures_util::future::try_join_all;
use serde::de::DeserializeOwned;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tracing::info;

use crate::config::XtaskConfig;
use crate::k8s::eks::smoke::{self, CliCtx, step_tenant, step_upstream};
use crate::k8s::status::{SCHED_METRICS_PORT, Scrape, scrape_pod};
use crate::k8s::{NS, NS_BUILDERS, client as kube, shared};
use crate::sh::{self, cmd};

/// What every `Scenario::run()` receives. `cli`/`kube`/`cfg` are shared
/// across the whole run (cheap clones); `tenants` is per-scenario for
/// `Isolation::Tenant { count }` — `count` names allocated from the pool.
pub struct QaCtx {
    pub kube: kube::Client,
    pub cli: Arc<CliCtx>,
    pub cfg: Arc<XtaskConfig>,
    /// `count` tenant names for `Isolation::Tenant`; empty otherwise.
    /// Index 0 is the "primary" — single-tenant scenarios just use
    /// `ctx.tenant()`. Seed scenarios don't yet read this — see the
    /// attribution caveat on [`QaCtx::nix_build_via_gateway`].
    #[allow(dead_code)]
    pub tenants: Vec<String>,
}

impl QaCtx {
    /// Primary tenant. Panics if isolation isn't `Tenant` — that's a
    /// scenario-author bug, not a runtime condition.
    #[allow(dead_code)] // see field doc above
    pub fn tenant(&self) -> &str {
        self.tenants
            .first()
            .map(String::as_str)
            .expect("tenant() called on non-Tenant scenario")
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

    /// `rio-cli --json <args>` parsed into `T`. The CLI's `--json` flag
    /// is the global flag, so it's prepended here.
    pub fn cli_json<T: DeserializeOwned>(&self, args: &[&str]) -> Result<T> {
        let mut full = vec!["--json"];
        full.extend_from_slice(args);
        let out = self.cli.run(&full)?;
        serde_json::from_str(&out).with_context(|| format!("rio-cli {args:?}: {out}"))
    }

    /// Shell out to `kubectl`. Thin escape hatch for asserts that the
    /// kube crate doesn't cover (pod exec, logs --since, raw resource
    /// inspection). Stdout returned; non-zero exit propagates as Err.
    pub fn kubectl(&self, args: &[&str]) -> Result<String> {
        let s = sh::shell()?;
        sh::try_read(cmd!(s, "kubectl {args...}"))
    }

    /// List running pods in `ns` matching `label_selector`. Convenience
    /// for builder/fetcher fan-out asserts (i165 D-state probe, i114
    /// log grep).
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

    /// Submit a trivial busybox build via the gateway (port-forward +
    /// ssh-ng), block until it completes. Reuses [`smoke::smoke_build`]
    /// — `secs` of `read /dev/zero` plus `out_kb` KiB of output.
    ///
    /// **Tenant attribution caveat:** the SSH key is the operator's
    /// `RIO_SSH_PUBKEY`, NOT the ephemeral `qa-{nonce}-{i}` tenant.
    /// Builds attribute to whichever tenant that key maps to. Scenarios
    /// asserting per-tenant behavior must account for this; scenarios
    /// asserting cluster-level behavior (mailbox depth, log relay,
    /// liveness-kill) don't care.
    pub async fn nix_build_via_gateway(&self, tag: &str, secs: u32, out_kb: u32) -> Result<()> {
        let key = crate::ssh::privkey_path(&self.cfg)?;
        let (port, _guard) = shared::port_forward(NS, "svc/rio-gateway", 0, 22).await?;
        // ssh_banner readiness — same poll as gateway_port_forward.
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
        smoke::smoke_build(tag, secs, out_kb, &store).await
    }

    /// Spawn `nix_build_via_gateway` in the background; returns a handle
    /// the scenario can `.await` later (or drop to abort). For asserts
    /// that observe scheduler state WHILE a build runs (i095, i163).
    pub fn nix_build_via_gateway_bg(
        &self,
        tag: &str,
        secs: u32,
        out_kb: u32,
    ) -> tokio::task::JoinHandle<Result<()>> {
        let cfg = self.cfg.clone();
        let tag = tag.to_string();
        tokio::spawn(async move {
            let key = crate::ssh::privkey_path(&cfg)?;
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
            smoke::smoke_build(&tag, secs, out_kb, &store).await
        })
    }

    /// Convenience: scheduler-leader pod name. Several scenarios need
    /// this for log inspection independent of metric scraping.
    pub async fn scheduler_leader(&self) -> Result<String> {
        kube::scheduler_leader(&self.kube, NS).await
    }

    /// Label selector for builder pods. Centralized so a future
    /// chart-side label rename touches one place.
    pub const BUILDER_LABEL: &str = "app.kubernetes.io/name=rio-builder";

    /// Namespace builders run in.
    pub const NS_BUILDERS: &str = NS_BUILDERS;
}

/// Ephemeral tenant pool. `new()` creates `size` fresh tenants
/// (`qa-{nonce}-{i}`) via rio-cli; `acquire(n)` hands out `n` names
/// under one semaphore reservation; `release()` returns them.
pub struct TenantPool {
    sem: Arc<Semaphore>,
    slots: Arc<Mutex<Vec<String>>>,
}

impl TenantPool {
    pub async fn new(cli: &CliCtx, size: usize) -> Result<Self> {
        let names = alloc_tenants(cli, size).await?;
        Ok(Self {
            sem: Arc::new(Semaphore::new(names.len())),
            slots: Arc::new(Mutex::new(names)),
        })
    }

    /// Delete every tenant the pool created. Called once after both
    /// scheduler phases drain. NotFound is swallowed (a scenario may
    /// have deleted its own tenant); any other error is propagated so a
    /// half-cleaned pool is loud.
    pub async fn cleanup(self, cli: &CliCtx) -> Result<()> {
        let names = Arc::into_inner(self.slots)
            .expect("all leases released before cleanup")
            .into_inner();
        for n in &names {
            match cli.run(&["delete-tenant", n]) {
                Ok(_) => {}
                Err(e) if format!("{e:#}").to_lowercase().contains("not found") => {}
                Err(e) => return Err(e.context(format!("delete-tenant {n}"))),
            }
        }
        info!("cleaned up {} ephemeral tenants", names.len());
        Ok(())
    }

    /// Borrow `n` tenants. Awaits if fewer than `n` are free.
    pub async fn acquire(&self, n: usize) -> TenantLease {
        let permit = self
            .sem
            .clone()
            .acquire_many_owned(n as u32)
            .await
            .expect("never closed");
        let mut slots = self.slots.lock().await;
        let at = slots.len() - n;
        let names = slots.split_off(at);
        TenantLease {
            slots: self.slots.clone(),
            names,
            _permit: permit,
        }
    }
}

pub struct TenantLease {
    slots: Arc<Mutex<Vec<String>>>,
    names: Vec<String>,
    _permit: OwnedSemaphorePermit,
}

impl TenantLease {
    pub fn names(&self) -> &[String] {
        &self.names
    }

    pub async fn release(mut self) {
        self.slots.lock().await.append(&mut self.names);
    }
}

/// Create `size` fresh tenants and configure their upstream cache.
/// Names are `qa-{unix-secs}-{i}` so concurrent/repeated runs don't
/// collide. CreateTenant + upstream are issued concurrently — neither
/// touches the gateway, so no 70s hot-reload wait (scenarios submit via
/// the cli-tunnel SubmitBuild path, not ssh-ng).
async fn alloc_tenants(cli: &CliCtx, size: usize) -> Result<Vec<String>> {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("post-1970")
        .as_secs();
    let names: Vec<String> = (0..size).map(|i| format!("qa-{nonce}-{i}")).collect();

    try_join_all(names.iter().map(|n| async move {
        step_tenant(cli, n).await?;
        step_upstream(cli, n).await
    }))
    .await?;

    info!(
        "provisioned {} ephemeral tenants (qa-{nonce}-*)",
        names.len()
    );
    Ok(names)
}
