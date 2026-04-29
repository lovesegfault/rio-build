//! Node-label cache for `hw_class` join at completion-ingest.
//!
//! ADR-023 §Hardware heterogeneity: builders are air-gapped from the
//! apiserver, so they report `spec.nodeName` (downward API) and the
//! controller joins to Node labels server-side when ingesting build
//! completion samples. `hw_class` is the operator's
//! `[sla.hw_classes.$h]` key whose label conjunction matches the
//! Node — fetched once via [`HwClassConfig::load`] (`GetHwClassConfig`
//! RPC) so the controller stamps the SAME `$h` string the scheduler's
//! `solve_intent_for` keys on, not a hardcoded 4-label reconstruction
//! that breaks the moment an operator's label schema differs (bug_061).
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
//! images list — kilobytes each). We need the labels map only. A
//! hand-rolled `HashMap<String, NodeMeta>` is the lightweight version.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{Patch, PatchParams};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use parking_lot::RwLock;
use tracing::{debug, info, warn};

use crate::reconcilers::{AdminClient, admin_call};

/// Pod annotation the [`run_pod_annotator`] watcher stamps with the
/// node's matched `hw_class` (the operator's `[sla.hw_classes.$h]`
/// key). Builder reads it via downward-API (`RIO_HW_CLASS`) to key its
/// `hw_perf_samples` insert.
pub const ANNOT_HW_CLASS: &str = "rio.build/hw-class";

/// `karpenter.sh/capacity-type` — `spot` or `on-demand`. Read from the
/// cached labels so the spot-interrupt watcher can skip on-demand nodes
/// without re-fetching.
const LABEL_CAPACITY_TYPE: &str = "karpenter.sh/capacity-type";

/// Operator-configured hw-class definitions, fetched once via
/// `GetHwClassConfig`. Shared (`Arc`) between [`NodeLabelCache`] and
/// the [`load`](Self::load) task. The match is lazy
/// ([`NodeLabelCache::hw_class_of`] etc. call [`Self::match_node`] on
/// each lookup) so config arriving AFTER nodes are cached still
/// resolves correctly; before load completes `hw_class = None`
/// everywhere — annotator skips, λ-samples skip.
///
/// Stored as a sorted `Vec` so [`Self::match_node`] is deterministic
/// when a Node satisfies two overlapping conjunctions (lexicographic
/// `$h` wins).
#[derive(Clone, Default)]
pub struct HwClassConfig(Arc<RwLock<Vec<HwClassDef>>>);

/// One `[sla.hw_classes.$h]` entry: the `$h` name + its ANDed label
/// conjunction.
type HwClassDef = (String, Vec<(String, String)>);

impl HwClassConfig {
    /// First `$h` (lexicographic) whose every `(k, v)` is satisfied by
    /// `labels`. `None` if no conjunction matches OR config is empty
    /// (not yet loaded).
    pub fn match_node(&self, labels: &BTreeMap<String, String>) -> Option<String> {
        let cfg = self.0.read();
        for (h, conj) in cfg.iter() {
            if conj
                .iter()
                .all(|(k, v)| labels.get(k).is_some_and(|nv| nv == v))
            {
                // rio-store rejects `!is_hw_class_name(hw_class)`. The
                // operator's `$h` is the value written to
                // `hw_perf_samples.hw_class`; a config change that
                // violates the charset would silently break the
                // builder's AppendHwPerfSample. `SlaConfig::validate`
                // is the load-bearing check (bug_038); this is a
                // belt-and-suspenders fail-fast in tests.
                debug_assert!(
                    rio_common::limits::is_hw_class_name(h),
                    "hw_class {h:?} fails is_hw_class_name"
                );
                return Some(h.clone());
            }
        }
        None
    }

    /// `[sla.hw_classes.$h].labels` for `h` — the conjunction the
    /// scheduler's `cells_to_selector_terms` would emit. `None` if `h`
    /// is unknown OR config not yet loaded. The §13b `cover_deficit`
    /// reads this to build NodeClaim `spec.requirements`.
    pub fn labels_for(&self, h: &str) -> Option<Vec<(String, String)>> {
        let cfg = self.0.read();
        cfg.iter()
            .find(|(name, _)| name == h)
            .map(|(_, conj)| conj.clone())
    }

