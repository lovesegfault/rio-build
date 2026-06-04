//! Build→node attribution + passive co-tenancy tagging.
//!
//! Every 5 s while the bench build runs: match the bench drv against
//! `rio-cli workers --json` (`DebugExecutorState.running_build`), map
//! the executor pod to its node, then sample that node's mountd
//! (`rio_mountd_connections_current` — "builds being served on this
//! node" — plus the Promote/request counters) and the executor's
//! castore-FUSE counters. Passive only: nothing is pinned or cordoned;
//! a contended sample just tags the run `contended`, which blocks
//! baseline saves and compares.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::Result;
use k8s_openapi::api::core::v1::{Node, Pod};
use kube::Api;
use serde_json::Value;
use tokio::time::{Duration, interval};
use tracing::{debug, warn};

use crate::k8s::NS_BUILDERS;
use crate::k8s::eks::smoke::CliCtx;
use crate::k8s::status::{Scrape, scrape_pod};

const SAMPLE_EVERY: Duration = Duration::from_secs(5);
const MOUNTD_METRICS_PORT: u16 = 9095;
const EXECUTOR_METRICS_PORT: u16 = 9093;
const MOUNTD_LABEL: &str = "app.kubernetes.io/name=rio-mountd";

#[derive(Debug, Clone)]
pub struct Sample {
    pub epoch_ms: u64,
    pub connections_current: f64,
    pub mountd_promote_bytes: f64,
    pub mountd_request_count: BTreeMap<String, f64>,
    pub mountd_request_sum: BTreeMap<String, f64>,
    /// Disk-pressure LRU sweep evictions, by dir (cache/chunks). A
    /// warm-leak verdict reads differently when the node cache was
    /// actually evicting versus quiet.
    pub mountd_cache_evicted: BTreeMap<String, f64>,
    /// `None` when the executor scrape failed on this tick (pod
    /// terminating, port hiccup) — mountd (a long-lived DS) is the
    /// resilient half.
    pub builder: Option<BuilderSample>,
}

#[derive(Debug, Clone)]
pub struct BuilderSample {
    pub open_case: BTreeMap<String, f64>,
    pub fetch_bytes: BTreeMap<String, f64>,
    pub open_seconds_count: f64,
}

#[derive(Debug, Default)]
pub struct CotenancyReport {
    /// `exact` | `unattributed`.
    pub attribution: String,
    pub node: Option<String>,
    pub instance_type: Option<String>,
    pub capacity_type: Option<String>,
    pub node_kernel: Option<String>,
    pub ami: Option<String>,
    /// Any sample saw >1 mountd connection on the bench node.
    pub contended: bool,
    pub max_co_tenants: u64,
    /// Executor pod UID changed between samples → the pod restarted
    /// mid-run → its counters reset → deltas are garbage.
    pub executor_uid_changed: bool,
    pub samples: Vec<Sample>,
}

