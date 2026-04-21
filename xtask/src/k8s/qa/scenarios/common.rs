//! Helpers shared across scenarios. Kept here (not on `QaCtx`) so the
//! sibling agent owning `ctx.rs` doesn't conflict.

use anyhow::Result;

use crate::k8s::qa::QaCtx;
use crate::k8s::status::{STORE_METRICS_PORT, Scrape, scrape_pod};
use crate::k8s::{NS_BUILDERS, NS_STORE};

/// Scrape one rio-store replica's `/metrics`. Multi-replica stores
/// don't aggregate — caller picks the pod (or sums across all via
/// [`scrape_all_stores`]).
pub async fn scrape_store(ctx: &QaCtx, pod: &str) -> Result<Scrape> {
    let body = scrape_pod(&ctx.kube, NS_STORE, pod, STORE_METRICS_PORT).await?;
    Ok(Scrape::parse(&body))
}

/// Scrape every store replica and return the per-pod scrapes.
pub async fn scrape_all_stores(ctx: &QaCtx) -> Result<Vec<(String, Scrape)>> {
    let pods = ctx.running_pods(NS_STORE, "app.kubernetes.io/name=rio-store")?;
    let mut out = Vec::with_capacity(pods.len());
    for p in pods {
        let s = scrape_store(ctx, &p).await?;
        out.push((p, s));
    }
    Ok(out)
}

/// First running builder pod's name, or None.
pub fn any_builder(ctx: &QaCtx) -> Result<Option<String>> {
    Ok(ctx
        .running_pods(NS_BUILDERS, QaCtx::BUILDER_LABEL)?
        .into_iter()
        .next())
}
