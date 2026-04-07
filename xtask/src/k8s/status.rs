//! `xtask k8s status` — one-shot deployment health report.
//!
//! Every section is best-effort: a missing release, unreachable
//! scheduler, or absent CRD renders as a status line, not a hard
//! error. The command should be useful precisely when things are
//! broken.

use std::collections::HashMap;
use std::net::Ipv4Addr;

use anyhow::{Context, Result, bail};
use console::style;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{Api, DeleteParams, ListParams};
use kube::core::{ApiResource, DynamicObject, GroupVersionKind};
use rio_crds::builderpool::BuilderPool;
use rio_crds::fetcherpool::FetcherPool;
use serde::Serialize;
use tracing::{debug, info};

use crate::k8s::eks::smoke::CliCtx;
use crate::k8s::provider::{Provider, ProviderKind};
use crate::k8s::{NAMESPACES, NS, NS_BUILDERS, NS_FETCHERS};
use crate::{helm, kube as k, ui};

/// Age threshold for "stuck" — 2 minutes. Below this, a pod/NodeClaim
/// is still plausibly starting; above, it's worth flagging.
const STUCK_SECS: u64 = 120;

/// Event lookback window — 5 minutes. FailedCreatePodSandBox events
/// older than this are stale (the pod either recovered or is already
/// counted elsewhere as a problem_pod).
const EVENT_WINDOW_SECS: u64 = 300;

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
    /// Per-subnet IP health. EKS-only (aws-sdk-ec2 describe-subnets);
    /// None for k3s (no subnet concept).
    subnets: Option<Vec<SubnetHealth>>,
    /// NodeClaims stuck in Unknown/Initialized=Unknown over 2min. The
    /// I-021/I-022 signal — Karpenter blocked on ResourceNotRegistered,
    /// InsufficientCapacity, etc.
    stuck_nodeclaims: Vec<StuckNodeClaim>,
    /// Scheduler Prometheus scrape via port-forward (leader pod :9091).
    /// The I-025 signal — fod_queue_depth + fetcher_utilization.
    scheduler_metrics: Option<SchedulerMetrics>,
    /// Pods with FailedCreatePodSandBox events in the last 5min.
    /// The I-022/I-027 signal — aws-cni IP assignment failures.
    ip_assign_failures: Vec<IpAssignFailure>,
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

/// FetcherPool status. Same shape as BuilderPool now (I-014):
/// {min,max} bounds, autoscaler-driven `desired`.
#[derive(Serialize)]
pub struct FpStatus {
    name: String,
    ready: i32,
    /// `status.desiredReplicas` — what the autoscaler set on the
    /// STS (or `spec.replicas.min` before first reconcile).
    desired: i32,
    max: i32,
}

#[derive(Serialize)]
pub struct SubnetHealth {
    az: String,
    cidr: String,
    available_ips: i32,
    node_count: usize,
    /// Heuristic: free IPs > 50 but ip_assign_failures is nonempty
    /// on nodes in this subnet → subnet is fragmented, prefix
    /// delegation can't find a contiguous /28. I-027.
    possibly_fragmented: bool,
}

#[derive(Serialize)]
pub struct StuckNodeClaim {
    name: String,
    nodepool: String,
    age_secs: u64,
    /// From .status.conditions — "ResourceNotRegistered",
    /// "InsufficientCapacity", "NodeNotReady", etc.
    blocking_reason: String,
}

#[derive(Serialize)]
pub struct SchedulerMetrics {
    fod_queue_depth: f64,
    fetcher_utilization: f64,
    derivations_queued: f64,
    /// fod_queue_depth > 0 && fetcher_utilization == 0.
    /// The I-025 check — FODs queued but no fetcher streams.
    possible_freeze: bool,
}

#[derive(Serialize)]
pub struct IpAssignFailure {
    pod: String,
    namespace: String,
    node: String,
    age_secs: u64,
}

#[derive(Serialize)]
#[serde(rename_all = "snake_case")]
enum RioCli {
    Output(String),
    Error(String),
}