/// Sample until `stop` flips. Never errors — a bench run with failed
/// attribution is still a run; it just comes back `unattributed`.
pub async fn watch(
    client: kube::Client,
    cli: Arc<CliCtx>,
    drv_path: Option<String>,
    mut stop: tokio::sync::watch::Receiver<bool>,
) -> CotenancyReport {
    let mut report = CotenancyReport {
        attribution: "unattributed".into(),
        ..Default::default()
    };
    // The drv hash is the join key against running_build (which may be
    // a bare hash, a basename, or a full path depending on the proto
    // consumer — substring containment covers all three).
    let drv_hash = drv_path.as_deref().and_then(store_hash);
    if drv_hash.is_none() {
        debug!("no drv path from pre-eval; co-tenancy will be unattributed");
    }

    let mut executor_pod: Option<String> = None;
    let mut executor_uid: Option<String> = None;
    let mut mountd_pod: Option<String> = None;
    let mut unmatched_polls = 0u32;

    let mut tick = interval(SAMPLE_EVERY);
    loop {
        tokio::select! {
            _ = stop.changed() => break,
            _ = tick.tick() => {}
        }

        // Attribution: poll the scheduler's ACTOR view until our drv
        // shows up as someone's running_build. Keeps polling every
        // tick until then — the bench build starts only after its
        // dataset drv finishes, which can be minutes into the nix
        // invocation. A miss must be LOUD past the grace window:
        // run 4 spent its whole window silently unattributed because
        // the matcher polled a view without the field it matched on.
        if executor_pod.is_none()
            && let Some(hash) = drv_hash.as_deref()
        {
            let exec_id = match poll_workers(&cli).await {
                Ok(entries) => {
                    let matched = match_executor(&entries, hash);
                    if matched.is_none() {
                        unmatched_polls += 1;
                        if warn_now(unmatched_polls) {
                            let running: Vec<&str> =
                                entries.iter().filter_map(|(_, r)| r.as_deref()).collect();
                            warn!(
                                "bench build unattributed after {unmatched_polls} polls: \
                                 looking for drv hash {hash}; workers --actor reports \
                                 running_build = {running:?}"
                            );
                        }
                    }
                    matched
                }
                Err(e) => {
                    unmatched_polls += 1;
                    if warn_now(unmatched_polls) {
                        warn!("co-tenancy attribution poll failed ({unmatched_polls} polls): {e}");
                    }
                    None
                }
            };
            if let Some(exec_id) = exec_id {
                match resolve_node(&client, &exec_id).await {
                    Ok((node, uid)) => {
                        debug!("bench build attributed: executor={exec_id} node={node}");
                        executor_uid = Some(uid);
                        mountd_pod = mountd_on_node(&client, &node).await;
                        if mountd_pod.is_none() {
                            warn!("no mountd pod found on {node} — no co-tenancy samples");
                        }
                        describe_node(&client, &node, &mut report).await;
                        report.node = Some(node);
                        report.attribution = "exact".into();
                        executor_pod = Some(exec_id);
                    }
                    Err(e) => warn!("executor pod resolve failed: {e:#}"),
                }
            }
        }

        let Some(mountd) = mountd_pod.as_deref() else {
            continue;
        };
        match sample_once(&client, mountd, executor_pod.as_deref()).await {
            Ok(s) => {
                let conns = s.connections_current as u64;
                report.max_co_tenants = report.max_co_tenants.max(conns);
                if s.connections_current > 1.0 {
                    report.contended = true;
                }
                report.samples.push(s);
            }
            Err(e) => debug!("co-tenancy sample failed: {e:#}"),
        }
        // Pod-UID validation: a restart resets the executor's counters,
        // so before/after deltas would be garbage.
        if let (Some(pod), Some(first_uid)) = (executor_pod.as_deref(), executor_uid.as_deref())
            && let Ok(Some(uid)) = pod_uid(&client, pod).await
            && uid != first_uid
        {
            report.executor_uid_changed = true;
        }
    }
    report
}

/// `/nix/store/<hash>-name.drv` → `<hash>`.
fn store_hash(drv_path: &str) -> Option<String> {
    let base = drv_path.rsplit('/').next()?;
    let (hash, _) = base.split_once('-')?;
    (!hash.is_empty()).then(|| hash.to_string())
}

/// First warning after ~30s of misses, then once a minute. The first
/// few polls legitimately miss (the dataset drv builds first; the
/// bench build may not be dispatched yet) — past that, silence would
/// hide a broken matcher, which is exactly how run 4 lost its
/// attribution.
fn warn_now(unmatched_polls: u32) -> bool {
    unmatched_polls == 6 || unmatched_polls.is_multiple_of(12)
}

/// One `rio-cli workers --actor --json` poll → `(executor_id,
/// running_build)` per actor-map entry.
///
/// `--actor` (DebugListExecutors) is load-bearing: plain `workers`
/// reads the PG view (`ExecutorInfo`), which has NO running_build
/// field — matching against it can never succeed. running_build only
/// exists on the actor view's `DebugExecutorState`.
async fn poll_workers(cli: &Arc<CliCtx>) -> Result<Vec<(String, Option<String>)>, String> {
    let cli = Arc::clone(cli);
    // CliCtx::run shells out synchronously — keep it off the async
    // worker threads.
    let out = tokio::task::spawn_blocking(move || cli.run(&["--json", "workers", "--actor"]))
        .await
        .map_err(|e| format!("workers poll task failed: {e}"))?
        .map_err(|e| format!("rio-cli workers --actor failed: {e:#}"))?;
    parse_workers(&out)
}

