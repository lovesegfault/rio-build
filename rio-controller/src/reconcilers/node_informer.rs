//! Node-label cache for `hw_class` join at completion-ingest.
//!
//! ADR-023 §Hardware heterogeneity: builders are air-gapped from the
//! apiserver, so they report `spec.nodeName` (downward API) and the
//! controller joins to Node labels server-side when ingesting build
//! completion samples. `hw_class = "{manufacturer}-{generation}-
//! {storage}"` from Karpenter-provisioned + rio.build labels.
//!
//! Node-gone-at-ingest → [`NodeLabelCache::hw_class_of`] returns
//! `None` and the sample's `hw_class` stays NULL. This is expected
//! for builds that finish after their node has been reaped (rare —
//! ephemeral builders typically outlive their single build).
//!
//! # Why a watcher, not per-ingest `Api::get`
//!
//! Completion-ingest is on the hot path (one per build). A cached
//! lookup is O(1) in-process; a per-ingest `GET /api/v1/nodes/{name}`
//! is an apiserver round-trip per build. The watch is one long-lived
//! connection that incrementally updates the cache.
//!
//! # Why not `reflector::store()`
//!
//! The reflector store caches full `Node` objects (status, conditions,
//! images list — kilobytes each). We need three label strings. A
//! hand-rolled `HashMap<String, HwClass>` is the lightweight version.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{Patch, PatchParams};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use parking_lot::RwLock;
use rio_proto::AdminServiceClient;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

/// Pod annotation the [`run_pod_annotator`] watcher stamps with the
/// node's [`HwClass::as_string`]. Builder reads it via downward-API
/// (`RIO_HW_CLASS`) to key its `hw_perf_samples` insert.
pub const ANNOT_HW_CLASS: &str = "rio.build/hw-class";

/// Karpenter-set: `aws`, `intel`, `amd`. Unknown if label absent
/// (non-Karpenter node, or Karpenter < v0.33).
const LABEL_MANUFACTURER: &str = "karpenter.k8s.aws/instance-cpu-manufacturer";
/// Karpenter-set: instance generation number as string (`"7"` for
/// c7/m7/r7, etc.).
const LABEL_GENERATION: &str = "karpenter.k8s.aws/instance-generation";
/// rio-set via EC2NodeClass `spec.tags` → Karpenter propagates to
/// Node labels. `ebs` (gp3 root) or `nvme` (instance-store).
const LABEL_STORAGE: &str = "rio.build/storage";

/// `karpenter.sh/capacity-type` — `spot` or `on-demand`. Cached so the
/// spot-interrupt watcher can skip on-demand nodes without re-fetching.
const LABEL_CAPACITY_TYPE: &str = "karpenter.sh/capacity-type";

/// Three labels that determine build-performance equivalence class.
/// Formatted as `"{manufacturer}-{generation}-{storage}"` for the
/// completion-sample `hw_class` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwClass {
    pub manufacturer: String,
    pub generation: String,
    pub storage: String,
}

/// Per-node cache value: `HwClass` + the bits the spot-interrupt
/// watcher needs (capacity type, creation epoch for node-seconds).
#[derive(Debug, Clone)]
struct NodeMeta {
    hw: HwClass,
    capacity_type: Option<String>,
    /// Epoch of the last exposure flush for this node (initialised to
    /// `metadata.creationTimestamp` on first sight). [`NodeLabelCache::spot_exposure`]
    /// and [`NodeLabelCache::drain_live_spot_exposure`] return the
    /// delta `now - last_exposure_at`, so live nodes contribute to the
    /// λ denominator every flush instead of only on Delete.
    last_exposure_at: Option<f64>,
}

impl HwClass {
    /// `"{manufacturer}-{generation}-{storage}"`, e.g. `"aws-7-ebs"`.
    pub fn as_string(&self) -> String {
        format!("{}-{}-{}", self.manufacturer, self.generation, self.storage)
    }

    /// Extract from a Node's labels. Missing labels default to
    /// `"unknown"` (manufacturer/generation) or `"ebs"` (storage —
    /// the conservative default; nvme is opt-in via EC2NodeClass).
    fn from_node(node: &Node) -> Self {
        let labels = node.metadata.labels.as_ref();
        let get = |k: &str| labels.and_then(|l| l.get(k)).cloned();
        Self {
            manufacturer: get(LABEL_MANUFACTURER).unwrap_or_else(|| "unknown".into()),
            generation: get(LABEL_GENERATION).unwrap_or_else(|| "unknown".into()),
            storage: get(LABEL_STORAGE).unwrap_or_else(|| "ebs".into()),
        }
    }
}

