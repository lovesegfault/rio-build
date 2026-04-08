//! `rio-cli poison-clear|poison-list` — poisoned-derivation inspection.
//!
//! Separate module (not inline in `main.rs`) — keep `main.rs` deltas to
//! enum variant + match arm + mod decl only.

use rio_proto::AdminServiceClient;
use rio_proto::types::ClearPoisonRequest;
use serde::Serialize;
use tonic::transport::Channel;

use crate::{fmt_secs_ago, json, rpc};

pub(crate) async fn run_clear(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
    drv_path: String,
) -> anyhow::Result<()> {
    // Validate BEFORE the RPC. The scheduler's DAG is keyed on
    // the full store path, so a bare hash silently no-matches
    // and cleared=false looks like "not poisoned" when it's
    // actually "you gave me the wrong key".
    if !drv_path.starts_with("/nix/store/") || !drv_path.ends_with(".drv") {
        anyhow::bail!(
            "expected full .drv store path (e.g. /nix/store/abc...-foo.drv), got '{drv_path}'.\n\
             Run `rio-cli poison-list` to find the right path."
        );
    }
    let req = ClearPoisonRequest {
        derivation_hash: drv_path.clone(),
    };
    let resp = rpc("ClearPoison", async || {
        client.clear_poison(req.clone()).await
    })
    .await?;
    if as_json {
        #[derive(Serialize)]
        struct ClearedJson<'a> {
            drv_path: &'a str,
            cleared: bool,
        }
        json(&ClearedJson {
            drv_path: &drv_path,
            cleared: resp.cleared,
        })?;
    } else if resp.cleared {
        println!("cleared poison for {drv_path}");
    } else {
        println!("{drv_path}: not poisoned (nothing to clear)");
    }
    Ok(())
}

pub(crate) async fn run_list(
    as_json: bool,
    client: &mut AdminServiceClient<Channel>,
) -> anyhow::Result<()> {
    let resp = rpc("ListPoisoned", async || client.list_poisoned(()).await).await?;
    if as_json {
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