    /// Replace the config wholesale from a `GetHwClassConfigResponse`.
    /// Sorted by `$h` for deterministic [`Self::match_node`] on overlap.
    fn set(&self, hw_classes: HashMap<String, rio_proto::types::HwClassLabels>) {
        let mut v: Vec<_> = hw_classes
            .into_iter()
            .map(|(h, def)| {
                let conj = def.labels.into_iter().map(|l| (l.key, l.value)).collect();
                (h, conj)
            })
            .collect();
        v.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        *self.0.write() = v;
    }

    /// Fetch `GetHwClassConfig` with bounded backoff (5 attempts, 1→16s).
    /// Returns once populated OR after the final attempt fails — callers
    /// already hold a balanced channel from `connect_forever`, so the
    /// scheduler is reachable; failures here are leader-election /
    /// service-gate transients. Unpopulated → [`Self::match_node`]
    /// returns `None` everywhere (degraded, not broken: annotator skips,
    /// λ-samples skip, `hw-bench-needed` keys on `intent.hw_class_names`
    /// which the scheduler populates independently).
    pub async fn load(&self, admin: &mut AdminClient) {
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=5 {
            match admin_call(admin.get_hw_class_config(())).await {
                Ok(r) => {
                    let hw_classes = r.into_inner().hw_classes;
                    info!(n = hw_classes.len(), "GetHwClassConfig loaded");
                    self.set(hw_classes);
                    return;
                }
                Err(e) => {
                    warn!(attempt, error = %e, "GetHwClassConfig failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(16));
                }
            }
        }
        warn!(
            "GetHwClassConfig: gave up after 5 attempts; hw_class will \
             stay None until controller restart (annotator/λ degraded)"
        );
    }

    /// Test-only constructor from `(h, [(k, v), …])` literals.
    #[cfg(test)]
    pub fn from_literals(defs: &[(&str, &[(&str, &str)])]) -> Self {
        let mut v: Vec<_> = defs
            .iter()
            .map(|(h, conj)| {
                let c = conj
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect();
                ((*h).to_string(), c)
            })
            .collect();
        v.sort_unstable_by(|a, b| a.0.cmp(&b.0));
        Self(Arc::new(RwLock::new(v)))
    }
}

/// Per-node cache value: full label map (matched lazily against
/// [`HwClassConfig`]) + the spot-exposure cursor.
#[derive(Debug, Clone)]
struct NodeMeta {
    /// All Node labels. [`HwClassConfig::match_node`] reads these on
    /// each lookup — config arriving after the node was cached still
    /// resolves. `capacity_type` is `labels.get(LABEL_CAPACITY_TYPE)`.
    labels: BTreeMap<String, String>,
    /// Epoch of the last exposure flush for this node (initialised to
    /// `metadata.creationTimestamp` on first sight). [`NodeLabelCache::spot_exposure`]
    /// and [`NodeLabelCache::drain_live_spot_exposure`] return the
    /// delta `now - last_exposure_at`, so live nodes contribute to the
    /// λ denominator every flush instead of only on Delete.
    last_exposure_at: Option<f64>,
}

impl NodeMeta {
    fn is_spot(&self) -> bool {
        self.labels.get(LABEL_CAPACITY_TYPE).map(String::as_str) == Some("spot")
    }
}

/// Node-name → labels cache + the [`HwClassConfig`] those labels are
/// matched against. Cheap to clone (`Arc` × 2); share one instance
/// between the informer task and consumers.
#[derive(Clone, Default)]
pub struct NodeLabelCache {
    nodes: Arc<RwLock<HashMap<String, NodeMeta>>>,
    config: HwClassConfig,
}

impl NodeLabelCache {
    /// Construct with a pre-populated [`HwClassConfig`]. `main.rs`
    /// loads the config once (retry-backoff) before spawning the
    /// informer/annotator tasks so the first watch relist already
    /// resolves correctly; lazy match means a late-loading config
    /// would also work, but loading-first avoids a window of
    /// `hw_class = None` λ-samples on cold start.
    pub fn with_config(config: HwClassConfig) -> Self {
        Self {
            nodes: Default::default(),
            config,
        }
    }