/// Node-name → `HwClass` cache. Cheap to clone (`Arc`); share one
/// instance between the informer task and consumers.
#[derive(Clone, Default)]
pub struct NodeLabelCache(Arc<RwLock<HashMap<String, NodeMeta>>>);

impl NodeLabelCache {
    /// Formatted `hw_class` string for `node_name`, or `None` if the
    /// node is not (or no longer) in the cache.
    pub fn hw_class_of(&self, node_name: &str) -> Option<String> {
        self.0.read().get(node_name).map(|m| m.hw.as_string())
    }

    /// `Some((hw_class, node_seconds_since_last_flush))` for
    /// `node_name` IF it's a spot node with a creation timestamp.
    /// Feeds the exposure half of λ\[h\] — on-demand nodes contribute
    /// neither numerator nor denominator (their λ is 0 by definition).
    ///
    /// Returns the slice since the last
    /// [`Self::drain_live_spot_exposure`] (or since creation if never
    /// drained) so the Delete arm reports only the final residual,
    /// not the full lifetime — the periodic flush has already banked
    /// the rest.
    pub fn spot_exposure(&self, node_name: &str, now_epoch: f64) -> Option<(String, f64)> {
        let g = self.0.read();
        let m = g.get(node_name)?;
        if m.capacity_type.as_deref() != Some("spot") {
            return None;
        }
        let secs = (now_epoch - m.last_exposure_at?).max(0.0);
        Some((m.hw.as_string(), secs))
    }

    /// For every live spot node, sum `now - last_exposure_at` per
    /// `hw_class` and advance `last_exposure_at = now`. Called from
    /// [`run`]'s periodic flush so the λ denominator includes
    /// still-running (right-censored) nodes — without this, a single
    /// early interrupt in a fresh N-node burst inflates λ ≈N× until
    /// the survivors are eventually deleted.
    ///
    /// Controller restart re-seeds `last_exposure_at = created_at`
    /// (in-process state), so the first post-restart flush re-reports
    /// the pre-restart slice once. Accepted: restarts are rare and
    /// the 24h EMA halflife dampens the duplicate; the alternative
    /// (seed `= now`) would *under*-count on cold start, which is the
    /// bias direction this fix exists to eliminate.
    pub fn drain_live_spot_exposure(&self, now_epoch: f64) -> Vec<(String, f64)> {
        let mut by_hw: HashMap<String, f64> = HashMap::new();
        let mut g = self.0.write();
        for m in g.values_mut() {
            if m.capacity_type.as_deref() != Some("spot") {
                continue;
            }
            let Some(last) = m.last_exposure_at else {
                continue;
            };
            let secs = (now_epoch - last).max(0.0);
            m.last_exposure_at = Some(now_epoch);
            if secs > 0.0 {
                *by_hw.entry(m.hw.as_string()).or_default() += secs;
            }
        }
        by_hw.into_iter().collect()
    }

    /// Evict every cached entry whose name is NOT in `seen`, returning
    /// each evicted spot entry's final exposure slice. Called on
    /// `InitDone` after a watch-gap relist: the raw `watcher` does NOT
    /// synthesize `Delete` for nodes gone during the gap, so without
    /// this they linger and feed [`Self::drain_live_spot_exposure`]
    /// ~60 phantom node-seconds/min forever — biasing λ\[h\] low and
    /// `solve_full` toward spot. Mirrors the `Delete` arm's residual
    /// flush so the evicted node's last slice still lands in the
    /// denominator.
    pub fn prune_absent(&self, seen: &HashSet<String>, now_epoch: f64) -> Vec<(String, f64)> {
        let mut evicted = Vec::new();
        self.0.write().retain(|name, m| {
            if seen.contains(name) {
                return true;
            }
            if m.capacity_type.as_deref() == Some("spot")
                && let Some(last) = m.last_exposure_at
            {
                evicted.push((m.hw.as_string(), (now_epoch - last).max(0.0)));
            }
            false
        });
        evicted
    }

    /// Current cache size. For the `rio_controller_node_cache_size`
    /// gauge and tests.
    pub fn len(&self) -> usize {
        self.0.read().len()
    }