/// Parse the DebugListExecutorsResponse JSON (snake_case serde on the
/// prost types). Indexes via serde_json::Value (the i056 idiom) so
/// proto growth doesn't break the bench.
fn parse_workers(json: &str) -> Result<Vec<(String, Option<String>)>, String> {
    let resp: Value =
        serde_json::from_str(json).map_err(|e| format!("workers --actor JSON parse: {e}"))?;
    let executors = resp
        .get("executors")
        .and_then(Value::as_array)
        .ok_or_else(|| "workers --actor JSON has no executors array".to_string())?;
    Ok(executors
        .iter()
        .filter_map(|e| {
            let id = e.get("executor_id").and_then(Value::as_str)?;
            let running = e
                .get("running_build")
                .and_then(Value::as_str)
                .map(str::to_string);
            Some((id.to_string(), running))
        })
        .collect())
}

/// The executor whose running_build matches our drv hash. The
/// scheduler's DrvHash is the .drv BASENAME (`{hash}-{name}.drv`);
/// substring containment in either direction also covers full-path
/// and bare-hash forms.
fn match_executor(entries: &[(String, Option<String>)], drv_hash: &str) -> Option<String> {
    entries.iter().find_map(|(id, running)| {
        let r = running.as_deref()?;
        (r.contains(drv_hash) || drv_hash.contains(r)).then(|| id.clone())
    })
}

/// Executor pod → (nodeName, pod UID). The executor_id is the pod
/// name (rio-builder auto-detects it from hostname).
async fn resolve_node(client: &kube::Client, exec_id: &str) -> Result<(String, String)> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS_BUILDERS);
    let pod = pods.get(exec_id).await?;
    let node = pod
        .spec
        .as_ref()
        .and_then(|s| s.node_name.clone())
        .ok_or_else(|| anyhow::anyhow!("executor pod {exec_id} has no nodeName"))?;
    let uid = pod
        .metadata
        .uid
        .ok_or_else(|| anyhow::anyhow!("executor pod {exec_id} has no uid"))?;
    Ok((node, uid))
}

async fn pod_uid(client: &kube::Client, pod: &str) -> Result<Option<String>> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS_BUILDERS);
    Ok(pods.get_opt(pod).await?.and_then(|p| p.metadata.uid))
}

/// All mountd pod names in the builder namespace (cold-reps evicts the
/// cache on every one of them — a missed pod is a warm node that would
/// fail the per-rep honesty gate).
pub(super) async fn all_mountd_pods(client: &kube::Client) -> Result<Vec<String>> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS_BUILDERS);
    let list = pods
        .list(&kube::api::ListParams::default().labels(MOUNTD_LABEL))
        .await?;
    Ok(list
        .items
        .into_iter()
        .filter_map(|p| p.metadata.name)
        .collect())
}

async fn mountd_on_node(client: &kube::Client, node: &str) -> Option<String> {
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS_BUILDERS);
    let list = pods
        .list(&kube::api::ListParams::default().labels(MOUNTD_LABEL))
        .await
        .ok()?;
    list.items
        .into_iter()
        .find(|p| p.spec.as_ref().and_then(|s| s.node_name.as_deref()) == Some(node))
        .and_then(|p| p.metadata.name)
}

/// Instance type / capacity / kernel / AMI off the Node object — the
/// compare identity key's hardware half.
async fn describe_node(client: &kube::Client, node: &str, report: &mut CotenancyReport) {
    let nodes: Api<Node> = Api::all(client.clone());
    let Ok(Some(n)) = nodes.get_opt(node).await else {
        return;
    };
    if let Some(labels) = &n.metadata.labels {
        report.instance_type = labels.get("node.kubernetes.io/instance-type").cloned();
        report.capacity_type = labels.get("karpenter.sh/capacity-type").cloned();
        report.ami = labels.get("rio.build/ami").cloned();
    }
    report.node_kernel = n.status.and_then(|s| s.node_info).map(|i| i.kernel_version);
}