    /// Operator's `[sla.hw_classes.$h]` key matching `node_name`'s
    /// labels, or `None` if the node is not (or no longer) cached, no
    /// `$h` matches, or config is not yet loaded.
    pub fn hw_class_of(&self, node_name: &str) -> Option<String> {
        let g = self.nodes.read();
        self.config.match_node(&g.get(node_name)?.labels)
    }

    /// `Some((hw_class, node_seconds_since_last_flush))` for
    /// `node_name` IF it's a spot node with a creation timestamp AND
    /// matches a configured `$h`. Feeds the exposure half of λ\[h\] —
    /// on-demand nodes contribute neither numerator nor denominator
    /// (their λ is 0 by definition); unmatched nodes (non-builder
    /// NodePool) have no `$h` to key on.
    ///
    /// Returns the slice since the last
    /// [`Self::drain_live_spot_exposure`] (or since creation if never
    /// drained) so the Delete arm reports only the final residual,
    /// not the full lifetime — the periodic flush has already banked
    /// the rest.
    pub fn spot_exposure(&self, node_name: &str, now_epoch: f64) -> Option<(String, f64)> {
        let g = self.nodes.read();
        let m = g.get(node_name)?;
        if !m.is_spot() {
            return None;
        }
        let hw = self.config.match_node(&m.labels)?;
        let secs = (now_epoch - m.last_exposure_at?).max(0.0);
        Some((hw, secs))
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
        let mut g = self.nodes.write();
        for m in g.values_mut() {
            if !m.is_spot() {
                continue;
            }
            let Some(last) = m.last_exposure_at else {
                continue;
            };
            let secs = (now_epoch - last).max(0.0);
            m.last_exposure_at = Some(now_epoch);
            if secs > 0.0
                && let Some(hw) = self.config.match_node(&m.labels)
            {
                *by_hw.entry(hw).or_default() += secs;
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
        // Aggregate by hw_class (mirrors `drain_live_spot_exposure`):
        // the caller awaits one `report_exposure` RPC per returned
        // entry inside a single `select!` arm, so per-NODE tuples turn
        // a 100-node spot-storm relist into 100 sequential awaits ×
        // ADMIN_RPC_TIMEOUT against a degraded scheduler — ~500s of
        // watch-loop starvation. The RPC payload carries no per-node
        // identity (only hw_class/kind/value, event_uid: None) and
        // exposure sums into λ's denominator, so aggregation is
        // lossless (bug_057).
        let mut by_hw: HashMap<String, f64> = HashMap::new();
        let config = &self.config;
        self.nodes.write().retain(|name, m| {
            if seen.contains(name) {
                return true;
            }
            if m.is_spot()
                && let Some(last) = m.last_exposure_at
                && let Some(hw) = config.match_node(&m.labels)
            {
                *by_hw.entry(hw).or_default() += (now_epoch - last).max(0.0);
            }
            false
        });
        by_hw.into_iter().collect()
    }

    /// Current cache size. For the `rio_controller_node_cache_size`
    /// gauge and tests.
    pub fn len(&self) -> usize {
        self.nodes.read().len()
    }

    /// `len() == 0`. Clippy `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.nodes.read().is_empty()
    }

    fn apply(&self, node: &Node) {
        let Some(name) = node.metadata.name.clone() else {
            return;
        };
        let created_at = node
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.as_second() as f64);
        let mut g = self.nodes.write();
        // Preserve last_exposure_at across re-apply (watch relist,
        // label-change Modify) so the periodic flush doesn't re-count
        // the already-banked slice. New entry → seed from created_at.
        let last_exposure_at = g.get(&name).and_then(|m| m.last_exposure_at).or(created_at);
        g.insert(
            name,
            NodeMeta {
                labels: node.metadata.labels.clone().unwrap_or_default(),
                last_exposure_at,
            },
        );
    }