    /// `len() == 0`. Clippy `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.0.read().is_empty()
    }

    fn apply(&self, node: &Node) {
        let Some(name) = node.metadata.name.clone() else {
            return;
        };
        let labels = node.metadata.labels.as_ref();
        let created_at = node
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.as_second() as f64);
        let mut g = self.0.write();
        // Preserve last_exposure_at across re-apply (watch relist,
        // label-change Modify) so the periodic flush doesn't re-count
        // the already-banked slice. New entry → seed from created_at.
        let last_exposure_at = g.get(&name).and_then(|m| m.last_exposure_at).or(created_at);
        g.insert(
            name,
            NodeMeta {
                hw: HwClass::from_node(node),
                capacity_type: labels.and_then(|l| l.get(LABEL_CAPACITY_TYPE)).cloned(),
                last_exposure_at,
            },
        );
    }

    fn delete(&self, node: &Node) -> Option<NodeMeta> {
        node.metadata
            .name
            .as_ref()
            .and_then(|n| self.0.write().remove(n))
    }
}

/// Run the Node informer. Returns on `shutdown.cancelled()` or if
/// the watch stream ends (shouldn't — `default_backoff()` retries
/// indefinitely).
///
/// `spawn_monitored("node-informer", run(...))` from main.rs. Panics
/// are logged; the controller keeps reconciling. Consumers see an
/// empty/stale cache → `hw_class_of` returns `None` → samples ingest
/// with NULL `hw_class` (degraded, not broken).
pub async fn run(
    client: Client,
    cache: NodeLabelCache,
    mut admin: AdminServiceClient<Channel>,
    shutdown: rio_common::signal::Token,
) {
    let nodes: Api<Node> = Api::all(client);

    // Raw event stream (not `.applied_objects()`): we need Delete to
    // evict cache entries. `default_backoff()`: exponential retry on
    // watch connection loss (apiserver restart, network blip) — same
    // as disruption.rs.
    let mut stream = watcher(nodes, watcher::Config::default())
        .default_backoff()
        .boxed();

    // Live-exposure flush cadence. λ's denominator must include
    // censored (still-running) observations; flushing every minute
    // bounds the right-censoring bias to ≤60 node-seconds per node.
    let mut flush = tokio::time::interval(Duration::from_secs(EXPOSURE_FLUSH_SECS));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Names seen between Init and InitDone. `Some` only during a
    // relist; `None` during steady-state Apply/Delete.
    let mut relist_seen: Option<HashSet<String>> = None;

    info!("Node informer started");

    loop {
        let event = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                debug!("Node informer: shutdown");
                return;
            }
            _ = flush.tick() => {
                for (hw, secs) in cache.drain_live_spot_exposure(now_epoch()) {
                    report_exposure(&mut admin, hw, secs).await;
                }
                continue;
            }
            next = stream.next() => match next {
                Some(Ok(ev)) => ev,
                Some(Err(e)) => {
                    warn!(error = %e, "Node informer: stream error");
                    continue;
                }
                None => {
                    warn!("Node informer: stream ended (unexpected)");
                    return;
                }
            },
        };

        // Apply/InitApply → upsert; Delete → remove. Init/InitDone
        // bracket a relist: Init opens a seen-set, InitApply both
        // upserts AND records the name, InitDone prunes any cached
        // entry NOT re-seen. The raw watcher synthesizes no Delete
        // for nodes gone during a watch gap; before the periodic
        // drain_live_spot_exposure flush this was a harmless stale-
        // entry leak (passive lookup), but now a leaked spot entry
        // would feed phantom node-seconds into λ's denominator until
        // controller restart.
        match event {
            watcher::Event::Apply(node) => {
                cache.apply(&node);
            }
            watcher::Event::InitApply(node) => {
                if let Some(seen) = relist_seen.as_mut()
                    && let Some(name) = &node.metadata.name
                {
                    seen.insert(name.clone());
                }
                cache.apply(&node);
            }
            watcher::Event::Delete(node) => {
                // ADR-023 phase-13: final exposure slice → λ
                // denominator. Compute BEFORE delete (cache still has
                // last_exposure_at). Periodic flush above has already
                // banked the bulk of this node's lifetime.
                if let Some(name) = &node.metadata.name
                    && let Some((hw, secs)) = cache.spot_exposure(name, now_epoch())
                {
                    report_exposure(&mut admin, hw, secs).await;
                }
                cache.delete(&node);
            }
            watcher::Event::Init => {
                relist_seen = Some(HashSet::new());
            }
            watcher::Event::InitDone => {
                if let Some(seen) = relist_seen.take() {
                    for (hw, secs) in cache.prune_absent(&seen, now_epoch()) {
                        report_exposure(&mut admin, hw, secs).await;
                    }
                }
            }
        }
    }
}

