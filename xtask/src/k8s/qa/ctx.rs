//! Per-scenario execution context + ephemeral tenant pool.

use std::sync::Arc;

use anyhow::Result;
use futures_util::future::try_join_all;
use tokio::sync::{Mutex, OwnedSemaphorePermit, Semaphore};
use tracing::info;

use crate::k8s::client as kube;
use crate::k8s::eks::smoke::{CliCtx, step_tenant, step_upstream};

/// What every `Scenario::run()` receives. `cli`/`kube` are shared across
/// the whole run (cheap clones); `tenants` is per-scenario for
/// `Isolation::Tenant { count }` — `count` names allocated from the pool.
#[allow(dead_code)] // fields read by scenario impls
pub struct QaCtx {
    pub kube: kube::Client,
    pub cli: Arc<CliCtx>,
    /// `count` tenant names for `Isolation::Tenant`; empty otherwise.
    /// Index 0 is the "primary" — single-tenant scenarios just use
    /// `ctx.tenant()`.
    pub tenants: Vec<String>,
}

#[allow(dead_code)] // called by scenario impls
impl QaCtx {
    /// Primary tenant. Panics if isolation isn't `Tenant` — that's a
    /// scenario-author bug, not a runtime condition.
    pub fn tenant(&self) -> &str {
        self.tenants
            .first()
            .map(String::as_str)
            .expect("tenant() called on non-Tenant scenario")
    }
}

/// Ephemeral tenant pool. `new()` creates `size` fresh tenants
/// (`qa-{nonce}-{i}`) via rio-cli; `acquire(n)` hands out `n` names
/// under one semaphore reservation; `release()` returns them.
///
/// Cleanup: there is no `DeleteTenant` RPC today, so ephemeral tenants
/// accumulate (8 per run, bounded by manual invocation). Tracked as a
/// follow-up — either add `DeleteTenant` to AdminService, or have
/// `qa --lint` flag tenant-table bloat.
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
