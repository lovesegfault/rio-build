//! `xtask k8s status` — one-shot deployment health report.
//!
//! Every section is best-effort: a missing release, unreachable
//! scheduler, or absent CRD renders as a status line, not a hard
//! error. The command should be useful precisely when things are
//! broken.

use anyhow::{Result, bail};
use console::style;
use kube::api::{Api, ListParams};
use rio_crds::workerpool::WorkerPool;
use serde::Serialize;

use crate::k8s::NS;
use crate::k8s::provider::{self, Provider, ProviderKind};
use crate::{helm, kube as k, ui};

#[derive(Serialize)]
pub struct Report {
    context: String,
    release: Option<helm::ReleaseStatus>,
    deployments: Vec<k::DeployStatus>,
    daemonsets: Vec<k::DsStatus>,
    worker_pools: Vec<WpStatus>,
    scheduler_leader: Option<String>,
    problem_pods: Vec<k::PodProblem>,
    rio_cli: RioCli,
}

#[derive(Serialize)]
pub struct WpStatus {
    name: String,
    ready: i32,
    replicas: i32,
    desired: i32,
    /// Last `Scaling` condition reason (ScaledUp/ScaledDown/UnknownMetric).
    reason: Option<String>,
    /// `SchedulerUnreachable` condition is True.
    scheduler_unreachable: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RioCli {
    Output(String),
    Error(String),
}

#[allow(clippy::print_stdout)]
pub async fn run(p: &dyn Provider, kind: ProviderKind, json: bool) -> Result<()> {
    let ctx = k::current_context()?;
    if !p.context_matches(&ctx) {
        let hint = match provider::detect(&ctx) {
            Some(actual) => format!("or pass `-p {actual}` to see that cluster's status"),
            None => "or point KUBECONFIG at a {kind} cluster".into(),
        };
        bail!(
            "kubeconfig context '{ctx}' is not a {kind} cluster\n  \
             run `cargo xtask k8s -p {kind} kubeconfig` to switch, {hint}"
        );
    }
    let client = k::client().await?;
    let report = gather(&client, ctx).await;
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        ui::suspend(|| render_human(&report));
    }
    Ok(())
}

async fn gather(client: &k::Client, context: String) -> Report {
    Report {
        context,
        release: helm::release_status("rio", NS).ok().flatten(),
        deployments: k::list_deployment_status(client, NS)
            .await
            .unwrap_or_default(),
        daemonsets: k::list_daemonset_status(client, NS)
            .await
            .unwrap_or_default(),
        worker_pools: list_worker_pools(client).await.unwrap_or_default(),
        scheduler_leader: k::scheduler_leader(client, NS).await.ok(),
        problem_pods: k::problem_pods(client, NS).await.unwrap_or_default(),
        rio_cli: match k::run_in_scheduler(client, NS, &["rio-cli", "status"]).await {
            Ok(out) => RioCli::Output(out),
            Err(e) => RioCli::Error(format!("{e:#}")),
        },
    }
}

async fn list_worker_pools(client: &k::Client) -> Result<Vec<WpStatus>> {
    let api: Api<WorkerPool> = Api::namespaced(client.clone(), NS);
    let mut out: Vec<_> = api
        .list(&ListParams::default())
        .await?
        .into_iter()
        .map(|wp| {
            let name = wp.metadata.name.unwrap_or_default();
            let st = wp.status.unwrap_or_default();
            let reason = st
                .conditions
                .iter()
                .find(|c| c.type_ == "Scaling")
                .map(|c| c.reason.clone());
            let scheduler_unreachable = st
                .conditions
                .iter()
                .any(|c| c.type_ == "SchedulerUnreachable" && c.status == "True");
            WpStatus {
                name,
                ready: st.ready_replicas,
                replicas: st.replicas,
                desired: st.desired_replicas,
                reason,
                scheduler_unreachable,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

// -- human rendering ----------------------------------------------------

fn glyph(ok: bool) -> console::StyledObject<&'static str> {
    if ok {
        style("✓").green()
    } else {
        style("✗").red()
    }
}

#[allow(clippy::print_stderr)]
fn header(s: &str) {
    eprintln!("{} {}", style("▸").blue(), style(s).bold());
}

#[allow(clippy::print_stderr)]
fn render_human(r: &Report) {
    eprintln!("{} {}", style("context:").dim(), style(&r.context).cyan());
    eprintln!();

    header("Helm release");
    match &r.release {
        Some(rel) => {
            let tag = rel.image_tag.as_deref().unwrap_or("?");
            let ok = rel.status == "deployed";
            eprintln!(
                "  {} {}  rev {}  {}  {}  tag {}",
                glyph(ok),
                rel.name,
                rel.revision,
                rel.chart,
                style(&rel.status).dim(),
                style(tag).cyan()
            );
        }
        None => eprintln!("  {} not installed", style("✗").red()),
    }

    eprintln!();
    header("Deployments");
    let w = r
        .deployments
        .iter()
        .map(|d| d.name.len())
        .max()
        .unwrap_or(0);
    for d in &r.deployments {
        eprintln!(
            "  {} {:w$}  {}/{} ready  {} updated",
            glyph(d.ok),
            d.name,
            d.ready,
            d.want,
            d.updated
        );
    }

    eprintln!();
    header("DaemonSets");
    let w = r.daemonsets.iter().map(|d| d.name.len()).max().unwrap_or(0);
    for d in &r.daemonsets {
        eprintln!(
            "  {} {:w$}  {}/{} ready  {} scheduled",
            glyph(d.ok),
            d.name,
            d.ready,
            d.desired,
            d.scheduled
        );
    }

    eprintln!();
    header("WorkerPools");
    if r.worker_pools.is_empty() {
        eprintln!("  {} none", style("·").dim());
    }
    let w = r
        .worker_pools
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0);
    for p in &r.worker_pools {
        let ok = p.ready == p.desired && !p.scheduler_unreachable;
        let reason = p.reason.as_deref().unwrap_or("-");
        let unreach = if p.scheduler_unreachable {
            style(" SchedulerUnreachable").red().to_string()
        } else {
            String::new()
        };
        eprintln!(
            "  {} {:w$}  {}/{}→{}  {}{}",
            glyph(ok),
            p.name,
            p.ready,
            p.replicas,
            p.desired,
            style(reason).dim(),
            unreach
        );
    }

    eprintln!();
    header("Scheduler");
    match &r.scheduler_leader {
        Some(l) => eprintln!("  {} leader: {}", style("✓").green(), style(l).cyan()),
        None => eprintln!("  {} no leader", style("✗").red()),
    }

    if !r.problem_pods.is_empty() {
        eprintln!();
        header("Problem pods");
        let w = r
            .problem_pods
            .iter()
            .map(|p| p.name.len())
            .max()
            .unwrap_or(0);
        for p in &r.problem_pods {
            let reason = p.reason.as_deref().unwrap_or("-");
            eprintln!(
                "  {} {:w$}  {}  {}  restarts={}",
                style("✗").red(),
                p.name,
                style(&p.phase).yellow(),
                reason,
                p.restarts
            );
        }
    }

    eprintln!();
    header("rio-cli status");
    match &r.rio_cli {
        RioCli::Output(out) => {
            for line in out.lines() {
                eprintln!("  {line}");
            }
        }
        RioCli::Error(e) => eprintln!("  {} unreachable: {e}", style("✗").red()),
    }
}