/// Live spot-node exposure flush cadence (seconds). See
/// [`NodeLabelCache::drain_live_spot_exposure`].
const EXPOSURE_FLUSH_SECS: u64 = 60;

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Builder/fetcher pod label selector — same value `disruption.rs`
/// filters on. Re-declared here (not re-exported) to keep this module
/// dependency-free of `pool`.
const POOL_LABEL: &str = "rio.build/pool";

/// Pod-watcher: stamp `rio.build/hw-class` on each builder pod once
/// `spec.nodeName` resolves.
///
/// ADR-023 phase-10: builders are air-gapped from the apiserver, so
/// they can't read Node labels themselves. The builder reads the
/// stamped annotation via downward-API (`RIO_HW_CLASS`) to key its
/// `hw_perf_samples` microbench insert. Stamps once (skip if the
/// annotation already exists) — pod annotations are sticky and the
/// hw_class can't change for a scheduled pod.
///
/// `spawn_monitored("hw-class-annotator", run_pod_annotator(...))`
/// from main.rs. Same degraded-not-broken contract as [`run`]: if the
/// watcher dies, builders skip the bench (`RIO_HW_CLASS` empty) and
/// the hw_class stays at `factor=1.0` until ≥3 pods report.
///
/// TODO: gate the stamp on `EXISTS(SELECT 1 FROM hw_perf_factors WHERE
/// hw_class=$1)` so well-sampled classes skip the ~5s bench. Deferred:
/// rio-controller has NO PG access today (apiserver-only — every other
/// reconciler talks to the scheduler/store via gRPC, never PG
/// directly). Adding a PgPool here means: config plumbing
/// (`DATABASE_URL`), helm secret mount, IRSA policy, and a connection
/// the controller otherwise doesn't need — >50 LoC of plumbing for a
/// ~5s saving that already runs concurrent with the ~30s cold-start.
/// If this becomes worth it, route through a new
/// `SchedulerAdmin.HwClassSampled(hw_class) -> bool` RPC instead
/// (controller already holds that channel).
pub async fn run_pod_annotator(
    client: Client,
    cache: NodeLabelCache,
    shutdown: rio_common::signal::Token,
) {
    let pods: Api<Pod> = Api::all(client.clone());
    let cfg = watcher::Config::default().labels(POOL_LABEL);
    let mut stream = watcher(pods, cfg)
        .default_backoff()
        .applied_objects()
        .boxed();

    info!("hw-class pod annotator started (label={POOL_LABEL})");

    loop {
        let pod = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                debug!("hw-class annotator: shutdown");
                return;
            }
            next = stream.next() => match next {
                Some(Ok(p)) => p,
                Some(Err(e)) => { warn!(error = %e, "hw-class annotator: stream error"); continue; }
                None => { warn!("hw-class annotator: stream ended (unexpected)"); return; }
            },
        };
        let Some((name, ns, hw)) = hw_class_patch_target(&pod, &cache) else {
            continue;
        };
        let pods_ns: Api<Pod> = Api::namespaced(client.clone(), &ns);
        let patch = serde_json::json!({
            "metadata": { "annotations": { ANNOT_HW_CLASS: hw } }
        });
        if let Err(e) = pods_ns
            .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            warn!(%name, %ns, error = %e, "hw-class annotator: patch failed");
        }
    }
}

