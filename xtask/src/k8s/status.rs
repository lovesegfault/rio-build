//! `xtask k8s status` — one-shot deployment health report.
//!
//! Every section is best-effort: a missing release, unreachable
//! scheduler, or absent CRD renders as a status line, not a hard
//! error. The command should be useful precisely when things are
//! broken.

use anyhow::{Context, Result};
use console::style;
use kube::api::{Api, ListParams};
use rio_crds::builderpool::BuilderPool;
use rio_crds::fetcherpool::FetcherPool;
use serde::Serialize;

use crate::k8s::eks::smoke::CliCtx;
use crate::k8s::provider::{Provider, ProviderKind};
use crate::k8s::{NAMESPACES, NS, NS_BUILDERS, NS_FETCHERS};
use crate::{helm, kube as k, ui};

#[derive(Serialize)]
pub struct Report {
    context: String,
    release: Option<helm::ReleaseStatus>,
    /// Per-namespace workload breakdown. ADR-019 four-namespace split:
    /// control plane in rio-system, store in rio-store, builders in
    /// rio-builders, fetchers in rio-fetchers.
    namespaces: Vec<NsReport>,
    builder_pools: Vec<BpStatus>,
    fetcher_pools: Vec<FpStatus>,
    scheduler_leader: Option<String>,
    rio_cli: RioCli,
}

#[derive(Serialize)]
pub struct NsReport {
    name: &'static str,
    deployments: Vec<k::DeployStatus>,
    daemonsets: Vec<k::DsStatus>,
    problem_pods: Vec<k::PodProblem>,
}

#[derive(Serialize)]
pub struct BpStatus {
    name: String,
    ready: i32,
    replicas: i32,
    desired: i32,
    /// Last `Scaling` condition reason (ScaledUp/ScaledDown/UnknownMetric).
    reason: Option<String>,
    /// `SchedulerUnreachable` condition is True.
    scheduler_unreachable: bool,
}