async fn sample_once(
    client: &kube::Client,
    mountd_pod: &str,
    executor_pod: Option<&str>,
) -> Result<Sample> {
    let body = scrape_pod(client, NS_BUILDERS, mountd_pod, MOUNTD_METRICS_PORT).await?;
    let s = Scrape::parse(&body);
    let mut sample = Sample {
        epoch_ms: jiff::Timestamp::now().as_millisecond() as u64,
        connections_current: s.sum("rio_mountd_connections_current"),
        mountd_promote_bytes: s.sum("rio_mountd_promote_bytes_total"),
        mountd_request_count: by_label(&s, "rio_mountd_request_seconds_count", "op"),
        mountd_request_sum: by_label(&s, "rio_mountd_request_seconds_sum", "op"),
        mountd_cache_evicted: by_label(&s, "rio_mountd_cache_evicted_bytes_total", "dir"),
        builder: None,
    };
    if let Some(pod) = executor_pod {
        match scrape_pod(client, NS_BUILDERS, pod, EXECUTOR_METRICS_PORT).await {
            Ok(body) => {
                let b = Scrape::parse(&body);
                sample.builder = Some(BuilderSample {
                    open_case: by_label(&b, "rio_builder_castore_fuse_open_case_total", "case"),
                    fetch_bytes: by_label(&b, "rio_builder_castore_fuse_fetch_bytes_total", "hit"),
                    open_seconds_count: b.sum("rio_builder_castore_fuse_open_seconds_count"),
                });
            }
            Err(e) => debug!("executor scrape failed: {e:#}"),
        }
    }
    Ok(sample)
}

/// Fold a metric's series into label-value → sum.
fn by_label(s: &Scrape, name: &str, key: &str) -> BTreeMap<String, f64> {
    let mut out = BTreeMap::new();
    for (labels, v) in s.series(name) {
        if let Some(val) = label_value(labels, key) {
            *out.entry(val).or_insert(0.0) += v;
        }
    }
    out
}

