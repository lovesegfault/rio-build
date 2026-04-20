//! `rio-cli poison-clear|poison-list` — poisoned-derivation inspection.

use crate::AdminClient;
use rio_proto::types::ClearPoisonRequest;
use serde::Serialize;

use crate::{fmt_secs_ago, json, rpc};

#[derive(clap::Args, Clone)]
pub(crate) struct ClearArgs {
    /// Full .drv store path (e.g. `/nix/store/abc...-foo.drv`).
    /// Bare hashes are rejected — a silent no-match would look
    /// like "not poisoned" when it's actually "wrong key format".
    drv_path: String,
}

pub(crate) async fn run_clear(
    as_json: bool,
    client: &mut AdminClient,
    a: ClearArgs,
) -> anyhow::Result<()> {
    // Validate BEFORE the RPC. The scheduler's DAG is keyed on
    // the full store path, so a bare hash silently no-matches
    // and cleared=false looks like "not poisoned" when it's
    // actually "you gave me the wrong key".
    if !a.drv_path.starts_with("/nix/store/") || !a.drv_path.ends_with(".drv") {
        anyhow::bail!(
            "expected full .drv store path (e.g. /nix/store/abc...-foo.drv), got '{}'.\n\
             Run `rio-cli poison-list` to find the right path.",
            a.drv_path
        );
    }
    let req = ClearPoisonRequest {
        derivation_hash: a.drv_path.clone(),
    };
    let resp = rpc("ClearPoison", async || {
        client.clear_poison(req.clone()).await
    })
    .await?;
    if as_json {
        return json(&serde_json::json!({ "drv_path": a.drv_path, "cleared": resp.cleared }));
    }
    if resp.cleared {
        println!("cleared poison for {}", a.drv_path);
    } else {
        println!("{}: not poisoned (nothing to clear)", a.drv_path);
    }
    Ok(())
}

pub(crate) async fn run_list(as_json: bool, client: &mut AdminClient) -> anyhow::Result<()> {
    let resp = rpc("ListPoisoned", async || client.list_poisoned(()).await).await?;
    if as_json {
        // `PoisonedDerivation` is NOT in rio-proto's Serialize allowlist
        // (build.rs `type_attribute` loop) — project the fields we need.
        #[derive(Serialize)]
        struct Row<'a> {
            drv_path: &'a str,
            failed_executors: &'a [String],
            poisoned_secs_ago: u64,
        }
        json(
            &resp
                .derivations
                .iter()
                .map(|d| Row {
                    drv_path: &d.drv_path,
                    failed_executors: &d.failed_executors,
                    poisoned_secs_ago: d.poisoned_secs_ago,
                })
                .collect::<Vec<_>>(),
        )?;
    } else if resp.derivations.is_empty() {
        println!("no poisoned derivations");
    } else {
        for d in &resp.derivations {
            println!(
                "{}\n  failed on: {}\n  poisoned:  {} (TTL 24h)",
                d.drv_path,
                d.failed_executors.join(", "),
                fmt_secs_ago(d.poisoned_secs_ago),
            );
        }
    }
    Ok(())
}
