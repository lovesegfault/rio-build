//! `rio-cli invalidate-path` — operator remediation for a path that was
//! uploaded, signed, and cached with wrong content (the "wrong-success"
//! incident class).
//!
//! Calls `StoreAdminService.InvalidatePath`: deletes the metadata that
//! makes the path a cache hit (narinfo + manifests via CASCADE,
//! path_tenants, and — unless `--keep-realisations` — the realisations
//! rows resolving to it) so the next submission misses the cache and
//! re-executes. Chunk refcounts are decremented alongside; chunks that
//! drop to zero are marked deleted and enqueued for backend deletion.
//! Idempotent: invalidating an absent path reports
//! `found = false` and exits 0.

use rio_proto::types::InvalidatePathRequest;

use crate::{StoreAdminClient, json, rpc};

#[derive(clap::Args, Clone)]
pub(crate) struct Args {
    /// Full store path (`/nix/store/{hash}-{name}`) to invalidate.
    path: String,
    /// Keep `realisations` rows resolving to this path (only delete the
    /// narinfo/manifest cache-hit metadata). Default deletes them — a
    /// CA realisation pointing at an invalidated path would otherwise
    /// keep answering QueryRealisation with a path that 404s on fetch.
    #[arg(long)]
    keep_realisations: bool,
}

pub(crate) async fn run(
    as_json: bool,
    client: &mut StoreAdminClient,
    a: Args,
) -> anyhow::Result<()> {
    let req = InvalidatePathRequest {
        store_path: a.path.clone(),
        keep_realisations: a.keep_realisations,
    };
    let resp = rpc("InvalidatePath", async || {
        client.invalidate_path(req.clone()).await
    })
    .await?;

    if as_json {
        json(&resp)?;
        return Ok(());
    }

    if !resp.found {
        println!("{}: not present (nothing to invalidate)", a.path);
        return Ok(());
    }
    println!("invalidated {}", a.path);
    println!("  narinfo deleted:          {}", resp.narinfo_deleted);
    println!("  manifest existed:         {}", resp.manifest_existed);
    println!("  realisations deleted:     {}", resp.realisations_deleted);
    println!(
        "  realisation deps deleted: {}",
        resp.realisation_deps_deleted
    );
    println!("  path_tenants deleted:     {}", resp.path_tenants_deleted);
    println!("  drv_modulo deleted:       {}", resp.drv_modulo_deleted);
    Ok(())
}