#[allow(clippy::print_stdout, clippy::print_stderr)]
pub async fn run(
    p: &dyn Provider,
    kind: ProviderKind,
    cfg: &crate::config::XtaskConfig,
    json: bool,
    reap_stuck_nodes: bool,
) -> Result<()> {
    // `-p k3s` means "show me k3s" — if kubeconfig points elsewhere,
    // switch it. Only error if the switch itself fails (e.g. k3s
    // not installed), not just because we're currently on EKS.
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
    let report = gather(&client, ctx, kind).await;

    if reap_stuck_nodes {
        ui::suspend(|| render_human(&report));
        reap(&client, &report).await?;
        // Re-gather and render the post-reap state.
        let ctx = k::current_context()?;
        let after = gather(&client, ctx, kind).await;
        eprintln!();
        eprintln!("{}", style("── post-reap ──────────────────").dim());
        if json {
            println!("{}", serde_json::to_string_pretty(&after)?);
        } else {
            ui::suspend(|| render_human(&after));
        }
        return Ok(());
    }

    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        ui::suspend(|| render_human(&report));
    }
    Ok(())
}

pub(crate) async fn gather(client: &k::Client, context: String, kind: ProviderKind) -> Report {
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

    // Node → InternalIP map, used by subnet gathering.
    let node_ips = node_internal_ips(client).await;

    let ip_assign_failures = gather_ip_assign_failures(client).await;

    // Subnets last: possibly_fragmented needs ip_assign_failures + node_ips.
    let subnets = match kind {
        ProviderKind::Eks => gather_subnets(&node_ips, &ip_assign_failures).await,
        _ => None,
    };

    Report {
        context,
        release: helm::release_status("rio", NS).ok().flatten(),
        namespaces,
        builder_pools: list_builder_pools(client).await.unwrap_or_default(),
        fetcher_pools: list_fetcher_pools(client).await.unwrap_or_default(),
        subnets,
        stuck_nodeclaims: gather_stuck_nodeclaims(client).await,
        scheduler_metrics: gather_scheduler_metrics(client).await,
        ip_assign_failures,
        // Lease lives in rio-system (scheduler's own namespace).
        scheduler_leader: k::scheduler_leader(client, NS).await.ok(),
        // Local rio-cli via port-forward + fetched mTLS — NOT in-pod
        // exec (scheduler image shouldn't bundle rio-cli). Best-effort:
        // a failed tunnel or cert-fetch renders as an error line, same
        // as every other section in this report.
        rio_cli: match CliCtx::open(client, 0, 0).await {
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

// -- gather helpers (all best-effort) -----------------------------------

/// Map of node name → InternalIP. Used by subnet mapping and by
/// stuck-node detection (to check `unschedulable`).
async fn node_internal_ips(client: &k::Client) -> HashMap<String, (String, bool)> {
    let api: Api<Node> = Api::all(client.clone());
    let Ok(nodes) = api.list(&ListParams::default()).await else {
        debug!("node list failed");
        return HashMap::new();
    };
    nodes
        .into_iter()
        .filter_map(|n| {
            let name = n.metadata.name?;
            let cordoned = n
                .spec
                .as_ref()
                .and_then(|s| s.unschedulable)
                .unwrap_or(false);
            let ip = n
                .status?
                .addresses?
                .into_iter()
                .find(|a| a.type_ == "InternalIP")?
                .address;
            Some((name, (ip, cordoned)))
        })
        .collect()
}

/// EC2 describe-subnets filtered by karpenter.sh/discovery tag.
/// `possibly_fragmented`: free IPs > 50 but a pod on a node in this
/// subnet had FailedCreatePodSandBox — aws-cni's ENI prefix delegation
/// wants a contiguous /28, which fragmentation denies even when the
/// total free-IP count looks healthy.
async fn gather_subnets(
    node_ips: &HashMap<String, (String, bool)>,
    ip_failures: &[IpAssignFailure],
) -> Option<Vec<SubnetHealth>> {
    use crate::k8s::eks::TF_DIR;
    let tf = crate::tofu::outputs(TF_DIR)
        .inspect_err(|e| debug!("tofu outputs: {e:#}"))
        .ok()?;
    let cluster = tf.get("cluster_name").ok()?;
    let region = tf.get("region").ok()?;

    let conf = crate::aws::config(Some(&region)).await;
    let ec2 = aws_sdk_ec2::Client::new(conf);

    let subnets = ec2
        .describe_subnets()
        .filters(
            aws_sdk_ec2::types::Filter::builder()
                .name("tag:karpenter.sh/discovery")
                .values(&cluster)
                .build(),
        )
        .send()
        .await
        .inspect_err(|e| debug!("ec2 describe-subnets: {e}"))
        .ok()?
        .subnets?;

    // Nodes whose pods had IP-assign failures, by name.
    let failing_nodes: std::collections::HashSet<&str> =
        ip_failures.iter().map(|f| f.node.as_str()).collect();

    let mut out: Vec<_> = subnets
        .into_iter()
        .filter_map(|s| {
            let cidr = s.cidr_block?;
            let az = s.availability_zone.unwrap_or_default();
            let available_ips = s.available_ip_address_count.unwrap_or(0);
            // Count nodes whose InternalIP falls in this CIDR, and
            // note whether any of them had IP-assign failures.
            let mut node_count = 0;
            let mut has_failing_node = false;
            for (name, (ip, _)) in node_ips {
                if cidr_contains(&cidr, ip) {
                    node_count += 1;
                    if failing_nodes.contains(name.as_str()) {
                        has_failing_node = true;
                    }
                }
            }
            Some(SubnetHealth {
                az,
                cidr,
                available_ips,
                node_count,
                possibly_fragmented: available_ips > 50 && has_failing_node,
            })
        })
        .collect();
    out.sort_by(|a, b| a.az.cmp(&b.az));
    Some(out)
}

/// NodeClaims whose Ready condition is Unknown with lastTransitionTime
/// >2min ago. Karpenter CRD — no k8s-openapi type, use DynamicObject.
async fn gather_stuck_nodeclaims(client: &k::Client) -> Vec<StuckNodeClaim> {
    let api = nodeclaim_api(client);
    let Ok(claims) = api.list(&ListParams::default()).await else {
        debug!("NodeClaim list failed (CRD not installed?)");
        return Vec::new();
    };
    let now = jiff::Timestamp::now();
    let mut out: Vec<_> = claims
        .into_iter()
        .filter_map(|nc| {
            let name = nc.metadata.name.clone()?;
            let nodepool = nc
                .metadata
                .labels
                .as_ref()
                .and_then(|l| l.get("karpenter.sh/nodepool").cloned())
                .unwrap_or_default();
            // Ready=Unknown with lastTransitionTime > STUCK_SECS ago
            // means Karpenter launched the instance but the node
            // never joined. blocking_reason is the first non-True
            // condition's Reason — typically the useful one
            // (ResourceNotRegistered, InsufficientCapacity, …).
            let conds = nc.data.pointer("/status/conditions")?.as_array()?;
            let ready = conds
                .iter()
                .find(|c| c.get("type").and_then(|v| v.as_str()) == Some("Ready"))?;
            if ready.get("status").and_then(|v| v.as_str()) != Some("Unknown") {
                return None;
            }
            let since = ready
                .get("lastTransitionTime")
                .and_then(|v| v.as_str())
                .and_then(|s| s.parse::<jiff::Timestamp>().ok())?;
            let age_secs = now.duration_since(since).as_secs().max(0) as u64;
            if age_secs <= STUCK_SECS {
                return None;
            }
            let blocking_reason = conds
                .iter()
                .find(|c| c.get("status").and_then(|v| v.as_str()) != Some("True"))
                .and_then(|c| c.get("reason").and_then(|v| v.as_str()))
                .unwrap_or("Unknown")
                .to_string();
            Some(StuckNodeClaim {
                name,
                nodepool,
                age_secs,
                blocking_reason,
            })
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// Port-forward to the scheduler leader's :9091, GET /metrics once,
/// parse three gauges. Reuses the minimal HTTP/1.0-over-portforward
/// approach from smoke.rs::sched_metric — one scrape, three parses.
async fn gather_scheduler_metrics(client: &k::Client) -> Option<SchedulerMetrics> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let leader = k::scheduler_leader(client, NS)
        .await
        .inspect_err(|e| debug!("scheduler leader: {e:#}"))
        .ok()?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    let mut pf = pods
        .portforward(&leader, &[9091])
        .await
        .inspect_err(|e| debug!("portforward: {e:#}"))
        .ok()?;
    let mut stream = pf.take_stream(9091)?;
    stream
        .write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await
        .ok()?;
    let mut body = String::new();
    stream.read_to_string(&mut body).await.ok()?;

    let parse = |name: &str| -> f64 {
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix(name)
                && let Some(v) = rest.split_whitespace().last()
            {
                return v.parse().unwrap_or(0.0);
            }
        }
        0.0
    };
    let fod_queue_depth = parse("rio_scheduler_fod_queue_depth");
    let fetcher_utilization = parse("rio_scheduler_fetcher_utilization");
    let derivations_queued = parse("rio_scheduler_derivations_queued");
    Some(SchedulerMetrics {
        fod_queue_depth,
        fetcher_utilization,
        derivations_queued,
        possible_freeze: fod_queue_depth > 0.0 && fetcher_utilization == 0.0,
    })
}

/// Events with reason=FailedCreatePodSandBox across all rio
/// namespaces, filtered to aws-cni IP-assign failures in the last
/// 5min. involvedObject gives the pod name; message contains the node
/// name (aws-cni logs it as "...on node ip-10-0-x-y...").
async fn gather_ip_assign_failures(client: &k::Client) -> Vec<IpAssignFailure> {
    let now = jiff::Timestamp::now();
    let mut out = Vec::new();
    for &(ns, _) in NAMESPACES {
        let api: Api<Event> = Api::namespaced(client.clone(), ns);
        let lp = ListParams::default().fields("reason=FailedCreatePodSandBox");
        let Ok(events) = api.list(&lp).await else {
            debug!("event list in {ns} failed");
            continue;
        };
        for ev in events {
            let msg = ev.message.as_deref().unwrap_or_default();
            // aws-cni failure messages look like:
            // "failed to setup network for sandbox ...: plugin type=\"aws-cni\" ...: failed to assign an IP address to container"
            if !(msg.contains("aws-cni") && msg.contains("failed to assign")) {
                continue;
            }
            let since = match ev.last_timestamp.as_ref() {
                Some(t) => t.0,
                None => continue,
            };
            let age_secs = now.duration_since(since).as_secs().max(0) as u64;
            if age_secs > EVENT_WINDOW_SECS {
                continue;
            }
            let pod = ev.involved_object.name.unwrap_or_default();
            // Node name isn't a first-class field on the event; the
            // pod that failed was scheduled to a node but the sandbox
            // create failed there. source.host is the kubelet's node.
            let node = ev.source.and_then(|s| s.host).unwrap_or_default();
            out.push(IpAssignFailure {
                pod,
                namespace: ns.to_string(),
                node,
                age_secs,
            });
        }
    }
    out.sort_by_key(|f| f.age_secs);
    out
}

/// Karpenter NodeClaim CRD via the dynamic API. Cluster-scoped.
fn nodeclaim_api(client: &k::Client) -> Api<DynamicObject> {
    let gvk = GroupVersionKind::gvk("karpenter.sh", "v1", "NodeClaim");
    let ar = ApiResource::from_gvk(&gvk);
    Api::all_with(client.clone(), &ar)
}

/// True if `ip` falls inside `cidr` (IPv4 only — EKS VPC subnets
/// are always IPv4 primary CIDRs).
fn cidr_contains(cidr: &str, ip: &str) -> bool {
    let Some((net, prefix)) = cidr.split_once('/') else {
        return false;
    };
    let Ok(prefix) = prefix.parse::<u32>() else {
        return false;
    };
    let (Ok(net), Ok(ip)) = (net.parse::<Ipv4Addr>(), ip.parse::<Ipv4Addr>()) else {
        return false;
    };
    let mask = if prefix == 0 {
        0
    } else {
        !0u32 << (32 - prefix)
    };
    (u32::from(net) & mask) == (u32::from(ip) & mask)
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
        .map(|fp| {
            let max = fp.spec.max_concurrent as i32;
            let min = 0i32;
            FpStatus {
                name: fp.metadata.name.unwrap_or_default(),
                ready: fp
                    .status
                    .as_ref()
                    .map(|s| s.ready_replicas)
                    .unwrap_or_default(),
                desired: fp.status.map(|s| s.desired_replicas).unwrap_or(min),
                max,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

// -- reap (stuck NodeClaim cleanup) -------------------------------------

/// Delete stuck NodeClaims that never reached Ready. Karpenter
/// reprovisions healthy replacements. Idempotent: deleting an
/// already-deleted NodeClaim is ignored.
#[allow(clippy::print_stderr)]
async fn reap(client: &k::Client, r: &Report) -> Result<()> {
    let claims = nodeclaim_api(client);

    let mut deleted = 0usize;
    for snc in &r.stuck_nodeclaims {
        if let Err(e) = claims.delete(&snc.name, &DeleteParams::default()).await {
            debug!("delete stuck NodeClaim {}: {e:#}", snc.name);
        } else {
            deleted += 1;
            info!(
                "deleted stuck NodeClaim {} ({})",
                snc.name, snc.blocking_reason
            );
        }
    }

    eprintln!();
    eprintln!(
        "  {} {deleted} stuck NodeClaims deleted",
        style("✓").green()
    );
    Ok(())
}

// -- preflight (deploy gate) --------------------------------------------

/// Bail with an actionable message if the cluster is in a state where
/// `helm upgrade` will likely wedge (stuck on pending pods that will
/// never get IPs, blocked on a prior pending-upgrade, etc).
pub(crate) fn preflight_check(r: &Report) -> Result<()> {
    if let Some(subnets) = &r.subnets {
        for s in subnets {
            if s.available_ips <= 20 {
                bail!(
                    "Subnet {} ({}) has {} free IPs — pods will likely fail IP assignment. \
                     Scale down workers or add secondary CIDR. Bypass: --deploy-skip-preflight",
                    s.az,
                    s.cidr,
                    s.available_ips
                );
            }
        }
    }

    if !r.ip_assign_failures.is_empty() {
        let nodes: std::collections::BTreeSet<&str> = r
            .ip_assign_failures
            .iter()
            .map(|f| f.node.as_str())
            .collect();
        let nodes: Vec<&str> = nodes.into_iter().collect();
        bail!(
            "{} pods had FailedCreatePodSandBox in last 5min. Nodes: {}. \
             Run `status --reap-stuck-nodes` first. Bypass: --deploy-skip-preflight",
            r.ip_assign_failures.len(),
            nodes.join(", ")
        );
    }

    if !r.stuck_nodeclaims.is_empty() {
        let reasons: std::collections::BTreeSet<&str> = r
            .stuck_nodeclaims
            .iter()
            .map(|n| n.blocking_reason.as_str())
            .collect();
        let reasons: Vec<&str> = reasons.into_iter().collect();
        bail!(
            "{} NodeClaims stuck Unknown — Karpenter blocked. Reasons: {}. \
             Run `status --reap-stuck-nodes` first. Bypass: --deploy-skip-preflight",
            r.stuck_nodeclaims.len(),
            reasons.join(", ")
        );
    }

    if let Some(rel) = &r.release
        && rel.status.starts_with("pending")
    {
        bail!(
            "Helm release in {} state — prior deploy incomplete. \
             `helm rollback rio` or delete the pending secret. Bypass: --deploy-skip-preflight",
            rel.status
        );
    }

    Ok(())
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
        let ok = p.ready == p.desired;
        eprintln!(
            "  {} {:w$}  {}/{}→{}",
            glyph(ok),
            p.name,
            p.ready,
            p.desired,
            p.max
        );
    }

    if let Some(subnets) = &r.subnets {
        eprintln!();
        header("Subnets (EKS)");
        let w = subnets.iter().map(|s| s.az.len()).max().unwrap_or(0);
        for s in subnets {
            let ok = s.available_ips > 20 && !s.possibly_fragmented;
            let frag = if s.possibly_fragmented {
                style(" fragmented?").yellow().to_string()
            } else {
                String::new()
            };
            eprintln!(
                "  {} {:w$}  {:>5} free IPs  {:>2} nodes  {}{}",
                glyph(ok),
                s.az,
                s.available_ips,
                s.node_count,
                style(&s.cidr).dim(),
                frag
            );
        }
    }

    if !r.stuck_nodeclaims.is_empty() {
        eprintln!();
        header("Stuck NodeClaims");
        let w = r
            .stuck_nodeclaims
            .iter()
            .map(|n| n.name.len())
            .max()
            .unwrap_or(0);
        for n in &r.stuck_nodeclaims {
            eprintln!(
                "  {} {:w$}  {}  {}s  pool={}",
                style("✗").red(),
                n.name,
                style(&n.blocking_reason).yellow(),
                n.age_secs,
                style(&n.nodepool).dim()
            );
        }
    }

    if !r.ip_assign_failures.is_empty() {
        eprintln!();
        header("IP-assign failures (last 5min)");
        for f in &r.ip_assign_failures {
            eprintln!(
                "  {} {}/{}  on {}  {}s ago",
                style("✗").red(),
                f.namespace,
                f.pod,
                style(&f.node).dim(),
                f.age_secs
            );
        }
    }

    eprintln!();
    header("Scheduler");
    match &r.scheduler_leader {
        Some(l) => eprintln!("  {} leader: {}", style("✓").green(), style(l).cyan()),
        None => eprintln!("  {} no leader", style("✗").red()),
    }
    if let Some(m) = &r.scheduler_metrics {
        let freeze = if m.possible_freeze {
            style(" FREEZE?").red().to_string()
        } else {
            String::new()
        };
        eprintln!(
            "  {} fod_queue={}  fetcher_util={:.2}  queued={}{}",
            glyph(!m.possible_freeze),
            m.fod_queue_depth,
            m.fetcher_utilization,
            m.derivations_queued,
            freeze
        );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cidr_contains_basics() {
        assert!(cidr_contains("10.0.32.0/19", "10.0.32.1"));
        assert!(cidr_contains("10.0.32.0/19", "10.0.63.254"));
        assert!(!cidr_contains("10.0.32.0/19", "10.0.64.1"));
        assert!(!cidr_contains("10.0.32.0/19", "10.0.31.255"));
        assert!(cidr_contains("0.0.0.0/0", "1.2.3.4"));
        assert!(!cidr_contains("garbage", "10.0.0.1"));
        assert!(!cidr_contains("10.0.0.0/19", "garbage"));
    }

    #[test]
    fn preflight_blocks_on_low_ips() {
        let r = Report {
            context: String::new(),
            release: None,
            namespaces: vec![],
            builder_pools: vec![],
            fetcher_pools: vec![],
            subnets: Some(vec![SubnetHealth {
                az: "us-east-2a".into(),
                cidr: "10.0.32.0/19".into(),
                available_ips: 5,
                node_count: 3,
                possibly_fragmented: false,
            }]),
            stuck_nodeclaims: vec![],
            scheduler_metrics: None,
            ip_assign_failures: vec![],
            scheduler_leader: None,
            rio_cli: RioCli::Error(String::new()),
        };
        let err = preflight_check(&r).unwrap_err().to_string();
        assert!(err.contains("us-east-2a"), "{err}");
        assert!(err.contains("5 free IPs"), "{err}");
        assert!(err.contains("--deploy-skip-preflight"), "{err}");
    }

    #[test]
    fn preflight_blocks_on_pending_helm() {
        let r = Report {
            context: String::new(),
            release: Some(helm::ReleaseStatus {
                name: "rio".into(),
                revision: "3".into(),
                status: "pending-upgrade".into(),
                chart: "rio-build-0.1.0".into(),
                app_version: String::new(),
                image_tag: None,
            }),
            namespaces: vec![],
            builder_pools: vec![],
            fetcher_pools: vec![],
            subnets: None,
            stuck_nodeclaims: vec![],
            scheduler_metrics: None,
            ip_assign_failures: vec![],
            scheduler_leader: None,
            rio_cli: RioCli::Error(String::new()),
        };
        let err = preflight_check(&r).unwrap_err().to_string();
        assert!(err.contains("pending-upgrade"), "{err}");
        assert!(err.contains("helm rollback"), "{err}");
    }

    #[test]
    fn preflight_passes_healthy() {
        let r = Report {
            context: String::new(),
            release: Some(helm::ReleaseStatus {
                name: "rio".into(),
                revision: "3".into(),
                status: "deployed".into(),
                chart: "rio-build-0.1.0".into(),
                app_version: String::new(),
                image_tag: None,
            }),
            namespaces: vec![],
            builder_pools: vec![],
            fetcher_pools: vec![],
            subnets: Some(vec![SubnetHealth {
                az: "us-east-2a".into(),
                cidr: "10.0.32.0/19".into(),
                available_ips: 4000,
                node_count: 3,
                possibly_fragmented: false,
            }]),
            stuck_nodeclaims: vec![],
            scheduler_metrics: None,
            ip_assign_failures: vec![],
            scheduler_leader: None,
            rio_cli: RioCli::Error(String::new()),
        };
        preflight_check(&r).unwrap();
    }
}