/// FetcherPool status is simpler — fixed replicas, no autoscaling
/// conditions (FetcherPoolStatus carries only ready_replicas).
#[derive(Serialize)]
pub struct FpStatus {
    name: String,
    ready: i32,
    replicas: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RioCli {
    Output(String),
    Error(String),
}

#[allow(clippy::print_stdout)]
pub async fn run(
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &crate::config::XtaskConfig,
    json: bool,
) -> Result<()> {
    // `-p kind` means "show me kind" — if kubeconfig points elsewhere,
    // switch it. Only error if the switch itself fails (e.g. kind
    // cluster doesn't exist), not just because we're currently on EKS.
    let ctx = k::current_context().unwrap_or_default();
    if !p.context_matches(&ctx) {
        tracing::info!("switching kubeconfig: {ctx} → {kind}");
        p.kubeconfig(cfg).await.with_context(|| {
            format!(
                "can't switch to {kind} — is the cluster running?\n  \
                 (was on '{ctx}'; run `cargo xtask k8s -p {kind} provision` \
                 if {kind} isn't up yet)"
            )
        })?;
    }
    let ctx = k::current_context()?;
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
    let mut namespaces = Vec::with_capacity(NAMESPACES.len());
    for &(ns, _) in NAMESPACES {
        namespaces.push(NsReport {
            name: ns,
            deployments: k::list_deployment_status(client, ns)
                .await
                .unwrap_or_default(),
            daemonsets: k::list_daemonset_status(client, ns)
                .await
                .unwrap_or_default(),
            problem_pods: k::problem_pods(client, ns).await.unwrap_or_default(),
        });
    }
    Report {
        context,
        release: helm::release_status("rio", NS).ok().flatten(),
        namespaces,
        builder_pools: list_builder_pools(client).await.unwrap_or_default(),
        fetcher_pools: list_fetcher_pools(client).await.unwrap_or_default(),
        // Lease lives in rio-system (scheduler's own namespace).
        scheduler_leader: k::scheduler_leader(client, NS).await.ok(),
        // Local rio-cli via port-forward + fetched mTLS — NOT in-pod
        // exec (scheduler image shouldn't bundle rio-cli). Best-effort:
        // a failed tunnel or cert-fetch renders as an error line, same
        // as every other section in this report.
        rio_cli: match CliCtx::open(client, 19001, 19002).await {
            Ok(cli) => match cli.run(&["status"]) {
                Ok(out) => RioCli::Output(out),
                // Err arm: sh::try_read bakes stdout+stderr head (512
                // chars) into the error message — {e:#} shows the
                // rio-cli diagnostic, not bare "command failed".
                Err(e) => RioCli::Error(format!("{e:#}")),
            },
            Err(e) => RioCli::Error(format!("tunnel: {e:#}")),
        },
    }
}

async fn list_builder_pools(client: &k::Client) -> Result<Vec<BpStatus>> {
    let api: Api<BuilderPool> = Api::namespaced(client.clone(), NS_BUILDERS);
    let mut out: Vec<_> = api
        .list(&ListParams::default())
        .await?
        .into_iter()
        .map(|bp| {
            let name = bp.metadata.name.unwrap_or_default();
            let st = bp.status.unwrap_or_default();
            let reason = st
                .conditions
                .iter()
                .find(|c| c.type_ == "Scaling")
                .map(|c| c.reason.clone());
            let scheduler_unreachable = st
                .conditions
                .iter()
                .any(|c| c.type_ == "SchedulerUnreachable" && c.status == "True");
            BpStatus {
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

async fn list_fetcher_pools(client: &k::Client) -> Result<Vec<FpStatus>> {
    let api: Api<FetcherPool> = Api::namespaced(client.clone(), NS_FETCHERS);
    let mut out: Vec<_> = api
        .list(&ListParams::default())
        .await?
        .into_iter()
        .map(|fp| FpStatus {
            name: fp.metadata.name.unwrap_or_default(),
            ready: fp.status.map(|s| s.ready_replicas).unwrap_or_default(),
            replicas: fp.spec.replicas,
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

    for ns in &r.namespaces {
        // Skip empty namespaces (rio-builders/rio-fetchers only have
        // STS pods, which show under BuilderPools/FetcherPools below).
        if ns.deployments.is_empty() && ns.daemonsets.is_empty() && ns.problem_pods.is_empty() {
            continue;
        }
        eprintln!();
        header(&format!("Namespace {}", ns.name));
        let w = ns
            .deployments
            .iter()
            .map(|d| d.name.len())
            .max()
            .unwrap_or(0);
        for d in &ns.deployments {
            eprintln!(
                "  {} {:w$}  {}/{} ready  {} updated",
                glyph(d.ok),
                d.name,
                d.ready,
                d.want,
                d.updated
            );
        }
        let w = ns
            .daemonsets
            .iter()
            .map(|d| d.name.len())
            .max()
            .unwrap_or(0);
        for d in &ns.daemonsets {
            eprintln!(
                "  {} {:w$}  {}/{} ready  {} scheduled",
                glyph(d.ok),
                d.name,
                d.ready,
                d.desired,
                d.scheduled
            );
        }
        for p in &ns.problem_pods {
            let reason = p.reason.as_deref().unwrap_or("-");
            eprintln!(
                "  {} {}  {}  {}  restarts={}",
                style("✗").red(),
                p.name,
                style(&p.phase).yellow(),
                reason,
                p.restarts
            );
        }
    }

    eprintln!();
    header("BuilderPools");
    if r.builder_pools.is_empty() {
        eprintln!("  {} none", style("·").dim());
    }
    let w = r
        .builder_pools
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0);
    for p in &r.builder_pools {
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
    header("FetcherPools");
    if r.fetcher_pools.is_empty() {
        eprintln!("  {} none", style("·").dim());
    }
    let w = r
        .fetcher_pools
        .iter()
        .map(|p| p.name.len())
        .max()
        .unwrap_or(0);
    for p in &r.fetcher_pools {
        let ok = p.ready == p.replicas;
        eprintln!("  {} {:w$}  {}/{}", glyph(ok), p.name, p.ready, p.replicas);
    }

    eprintln!();
    header("Scheduler");
    match &r.scheduler_leader {
        Some(l) => eprintln!("  {} leader: {}", style("✓").green(), style(l).cyan()),
        None => eprintln!("  {} no leader", style("✗").red()),
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