    fn delete(&self, node: &Node) -> Option<NodeMeta> {
        node.metadata
            .name
            .as_ref()
            .and_then(|n| self.nodes.write().remove(n))
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
    mut admin: AdminClient,
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
/// watcher dies, builders' downward-API volume stays empty,
/// `hw_class::resolve` returns `None` after its 30s bound, and the
/// hw_class stays at `factor=1.0` until ≥3 pods report. The volume
/// (NOT env-var) form means a late stamp still reaches a running pod.
///
/// TODO: gate the stamp on `EXISTS(SELECT 1 FROM hw_perf_samples WHERE
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
    mut admin: AdminClient,
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
        // `event_uid` makes the INSERT idempotent: `.applied_objects()`
        // re-yields every still-extant Event on relist (controller
        // restart, apiserver restart, watch reconnect). Without dedup
        // each relist double-counts into λ's numerator → `solve_full`
        // biases away from spot.
        let r = admin_call(admin.append_interrupt_sample(
            rio_proto::types::AppendInterruptSampleRequest {
                hw_class: hw_class.clone(),
                kind: "interrupt".into(),
                value: 1.0,
                event_uid: ev.metadata.uid.clone(),
            },
        ))
        .await;
        match r {
            Ok(_) => debug!(%node, %hw_class, "spot-interrupt: sample appended"),
            Err(e) => warn!(%node, error = %e, "spot-interrupt: append failed"),
        }
    }
}

/// Append `kind='exposure'` node-seconds for one `hw_class`.
/// Best-effort: a failed/timed-out RPC drops one denominator sample
/// (λ reads slightly high until the next flush lands). Bounded by
/// [`admin_call`]'s timeout so a hung scheduler can't wedge the Node-
/// informer's watch loop (every caller is inside that loop).
async fn report_exposure(admin: &mut AdminClient, hw_class: String, secs: f64) {
    if let Err(e) = admin_call(admin.append_interrupt_sample(
        rio_proto::types::AppendInterruptSampleRequest {
            hw_class,
            kind: "exposure".into(),
            value: secs,
            // Timer-driven, no K8s Event → NULL uid (unconstrained by
            // the M_047 partial unique index).
            event_uid: None,
        },
    ))
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

    /// Two-class config keyed on `rio.build/hw-band` only — covers the
    /// "operator's label schema is arbitrary" case (bug_061: a single
    /// non-Karpenter label, NOT the hardcoded 4-tuple).
    fn band_config() -> HwClassConfig {
        HwClassConfig::from_literals(&[
            ("intel-7", &[("rio.build/hw-band", "7")]),
            ("intel-6", &[("rio.build/hw-band", "6")]),
        ])
    }

    fn band_cache() -> NodeLabelCache {
        NodeLabelCache::with_config(band_config())
    }

    #[test]
    fn labels_for_returns_conjunction() {
        let cfg = band_config();
        assert_eq!(
            cfg.labels_for("intel-7"),
            Some(vec![("rio.build/hw-band".into(), "7".into())])
        );
        assert_eq!(cfg.labels_for("nope"), None);
        // Empty (not yet loaded) → None for any h.
        assert_eq!(HwClassConfig::default().labels_for("intel-7"), None);
    }

    #[test]
    fn hw_class_of_matches_operator_config() {
        let cache = band_cache();
        cache.apply(&node(
            "ip-10-0-1-5",
            &[("rio.build/hw-band", "7"), ("unrelated", "x")],
        ));
        assert_eq!(cache.hw_class_of("ip-10-0-1-5"), Some("intel-7".into()));
        assert_eq!(cache.hw_class_of("missing"), None);
    }

    /// bug_061 contract: the matched value IS the operator's `$h` key,
    /// not a controller-side `"{mfg}-{gen}-{storage}-{band}"`
    /// reconstruction. A node satisfying a multi-label conjunction
    /// returns the `$h` string as-is; a node missing one label of the
    /// conjunction returns `None` (no `"unknown"` fill-in).
    #[test]
    fn hw_class_is_operator_key_not_reconstruction() {
        let cfg = HwClassConfig::from_literals(&[(
            "amd-nvme-mid",
            &[
                ("karpenter.k8s.aws/instance-cpu-manufacturer", "amd"),
                ("rio.build/storage", "nvme"),
            ],
        )]);
        let cache = NodeLabelCache::with_config(cfg);
        cache.apply(&node(
            "full",
            &[
                ("karpenter.k8s.aws/instance-cpu-manufacturer", "amd"),
                ("rio.build/storage", "nvme"),
                ("karpenter.k8s.aws/instance-generation", "6"),
            ],
        ));
        assert_eq!(cache.hw_class_of("full"), Some("amd-nvme-mid".into()));
        // Partial match (one label of the conjunction missing) → None.
        cache.apply(&node(
            "partial",
            &[("karpenter.k8s.aws/instance-cpu-manufacturer", "amd")],
        ));
        assert_eq!(cache.hw_class_of("partial"), None);
    }

    /// Unloaded config → `hw_class_of` returns `None` (annotator
    /// skips, λ-samples skip — degraded, not broken). Late config load
    /// resolves already-cached nodes (lazy match).
    #[test]
    fn hw_class_none_until_config_loaded_then_lazy_match() {
        let cache = NodeLabelCache::default();
        cache.apply(&node("n", &[("rio.build/hw-band", "7")]));
        assert_eq!(cache.hw_class_of("n"), None, "config empty → no match");
        // Config arrives: already-cached node now resolves.
        cache.config.set(
            [(
                "intel-7".into(),
                rio_proto::types::HwClassLabels {
                    labels: vec![rio_proto::types::NodeLabelMatch {
                        key: "rio.build/hw-band".into(),
                        value: "7".into(),
                    }],
                },
            )]
            .into(),
        );
        assert_eq!(cache.hw_class_of("n"), Some("intel-7".into()));
    }

    /// Overlapping conjunctions → deterministic (lexicographic `$h`).
    #[test]
    fn match_node_deterministic_on_overlap() {
        let cfg = HwClassConfig::from_literals(&[("zz", &[("k", "v")]), ("aa", &[("k", "v")])]);
        let labels: BTreeMap<_, _> = [("k".into(), "v".into())].into();
        assert_eq!(cfg.match_node(&labels), Some("aa".into()));
    }

    #[test]
    fn delete_evicts_entry() {
        let cache = band_cache();
        let n = node("ip-10-0-1-5", &[("rio.build/hw-band", "7")]);
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
        let cache = band_cache();
        let mut spot = node(
            "spot-n",
            &[(LABEL_CAPACITY_TYPE, "spot"), ("rio.build/hw-band", "7")],
        );
        spot.metadata.creation_timestamp = Some(Time(Timestamp::from_second(1000).unwrap()));
        cache.apply(&spot);
        let mut od = node(
            "od-n",
            &[
                (LABEL_CAPACITY_TYPE, "on-demand"),
                ("rio.build/hw-band", "7"),
            ],
        );
        od.metadata.creation_timestamp = Some(Time(Timestamp::from_second(1000).unwrap()));
        cache.apply(&od);
        // Spot node → exposure with hw_class + uptime.
        let (hw, secs) = cache.spot_exposure("spot-n", 1100.0).unwrap();
        assert_eq!(hw, "intel-7");
        assert!((secs - 100.0).abs() < 1e-6);
        // On-demand → no exposure (λ=0 by definition).
        assert!(cache.spot_exposure("od-n", 1100.0).is_none());
        // Unknown node → None.
        assert!(cache.spot_exposure("missing", 1100.0).is_none());
        // Spot but no matching $h (config empty) → None (no key for λ).
        let bare = NodeLabelCache::default();
        bare.apply(&spot);
        assert!(bare.spot_exposure("spot-n", 1100.0).is_none());
    }

    fn spot_node(name: &str, band: &str, created: i64) -> Node {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::Timestamp;
        let mut n = node(
            name,
            &[(LABEL_CAPACITY_TYPE, "spot"), ("rio.build/hw-band", band)],
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
        let cache = band_cache();
        cache.apply(&spot_node("a", "7", 1000));
        cache.apply(&spot_node("b", "7", 1000));
        cache.apply(&spot_node("c", "6", 1020));
        // On-demand node: must NOT appear in drain output.
        let mut od = node("od", &[(LABEL_CAPACITY_TYPE, "on-demand")]);
        od.metadata.creation_timestamp = spot_node("x", "7", 1000).metadata.creation_timestamp;
        cache.apply(&od);

        // First drain at t=1060: a+b → 2×60=120s for intel-7, c → 40s.
        let mut d = cache.drain_live_spot_exposure(1060.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(d, vec![("intel-6".into(), 40.0), ("intel-7".into(), 120.0)]);

        // Second drain at t=1120: deltas only (60s each), not
        // cumulative-from-created.
        let mut d = cache.drain_live_spot_exposure(1120.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(d, vec![("intel-6".into(), 60.0), ("intel-7".into(), 120.0)]);

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
        let cache = band_cache();
        cache.apply(&spot_node("a", "7", 1000));
        cache.apply(&spot_node("b", "7", 1000));
        cache.apply(&spot_node("b2", "7", 1000));
        cache.apply(&spot_node("c", "6", 1020));
        // On-demand: evicted but contributes no exposure slice.
        let mut od = node("od", &[(LABEL_CAPACITY_TYPE, "on-demand")]);
        od.metadata.creation_timestamp = spot_node("x", "7", 1000).metadata.creation_timestamp;
        cache.apply(&od);

        // Periodic flush at t=1060 advances all cursors to 1060.
        let _ = cache.drain_live_spot_exposure(1060.0);

        // Relist at t=1090 saw only {a, c}. b, b2, od deleted during
        // the gap → prune. b+b2 residuals (1060..1090 each) are
        // returned AGGREGATED by hw_class (bug_057: per-node tuples
        // would emit 2 entries × 30.0, defeating the watch-loop
        // bound); od is not spot → no exposure slice.
        let seen: HashSet<String> = ["a", "c"].into_iter().map(String::from).collect();
        let evicted = cache.prune_absent(&seen, 1090.0);
        assert_eq!(
            evicted,
            vec![("intel-7".into(), 60.0)],
            "two same-hw_class spot nodes → ONE aggregated entry (Σsecs)"
        );
        assert_eq!(cache.len(), 2);
        assert!(cache.hw_class_of("b").is_none());
        assert!(cache.hw_class_of("b2").is_none());
        assert!(cache.hw_class_of("od").is_none());

        // Next periodic drain at t=1120: only survivors contribute —
        // no phantom 60s from b/b2.
        let mut d = cache.drain_live_spot_exposure(1120.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(d, vec![("intel-6".into(), 60.0), ("intel-7".into(), 60.0)]);
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
        let cache = band_cache();
        cache.apply(&node("ip-10-0-1-5", &[("rio.build/hw-band", "7")]));
        // Scheduled, not yet annotated → stamp.
        let p = pod("rb-abc", "rio", Some("ip-10-0-1-5"), &[]);
        assert_eq!(
            hw_class_patch_target(&p, &cache),
            Some(("rb-abc".into(), "rio".into(), "intel-7".into()))
        );
        // Already annotated → skip (sticky).
        let p = pod(
            "rb-abc",
            "rio",
            Some("ip-10-0-1-5"),
            &[(ANNOT_HW_CLASS, "intel-7")],
        );
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Pending (no nodeName yet) → skip.
        let p = pod("rb-pending", "rio", None, &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Node not in cache (informer race / non-Karpenter) → skip.
        let p = pod("rb-unk", "rio", Some("ip-10-0-9-9"), &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Cached node but no $h matches (e.g. non-builder NodePool, or
        // config not yet loaded) → skip — don't stamp a bogus value.
        cache.apply(&node("nomatch", &[("rio.build/hw-band", "9")]));
        let p = pod("rb-nm", "rio", Some("nomatch"), &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
    }

    #[test]
    fn apply_upserts_on_label_change() {
        let cache = band_cache();
        cache.apply(&node("n", &[("rio.build/hw-band", "6")]));
        assert_eq!(cache.hw_class_of("n"), Some("intel-6".into()));
        // Relabel (e.g., operator manually patched) → Modify event →
        // apply() again → cache reflects new value.
        cache.apply(&node("n", &[("rio.build/hw-band", "7")]));
        assert_eq!(cache.hw_class_of("n"), Some("intel-7".into()));
        assert_eq!(cache.len(), 1, "upsert, not duplicate");
    }
}