/// ADR-023 phase-13 λ\[h\] self-calibration: watch `core/v1.Event` for
/// `reason=SpotInterrupted` (Karpenter emits this on BOTH the NodeClaim
/// and the Node when AWS sends the 2-minute spot interruption notice;
/// we watch the Node event because [`NodeLabelCache`] is keyed by Node
/// name), resolve the referenced node's hw_class, and append
/// `interrupt_samples(hw_class, kind='interrupt', value=1)` via
/// `AdminService.AppendInterruptSample`.
///
/// The exposure half (`kind='exposure', value=node_seconds`) is
/// emitted from [`run`]: a periodic flush banks live node-seconds
/// every `EXPOSURE_FLUSH_SECS`, and the Node-DELETE arm appends
/// the final residual slice. Live nodes MUST contribute — counting
/// only completed lifetimes is the right-censoring bias that spikes
/// λ at burst onset.
///
/// `field_selector` narrows the watch server-side so we don't churn
/// on the cluster's full event firehose. Karpenter's interruption
/// controller emits the `SpotInterrupted` event on BOTH the NodeClaim
/// and the Node; we watch the **Node** event so `involvedObject.name`
/// is the Node `metadata.name` — the same key [`NodeLabelCache`] is
/// indexed by. The NodeClaim event's `involvedObject.name` is the
/// NodeClaim name (`{nodepool}-{hash}` on EKS), which would always
/// miss the IP-derived-Node-name-keyed cache.
pub async fn run_spot_interrupt_watcher(
    client: Client,
    cache: NodeLabelCache,
    mut admin: AdminServiceClient<Channel>,
    shutdown: rio_common::signal::Token,
) {
    let events: Api<Event> = Api::all(client);
    let cfg = watcher::Config::default().fields("reason=SpotInterrupted,involvedObject.kind=Node");
    let mut stream = watcher(events, cfg)
        .default_backoff()
        .applied_objects()
        .boxed();

    info!("spot-interrupt watcher started");

    loop {
        let ev = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            next = stream.next() => match next {
                Some(Ok(e)) => e,
                Some(Err(e)) => { warn!(error = %e, "spot-interrupt: stream error"); continue; }
                None => { warn!("spot-interrupt: stream ended (unexpected)"); return; }
            },
        };
        // involvedObject.kind=Node ⇒ involvedObject.name IS the Node
        // metadata.name — the cache key.
        let Some(node) = ev.involved_object.name else {
            continue;
        };
        let Some(hw_class) = cache.hw_class_of(&node) else {
            debug!(%node, "spot-interrupt: node not in cache; skipping");
            continue;
        };
        let r = admin
            .append_interrupt_sample(rio_proto::types::AppendInterruptSampleRequest {
                hw_class: hw_class.clone(),
                kind: "interrupt".into(),
                value: 1.0,
            })
            .await;
        match r {
            Ok(_) => debug!(%node, %hw_class, "spot-interrupt: sample appended"),
            Err(e) => warn!(%node, error = %e, "spot-interrupt: append failed"),
        }
    }
}

/// Append `kind='exposure'` node-seconds for one `hw_class`.
/// Best-effort: a failed RPC drops one denominator sample (λ reads
/// slightly high until the next flush lands).
async fn report_exposure(admin: &mut AdminServiceClient<Channel>, hw_class: String, secs: f64) {
    if let Err(e) = admin
        .append_interrupt_sample(rio_proto::types::AppendInterruptSampleRequest {
            hw_class,
            kind: "exposure".into(),
            value: secs,
        })
        .await
    {
        warn!(error = %e, "spot-exposure: append failed");
    }
}

