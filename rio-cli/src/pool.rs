//! `rio-cli pool get|describe` — Pool CR inspection.
//!
//! Unlike every other subcommand in rio-cli, this talks to the
//! Kubernetes apiserver directly (kube-rs `Api<T>`), not to the
//! scheduler's gRPC AdminService. `kubectl get pl` already works
//! (the CRD has printcolumns); this adds `--json` with a stable
//! shape and a `describe` that pretty-prints `.status.conditions`.

use clap::Subcommand;
use kube::{Api, Client, ResourceExt};
use serde::Serialize;

use rio_crds::pool::{Pool, PoolStatus};

#[derive(Subcommand, Clone)]
pub enum PoolCmd {
    /// List Pools in a namespace.
    Get {
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
    /// Describe one Pool: spec kind/systems + live status (replicas,
    /// conditions).
    Describe {
        name: String,
        #[arg(short, long, default_value = "default")]
        namespace: String,
    },
}

/// Run the `pool` subcommand. Called from `main.rs` BEFORE the gRPC
/// admin client connect — this subcommand doesn't touch the
/// scheduler, so it must work even when the scheduler is down.
pub(crate) async fn run(as_json: bool, cmd: PoolCmd) -> anyhow::Result<()> {
    let client = Client::try_default()
        .await
        .map_err(|e| anyhow::anyhow!("kube client: {e}"))?;

    match cmd {
        PoolCmd::Get { namespace } => get(as_json, client, &namespace).await,
        PoolCmd::Describe { name, namespace } => describe(as_json, client, &namespace, &name).await,
    }
}

async fn get(as_json: bool, client: Client, ns: &str) -> anyhow::Result<()> {
    let api: Api<Pool> = Api::namespaced(client, ns);
    let list = api
        .list(&Default::default())
        .await
        .map_err(|e| anyhow::anyhow!("list Pools in {ns}: {e}"))?;

    if as_json {
        return crate::json(&GetJson {
            items: list.items.iter().map(RowJson::from).collect(),
        });
    }

    println!(
        "{:<24} {:<8} {:<12} READY/DESIRED",
        "NAME", "KIND", "SYSTEMS"
    );
    for p in &list.items {
        let st = p.status.clone().unwrap_or_default();
        println!(
            "{:<24} {:<8} {:<12} {}/{}",
            p.name_any(),
            format!("{:?}", p.spec.kind),
            p.spec.systems.join(","),
            st.ready_replicas,
            st.desired_replicas,
        );
    }
    Ok(())
}

async fn describe(as_json: bool, client: Client, ns: &str, name: &str) -> anyhow::Result<()> {
    let api: Api<Pool> = Api::namespaced(client, ns);
    let p = api
        .get(name)
        .await
        .map_err(|e| anyhow::anyhow!("get Pool {ns}/{name}: {e}"))?;

    if as_json {
        return crate::json(&RowJson::from(&p));
    }

    println!("Name:      {}", p.name_any());
    println!("Namespace: {ns}");
    println!("Kind:      {:?}", p.spec.kind);
    println!("Systems:   {}", p.spec.systems.join(", "));
    println!("Features:  {}", p.spec.features.join(", "));
    if let Some(st) = &p.status {
        println!(
            "Replicas:  {}/{} (ready/desired)",
            st.ready_replicas, st.desired_replicas
        );
        println!("Conditions:");
        for c in &st.conditions {
            println!("  {:<24} {}={}  {}", c.type_, c.status, c.reason, c.message);
        }
    } else {
        println!("Status: (not yet populated — reconciler pending)");
    }
    Ok(())
}

#[derive(Serialize)]
struct GetJson<'a> {
    items: Vec<RowJson<'a>>,
}

#[derive(Serialize)]
struct RowJson<'a> {
    name: String,
    kind: String,
    systems: &'a [String],
    features: &'a [String],
    status: Option<&'a PoolStatus>,
}

impl<'a> From<&'a Pool> for RowJson<'a> {
    fn from(p: &'a Pool) -> Self {
        Self {
            name: p.name_any(),
            kind: format!("{:?}", p.spec.kind),
            systems: &p.spec.systems,
            features: &p.spec.features,
            status: p.status.as_ref(),
        }
    }
}