/// Extract `key="…"` from a raw prometheus label string. The match
/// must sit at a label boundary (`{` or `,` before it) so `op` never
/// matches inside a sibling key like `xop`.
fn label_value(labels: &str, key: &str) -> Option<String> {
    let pat = format!("{key}=\"");
    let mut from = 0;
    while let Some(rel) = labels[from..].find(&pat) {
        let i = from + rel;
        if matches!(labels[..i].chars().next_back(), Some('{' | ',')) {
            let rest = &labels[i + pat.len()..];
            return Some(rest[..rest.find('"')?].to_string());
        }
        from = i + 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Realistic `rio-cli workers --actor --json` shape: snake_case
    /// keys (serde derived straight on the prost types), one busy
    /// builder whose running_build is the nonce'd fsbench drv BASENAME
    /// (the scheduler's DrvHash form), one idle, one fetcher on an
    /// unrelated drv.
    const WORKERS_ACTOR_JSON: &str = r#"{
        "executors": [
            {
                "executor_id": "rio-builder-x86-64-bld-7f9c4",
                "has_stream": true,
                "is_registered": true,
                "warm": true,
                "kind": 1,
                "systems": ["x86_64-linux"],
                "last_heartbeat_ago_secs": 2,
                "draining": false,
                "store_degraded": false,
                "running_build": "q7d4m2k9p1aaaaaaaaaaaaaaaaaaaaaa-fsbench-run-rio-fsbench-w1-x1y2z3abc456.drv"
            },
            {
                "executor_id": "rio-builder-x86-64-bld-idle1",
                "has_stream": true,
                "is_registered": true,
                "warm": true,
                "kind": 1,
                "systems": ["x86_64-linux"],
                "last_heartbeat_ago_secs": 1,
                "draining": false,
                "store_degraded": false,
                "running_build": null
            },
            {
                "executor_id": "rio-fetcher-2",
                "has_stream": true,
                "is_registered": true,
                "warm": true,
                "kind": 2,
                "systems": ["x86_64-linux"],
                "last_heartbeat_ago_secs": 3,
                "draining": false,
                "store_degraded": false,
                "running_build": "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-curl-8.14.1.drv"
            }
        ],
        "leader_for_secs": 412
    }"#;

    #[test]
    fn parse_and_match_against_actor_view_shape() {
        let entries = parse_workers(WORKERS_ACTOR_JSON).unwrap();
        assert_eq!(entries.len(), 3);
        assert_eq!(entries[1].1, None, "idle worker has no running_build");

        // The matcher input is the bare hash extracted from the
        // locally pre-evaluated drv path (store_hash); running_build
        // is the basename — containment must bridge the two forms.
        let local_drv = "/nix/store/q7d4m2k9p1aaaaaaaaaaaaaaaaaaaaaa-fsbench-run-rio-fsbench-w1-x1y2z3abc456.drv";
        let hash = store_hash(local_drv).unwrap();
        assert_eq!(
            match_executor(&entries, &hash).as_deref(),
            Some("rio-builder-x86-64-bld-7f9c4")
        );
        // Full basename and full path also match (running_build form
        // may change with the proto — either direction works).
        assert_eq!(
            match_executor(&entries, local_drv.trim_start_matches("/nix/store/")).as_deref(),
            Some("rio-builder-x86-64-bld-7f9c4")
        );

        // A drv that nobody is building must not match anything (the
        // fetcher's unrelated drv must not false-positive).
        assert_eq!(match_executor(&entries, "nonexistenthash123"), None);
    }

    #[test]
    fn parse_workers_rejects_shapeless_json() {
        // The PG view (`workers` without --actor) has executors but no
        // running_build fields — it parses but every entry is None, so
        // the matcher can never latch. The loud-warning path exposes
        // exactly that: all-None running lists.
        let pg_view = r#"{"executors":[{"executor_id":"w1","status":"alive"}]}"#;
        let entries = parse_workers(pg_view).unwrap();
        assert_eq!(entries[0].1, None);
        assert_eq!(match_executor(&entries, "anything"), None);

        assert!(parse_workers("{}").is_err(), "missing executors array");
        assert!(parse_workers("not json").is_err());
    }

    #[test]
    fn warn_cadence_grace_then_periodic() {
        // ~30s of grace (dataset build precedes the bench build), then
        // once a minute — never silent forever.
        let warns: Vec<u32> = (1..=40).filter(|n| warn_now(*n)).collect();
        assert_eq!(warns, vec![6, 12, 24, 36]);
    }

    #[test]
    fn store_hash_extracts_base32() {
        assert_eq!(
            store_hash("/nix/store/abc123xyz-fsbench-run-s.drv").as_deref(),
            Some("abc123xyz")
        );
        assert_eq!(store_hash("no-slash-name.drv").as_deref(), Some("no"));
        assert_eq!(store_hash("/nix/store/"), None);
    }

    #[test]
    fn by_label_folds_series() {
        let s = Scrape::parse(
            "rio_mountd_request_seconds_count{op=\"mount\"} 3\n\
             rio_mountd_request_seconds_count{op=\"promote\"} 7\n\
             rio_mountd_request_seconds_count{op=\"mount\",code=\"ok\"} 2\n",
        );
        let m = by_label(&s, "rio_mountd_request_seconds_count", "op");
        // Same op across differing extra labels sums together.
        assert_eq!(m.get("mount"), Some(&5.0));
        assert_eq!(m.get("promote"), Some(&7.0));
    }

    #[test]
    fn label_value_handles_position_and_absence() {
        assert_eq!(
            label_value("{a=\"x\",op=\"mount\"}", "op").as_deref(),
            Some("mount")
        );
        assert_eq!(label_value("{a=\"x\"}", "op"), None);
        // A sibling key ending in `op` must not shadow the real one —
        // matches only count at label boundaries.
        assert_eq!(
            label_value("{xop=\"wrong\",op=\"right\"}", "op").as_deref(),
            Some("right")
        );
    }
}