/// `Some((pod_name, namespace, hw_class))` if `pod` should be stamped:
/// it has a `spec.nodeName`, that node is in `cache`, and the
/// annotation isn't already set. Pure for unit-testability.
fn hw_class_patch_target(pod: &Pod, cache: &NodeLabelCache) -> Option<(String, String, String)> {
    let already = pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(ANNOT_HW_CLASS));
    if already {
        return None;
    }
    let node = pod.spec.as_ref()?.node_name.as_deref()?;
    let hw = cache.hw_class_of(node)?;
    let name = pod.metadata.name.clone()?;
    let ns = pod.metadata.namespace.clone()?;
    Some((name, ns, hw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn node(name: &str, labels: &[(&str, &str)]) -> Node {
        let mut n = Node::default();
        n.metadata.name = Some(name.into());
        n.metadata.labels = Some(
            labels
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect::<BTreeMap<_, _>>(),
        );
        n
    }

    #[test]
    fn hw_class_of_returns_formatted() {
        let cache = NodeLabelCache::default();
        cache.apply(&node(
            "ip-10-0-1-5",
            &[
                (LABEL_MANUFACTURER, "aws"),
                (LABEL_GENERATION, "7"),
                (LABEL_STORAGE, "ebs"),
            ],
        ));
        assert_eq!(cache.hw_class_of("ip-10-0-1-5"), Some("aws-7-ebs".into()));
        assert_eq!(cache.hw_class_of("missing"), None);
    }

    #[test]
    fn hw_class_defaults_on_missing_labels() {
        let cache = NodeLabelCache::default();
        // No labels at all → all defaults.
        cache.apply(&node("bare", &[]));
        assert_eq!(
            cache.hw_class_of("bare"),
            Some("unknown-unknown-ebs".into())
        );
        // Partial: only manufacturer set.
        cache.apply(&node("partial", &[(LABEL_MANUFACTURER, "intel")]));
        assert_eq!(
            cache.hw_class_of("partial"),
            Some("intel-unknown-ebs".into())
        );
    }

    #[test]
    fn delete_evicts_entry() {
        let cache = NodeLabelCache::default();
        let n = node("ip-10-0-1-5", &[(LABEL_STORAGE, "nvme")]);
        cache.apply(&n);
        assert_eq!(cache.len(), 1);
        cache.delete(&n);
        assert_eq!(cache.hw_class_of("ip-10-0-1-5"), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn spot_exposure_gates_on_capacity_type() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::Timestamp;
        let cache = NodeLabelCache::default();
        let mut spot = node(
            "spot-n",
            &[(LABEL_CAPACITY_TYPE, "spot"), (LABEL_GENERATION, "7")],
        );
        spot.metadata.creation_timestamp = Some(Time(Timestamp::from_second(1000).unwrap()));
        cache.apply(&spot);
        let mut od = node("od-n", &[(LABEL_CAPACITY_TYPE, "on-demand")]);
        od.metadata.creation_timestamp = Some(Time(Timestamp::from_second(1000).unwrap()));
        cache.apply(&od);
        // Spot node → exposure with hw_class + uptime.
        let (hw, secs) = cache.spot_exposure("spot-n", 1100.0).unwrap();
        assert_eq!(hw, "unknown-7-ebs");
        assert!((secs - 100.0).abs() < 1e-6);
        // On-demand → no exposure (λ=0 by definition).
        assert!(cache.spot_exposure("od-n", 1100.0).is_none());
        // Unknown node → None.
        assert!(cache.spot_exposure("missing", 1100.0).is_none());
    }

    fn spot_node(name: &str, gen_: &str, created: i64) -> Node {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::Timestamp;
        let mut n = node(
            name,
            &[(LABEL_CAPACITY_TYPE, "spot"), (LABEL_GENERATION, gen_)],
        );
        n.metadata.creation_timestamp = Some(Time(Timestamp::from_second(created).unwrap()));
        n
    }

    /// Right-censoring fix: live spot nodes contribute exposure on
    /// every periodic drain, not only on Delete. Drain returns the
    /// per-hw_class delta since last drain and advances the cursor;
    /// Delete-arm `spot_exposure` then sees only the residual.
    #[test]
    fn drain_live_spot_exposure_banks_incremental_deltas() {
        let cache = NodeLabelCache::default();
        cache.apply(&spot_node("a", "7", 1000));
        cache.apply(&spot_node("b", "7", 1000));
        cache.apply(&spot_node("c", "6", 1020));
        // On-demand node: must NOT appear in drain output.
        let mut od = node("od", &[(LABEL_CAPACITY_TYPE, "on-demand")]);
        od.metadata.creation_timestamp = spot_node("x", "7", 1000).metadata.creation_timestamp;
        cache.apply(&od);

        // First drain at t=1060: a+b → 2×60=120s for gen-7, c → 40s.
        let mut d = cache.drain_live_spot_exposure(1060.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            d,
            vec![
                ("unknown-6-ebs".into(), 40.0),
                ("unknown-7-ebs".into(), 120.0),
            ]
        );

        // Second drain at t=1120: deltas only (60s each), not
        // cumulative-from-created.
        let mut d = cache.drain_live_spot_exposure(1120.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            d,
            vec![
                ("unknown-6-ebs".into(), 60.0),
                ("unknown-7-ebs".into(), 120.0),
            ]
        );

        // Re-apply (watch relist / label Modify) MUST preserve
        // last_exposure_at — no double-count of the banked slice.
        cache.apply(&spot_node("a", "7", 1000));
        let (_, secs) = cache.spot_exposure("a", 1150.0).unwrap();
        assert!((secs - 30.0).abs() < 1e-6, "residual since last drain");

        // Delete-arm sees only the residual since last drain, not
        // full lifetime (periodic flush already banked 1000..1120).
        let (_, secs) = cache.spot_exposure("c", 1150.0).unwrap();
        assert!((secs - 30.0).abs() < 1e-6);
    }

    /// Watch-gap relist: a spot node deleted during the gap gets no
    /// Delete event. `prune_absent` (called on InitDone) must evict it
    /// — returning its final exposure slice — so it stops feeding
    /// `drain_live_spot_exposure` phantom node-seconds.
    #[test]
    fn prune_absent_evicts_nodes_missing_from_relist() {
        let cache = NodeLabelCache::default();
        cache.apply(&spot_node("a", "7", 1000));
        cache.apply(&spot_node("b", "7", 1000));
        cache.apply(&spot_node("c", "6", 1020));
        // On-demand: evicted but contributes no exposure slice.
        let mut od = node("od", &[(LABEL_CAPACITY_TYPE, "on-demand")]);
        od.metadata.creation_timestamp = spot_node("x", "7", 1000).metadata.creation_timestamp;
        cache.apply(&od);

        // Periodic flush at t=1060 advances all cursors to 1060.
        let _ = cache.drain_live_spot_exposure(1060.0);

        // Relist at t=1090 saw only {a, c}. b and od deleted during
        // the gap → prune. b's residual (1060..1090) is returned;
        // od is not spot → no exposure slice.
        let seen: HashSet<String> = ["a", "c"].into_iter().map(String::from).collect();
        let evicted = cache.prune_absent(&seen, 1090.0);
        assert_eq!(evicted, vec![("unknown-7-ebs".into(), 30.0)]);
        assert_eq!(cache.len(), 2);
        assert!(cache.hw_class_of("b").is_none());
        assert!(cache.hw_class_of("od").is_none());

        // Next periodic drain at t=1120: only survivors contribute —
        // no phantom 60s from b.
        let mut d = cache.drain_live_spot_exposure(1120.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(
            d,
            vec![
                ("unknown-6-ebs".into(), 60.0),
                ("unknown-7-ebs".into(), 60.0),
            ]
        );
    }

    fn pod(name: &str, ns: &str, node: Option<&str>, annots: &[(&str, &str)]) -> Pod {
        let mut p = Pod::default();
        p.metadata.name = Some(name.into());
        p.metadata.namespace = Some(ns.into());
        if !annots.is_empty() {
            p.metadata.annotations = Some(
                annots
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect(),
            );
        }
        if let Some(n) = node {
            p.spec = Some(k8s_openapi::api::core::v1::PodSpec {
                node_name: Some(n.into()),
                ..Default::default()
            });
        }
        p
    }

    #[test]
    fn patch_target_stamps_once() {
        let cache = NodeLabelCache::default();
        cache.apply(&node("ip-10-0-1-5", &[(LABEL_STORAGE, "nvme")]));
        // Scheduled, not yet annotated → stamp.
        let p = pod("rb-abc", "rio", Some("ip-10-0-1-5"), &[]);
        assert_eq!(
            hw_class_patch_target(&p, &cache),
            Some(("rb-abc".into(), "rio".into(), "unknown-unknown-nvme".into()))
        );
        // Already annotated → skip (sticky).
        let p = pod(
            "rb-abc",
            "rio",
            Some("ip-10-0-1-5"),
            &[(ANNOT_HW_CLASS, "unknown-unknown-nvme")],
        );
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Pending (no nodeName yet) → skip.
        let p = pod("rb-pending", "rio", None, &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Node not in cache (informer race / non-Karpenter) → skip.
        let p = pod("rb-unk", "rio", Some("ip-10-0-9-9"), &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
    }

    #[test]
    fn apply_upserts_on_label_change() {
        let cache = NodeLabelCache::default();
        cache.apply(&node("n", &[(LABEL_STORAGE, "ebs")]));
        assert_eq!(cache.hw_class_of("n"), Some("unknown-unknown-ebs".into()));
        // Relabel (e.g., operator manually patched) → Modify event →
        // apply() again → cache reflects new value.
        cache.apply(&node("n", &[(LABEL_STORAGE, "nvme")]));
        assert_eq!(cache.hw_class_of("n"), Some("unknown-unknown-nvme".into()));
        assert_eq!(cache.len(), 1, "upsert, not duplicate");
    }
}
