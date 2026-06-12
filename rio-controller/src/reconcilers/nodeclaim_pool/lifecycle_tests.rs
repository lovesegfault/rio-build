//! Lifecycle-invariants suite: drives the REAL `tick()` /
//! `reconcile_once` / `consolidate_only` bodies through lease-acquire,
//! lease-loss, standby, ⊥-streak, and consolidate-only edges with an
//! injected clock, asserting the per-field stale-state polarity table
//! (`#r("ctrl.nodeclaim.lease-edge-polarity")`,
//! docs/spec/components/controller.typ).
//!
//! Three bug rounds (r40 bug_012/020, r42 bug_023, r43 bug_023/
//! merged_016) hit `consolidate_only`/`observe_*`/lease-edge ORDERING
//! interactions that single-path verifiers missed — so this suite
//! never re-stages calls in its own order: every test drives the real
//! tick bodies through the seams that already exist:
//!
//! - kube: [`rio_test_support::kube_mock::ApiServerVerifier`] — a
//!   scripted ordered scenario queue. Every tick structurally pins its
//!   exact kube request set (an unexpected LIST or reap DELETE fails
//!   the scenario match; a missing one trips the guard's join timeout).
//! - scheduler: [`rio_test_support::grpc::admin::MockAdmin`]
//!   (programmable poll/ledger responses + recorded ack requests) and
//!   [`rio_test_support::grpc::dead_channel`] for ⊥ ticks.
//! - PG: [`rio_test_support::pg::TestDb`]; a deterministically-failing
//!   pool is a second pool from the same TestDb with `.close()`
//!   awaited (instant `PoolClosed`, no connect timeouts).
//! - clock: the threaded `tick(now: SystemTime)` parameter.
//!
//! Construction is by struct literal (this is a child module of
//! `nodeclaim_pool`, so private fields are visible), bypassing the
//! async PG-loading `new()`. NOTE: every new `NodeClaimPoolReconciler`
//! field lands in [`Lab::reconciler`] — that touch is exactly where
//! the polarity rule demands the author classify the field
//! (controller.typ, lease-edge-polarity rule body).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kube::Api;
use rio_lease::{LeaderState, LeaseHooks as _};
use rio_proto::types::{ListOpenAttemptsResponse, OpenAttempt};
use rio_test_support::grpc::{MockAdmin, dead_channel, spawn_mock_admin};
use rio_test_support::kube_mock::{ApiServerVerifier, Method, Scenario};
use rio_test_support::pg::TestDb;
use serde_json::{Value, json};

use super::*;

/// Base test epoch: 2026-01-01T00:00:00Z. All scenario timestamps are
/// rendered by [`rfc3339`] as offsets within that day.
const T0: u64 = 1_767_225_600;

/// Injected tick clock: `T0 + secs` as a `SystemTime`.
fn at(secs: u64) -> SystemTime {
    UNIX_EPOCH + Duration::from_secs(T0 + secs)
}

/// RFC3339 render of `T0 + secs` (offsets must stay within one day —
/// asserted, not truncated).
fn rfc3339(secs: u64) -> String {
    assert!(secs < 86_400, "test offsets stay within 2026-01-01");
    format!(
        "2026-01-01T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// The suite's single test cell: `mid-ebs-x86:spot` (the `*`
/// min-consolidation glob → 300s policy floor).
fn cell() -> Cell {
    Cell("mid-ebs-x86".into(), CapacityType::Spot)
}

/// Decode one wire cell-event entry via the shared grammar
/// (`rio_common::cell_wire`) and project the CELL identity — wire
/// entries now carry the `@epoch` suffix (merged_bug_008), so tests
/// match through the decoder instead of string equality (witness
/// provenance: the same codec production consumes).
fn wire_cell(s: &str) -> Cell {
    let p = rio_common::cell_wire::decode_cell_event(s).expect("wire entry decodes");
    Cell(p.hw_class, p.capacity.into())
}

/// Whether `plane` carries `c` (decoded — epoch-agnostic).
fn carries(plane: &[String], c: &Cell) -> bool {
    plane.iter().any(|s| wire_cell(s) == *c)
}

/// The minted epoch of one wire entry (None = legacy epoch-less).
fn wire_epoch(s: &str) -> Option<rio_common::cell_wire::EvidenceEpoch> {
    rio_common::cell_wire::decode_cell_event(s)
        .expect("wire entry decodes")
        .epoch
}

// ───────────────────────── JSON builders ─────────────────────────────

/// NodeClaim list-item JSON. `registered`: `Some(transition_offset)` ⇒
/// `Registered=True` at `T0+offset` with `status.nodeName =
/// "node-{name}"`; `None` ⇒ in-flight (no Registered condition, no
/// nodeName). `created` is the `metadata.creationTimestamp` offset.
fn nc_json(name: &str, created: u64, registered: Option<u64>) -> Value {
    let mut status = json!({
        "allocatable": { "cpu": "8", "memory": "32Gi", "ephemeral-storage": "100Gi" },
    });
    if let Some(t) = registered {
        status["conditions"] = json!([{
            "type": "Registered", "status": "True",
            "lastTransitionTime": rfc3339(t),
            "reason": "", "message": "",
        }]);
        status["nodeName"] = json!(format!("node-{name}"));
    }
    json!({
        "apiVersion": "karpenter.sh/v1",
        "kind": "NodeClaim",
        "metadata": {
            "name": name,
            "creationTimestamp": rfc3339(created),
            "labels": {
                "rio.build/hw-class": "mid-ebs-x86",
                "karpenter.sh/capacity-type": "spot",
                "rio.build/nodeclaim-pool": "builder",
            },
        },
        "spec": {
            "nodeClassRef": {
                "group": "karpenter.k8s.aws",
                "kind": "EC2NodeClass",
                "name": "default",
            },
        },
        "status": status,
    })
}

/// NEVER-Registered NodeClaim observed mid-GC-transit: Karpenter set
/// `deletionTimestamp` (terminal launch failure → finalize) but the
/// finalizer hasn't cleared yet — the live_050(b) window. NOT reaped by
/// `classify` (already-terminating claims are skipped); only
/// `detect_vanished`'s exit alphabet sees it.
fn nc_json_terminating(name: &str, created: u64) -> Value {
    let mut v = nc_json(name, created, None);
    v["metadata"]["deletionTimestamp"] = json!(rfc3339(created + 2));
    v["metadata"]["finalizers"] = json!(["karpenter.sh/termination"]);
    v
}

/// NEVER-Registered NodeClaim with `Launched=True` — capacity
/// materialized but the kubelet never registered. Past the ice
/// timeout, [`health::classify`] reaps it as `BootTimeout` (no ICE
/// mask — the pinned `record_reap` posture).
fn nc_json_boot_stuck(name: &str, created: u64) -> Value {
    let mut v = nc_json(name, created, None);
    v["status"]["conditions"] = json!([{
        "type": "Launched", "status": "True",
        "lastTransitionTime": rfc3339(created + 1),
        "reason": "", "message": "",
    }]);
    v
}

/// [`nc_json_boot_stuck`] observed mid-teardown (`deletionTimestamp`
/// set, finalizer pending) — the W9-BB ambiguous-commit window: the
/// controller's earlier DELETE errored but committed server-side.
fn nc_json_boot_stuck_terminating(name: &str, created: u64) -> Value {
    let mut v = nc_json_boot_stuck(name, created);
    v["metadata"]["deletionTimestamp"] = json!(rfc3339(created + 250));
    v["metadata"]["finalizers"] = json!(["karpenter.sh/termination"]);
    v
}

/// REGISTERED NodeClaim observed mid-teardown (`deletionTimestamp`
/// set, the ~60-90s Karpenter finalizer draining) — the W11-AG
/// ambiguous-commit window for registered-claim reaps (Dead/idle):
/// the controller's earlier DELETE errored but committed server-side.
fn nc_json_registered_terminating(name: &str, created: u64, registered_at: u64) -> Value {
    let mut v = nc_json(name, created, Some(registered_at));
    v["metadata"]["deletionTimestamp"] = json!(rfc3339(created + 35));
    v["metadata"]["finalizers"] = json!(["karpenter.sh/termination"]);
    v
}

/// In-flight NodeClaim carrying `Launched=False reason=LaunchFailed` —
/// [`health::classify`] short-circuits this to an ICE reap with no age
/// gate (the deterministic reap shape).
fn nc_json_ice(name: &str, created: u64) -> Value {
    let mut v = nc_json(name, created, None);
    v["status"]["conditions"] = json!([{
        "type": "Launched", "status": "False",
        "lastTransitionTime": rfc3339(created),
        "reason": "LaunchFailed", "message": "",
    }]);
    v
}

/// Running pod bound to `node` requesting `cores` CPUs (drives
/// `PodSnapshot::requested_for` — `cores > 0` ⇒ the node is busy).
fn pod_json(name: &str, node: &str, cores: u32) -> Value {
    json!({
        "apiVersion": "v1",
        "kind": "Pod",
        "metadata": {
            "name": name,
            "creationTimestamp": rfc3339(0),
            "labels": { "rio.build/pool": "p" },
        },
        "spec": {
            "nodeName": node,
            "containers": [{
                "name": "c",
                "resources": { "requests": { "cpu": format!("{cores}") } },
            }],
        },
        "status": { "phase": "Running" },
    })
}

fn list(kind: &str, api_version: &str, items: Vec<Value>) -> String {
    json!({
        "kind": kind, "apiVersion": api_version,
        "metadata": {}, "items": items,
    })
    .to_string()
}

fn nc_list(items: Vec<Value>) -> String {
    list("NodeClaimList", "karpenter.sh/v1", items)
}

fn pod_list(items: Vec<Value>) -> String {
    list("PodList", "v1", items)
}

fn pool_list_empty() -> String {
    list("PoolList", "rio.build/v1alpha1", vec![])
}

/// `Api::delete` response: a success `Status` envelope.
fn delete_scenario(name: &'static str) -> Scenario {
    Scenario::ok(
        Method::DELETE,
        name,
        json!({
            "kind": "Status", "apiVersion": "v1",
            "status": "Success", "code": 200,
        })
        .to_string(),
    )
}

// ─────────────────────── per-tick-mode scenarios ─────────────────────

/// Healthy `reconcile_once` tick: `[Pools LIST, Pods LIST, NodeClaims
/// LIST]` + any scripted effects (reap DELETEs / cover POSTs), in the
/// real bodies' order.
fn full_tick_scenario(pods: Vec<Value>, ncs: Vec<Value>, effects: Vec<Scenario>) -> Vec<Scenario> {
    let mut v = vec![
        Scenario::ok(
            Method::GET,
            "/apis/rio.build/v1alpha1/pools",
            pool_list_empty(),
        ),
        Scenario::ok(Method::GET, "/api/v1/pods", pod_list(pods)),
        Scenario::ok(
            Method::GET,
            "/apis/karpenter.sh/v1/nodeclaims",
            nc_list(ncs),
        ),
    ];
    v.extend(effects);
    v
}

/// `consolidate_only` tick: `[Pods LIST, NodeClaims LIST]` + scripted
/// effects (reap_idle DELETEs come before reap_unhealthy DELETEs).
fn consolidate_tick_scenario(
    pods: Vec<Value>,
    ncs: Vec<Value>,
    effects: Vec<Scenario>,
) -> Vec<Scenario> {
    let mut v = vec![
        Scenario::ok(Method::GET, "/api/v1/pods", pod_list(pods)),
        Scenario::ok(
            Method::GET,
            "/apis/karpenter.sh/v1/nodeclaims",
            nc_list(ncs),
        ),
    ];
    v.extend(effects);
    v
}

/// Pre-threshold ⊥ tick (streak 1..4): the THIRD per-tick-mode
/// builder. The fixed ⊥ arm runs the shared kube-only observation
/// block — exactly `[Pods LIST, NodeClaims LIST]`, no effects (an
/// unexpected reap DELETE or ack panics the scenario match; a missing
/// LIST trips the guard's join timeout). Every pre-threshold ⊥ tick
/// in the suite routes through this ONE builder, so the
/// disposition-dependent request shape lives in exactly one place.
fn bot_tick_scenario(pods: Vec<Value>, ncs: Vec<Value>) -> Vec<Scenario> {
    vec![
        Scenario::ok(Method::GET, "/api/v1/pods", pod_list(pods)),
        Scenario::ok(
            Method::GET,
            "/apis/karpenter.sh/v1/nodeclaims",
            nc_list(ncs),
        ),
    ]
}

// ───────────────────────────── the Lab ───────────────────────────────

/// Everything a lifecycle test needs: the reconciler (struct-literal
/// built), its programmable peers, and the lease/gate handles.
struct Lab {
    r: NodeClaimPoolReconciler,
    db: TestDb,
    admin: MockAdmin,
    /// Live channel to [`Self::admin`]; swap `r.admin` to
    /// [`dead_channel`] for ⊥ ticks and back to this for recovery.
    admin_channel: tonic::transport::Channel,
    gate: PlaceableGate,
    leader_flag: Arc<AtomicBool>,
    /// Keep the mock admin server task alive for the Lab's lifetime.
    _admin_server: tokio::task::JoinHandle<()>,
}

fn admin_client(ch: tonic::transport::Channel) -> AdminClient {
    rio_proto::AdminServiceClient::with_interceptor(
        ch,
        crate::reconcilers::fence::GenerationStamp::new(
            rio_auth::hmac::ServiceTokenInterceptor::new(None, "rio-controller"),
            std::sync::Arc::new(std::sync::atomic::AtomicU64::new(1)),
        ),
    )
}

impl Lab {
    async fn new() -> Self {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (admin, addr, server) = spawn_mock_admin().await.expect("mock admin");
        let admin_channel = tonic::transport::Endpoint::try_from(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect mock admin");
        let leader_flag = Arc::new(AtomicBool::new(true));
        let leader =
            LeaderState::from_parts(Arc::new(AtomicU64::new(1)), leader_flag.clone(), true);
        let (placeable_tx, gate) = placeable_channel();
        // Throwaway client for construction; every driven tick swaps in
        // a fresh verifier-backed client (see [`Lab::tick`]).
        let (client, _verifier) = ApiServerVerifier::new();
        let r = NodeClaimPoolReconciler {
            nodeclaims: Api::all(client.clone()),
            pools: Api::all(client.clone()),
            pods: Api::all(client),
            admin: admin_client(admin_channel.clone()),
            pg: db.pool.clone(),
            leader,
            cfg: NodeClaimPoolConfig::default(),
            hw_config: crate::reconcilers::node_informer::HwClassConfig::default(),
            placeable_tx,
            hooks: ControllerLeaseHooks::default(),
            sketches: CellSketches::default(),
            recorded_boot: HashSet::new(),
            prev_idle: HashMap::new(),
            prev_extra_cells: HashSet::new(),
            prev_unplaced_extras: HashSet::new(),
            inflight_created: HashMap::new(),
            // Polarity: SUPPRESS — cleared on the acquisition edge
            // beside `inflight_created` (bug_094; see the per-field
            // table at the acquire match).
            delete_tombstones: health::DeleteTombstones::default(),
            consecutive_bot_ticks: 0,
            pending_evidence: Default::default(),
            edge_seen_epoch: 0,
            reloaded_epoch: 0,
            tick_counter: 0,
            wedge: wedge::WedgeTracker::default(),
            pending_wedge_evictions: std::collections::BTreeSet::new(),
        };
        Self {
            r,
            db,
            admin,
            admin_channel,
            gate,
            leader_flag,
            _admin_server: server,
        }
    }

    /// Drive ONE real tick at `T0 + t_off` against a fresh scripted
    /// apiserver. The guard join proves the tick made EXACTLY the
    /// scripted kube requests (order, count, and nothing else).
    async fn tick(&mut self, t_off: u64, scenarios: Vec<Scenario>) {
        let (client, verifier) = ApiServerVerifier::new();
        self.r.nodeclaims = Api::all(client.clone());
        self.r.pools = Api::all(client.clone());
        self.r.pods = Api::all(client);
        let guard = verifier.run(scenarios);
        self.r.tick(at(t_off)).await;
        guard.verified().await;
    }

    /// A second pool on the same TestDb, pre-closed: every PG call
    /// fails instantly with `PoolClosed` (the deterministic reload-Err
    /// driver — no connect timeouts).
    async fn closed_pool(&self) -> sqlx::PgPool {
        let p = self.db.reopen().await;
        p.close().await;
        p
    }

    /// All `nodeclaim_cell_state` rows as ordered text — the
    /// byte-unchanged comparator for the persist-gate tests.
    async fn pg_rows(&self) -> Vec<String> {
        sqlx::query_scalar::<_, String>(
            "SELECT t::text FROM nodeclaim_cell_state t ORDER BY t::text",
        )
        .fetch_all(&self.db.pool)
        .await
        .expect("rows")
    }

    fn ack_calls(&self) -> Vec<rio_proto::types::AckSpawnedIntentsRequest> {
        self.admin.ack_calls.read().unwrap().clone()
    }

    /// Program the open-attempt ledger view (the OA2 wedge input).
    fn set_open_attempts(&self, attempts: Vec<OpenAttempt>) {
        *self.admin.open_attempts.write().unwrap() = ListOpenAttemptsResponse {
            recently_closed: vec![],
            attempts,
            leader_for_secs: 3600,
        };
    }

    /// Boot-sample count for [`cell`] (the `boot_active` sketch).
    fn boot_samples(&self) -> usize {
        self.r
            .sketches
            .get(&cell())
            .map_or(0, |s| s.boot_active.count())
    }

    fn idle_gaps(&self) -> Vec<consolidate::IdleGapEvent> {
        self.r
            .sketches
            .get(&cell())
            .map(|s| s.idle_gap_events.clone())
            .unwrap_or_default()
    }
}

/// Persist a "previous leader's" sketch state (5 boot samples on
/// [`cell`]) into PG; returns the count for later comparison.
async fn seed_previous_leader(db: &TestDb) -> usize {
    let mut s = CellSketches::default();
    let st = s.cell_mut(&cell());
    for v in [10.0, 11.0, 12.0, 13.0, 14.0] {
        st.boot_active.add(v);
    }
    s.persist(&db.pool).await.expect("seed persist");
    5
}

// ────────────────────────────── tests ────────────────────────────────

/// R1+R2+R3 acquire-Ok arm: `prev_idle` (AMPLIFY) and the suppress
/// fields (`recorded_boot`, `inflight_created`) all clear; sketches
/// reload from PG; the latch drops. Live claims make every clear
/// observable: a wrongly-retained `prev_idle` entry would record a
/// huge uncensored gap on the busy node; a wrongly-retained
/// `inflight_created` entry would still be tracked (its claim is live
/// and in-flight, so nothing else drops it); the cleared
/// `recorded_boot` re-edges the FRESH registration into a sample.
// r[verify ctrl.nodeclaim.lease-edge-polarity+4]
#[tokio::test]
async fn acquire_ok_clears_prev_idle_and_suppress_fields() {
    let mut lab = Lab::new().await;
    let seeded = seed_previous_leader(&lab.db).await;
    // Stale standby state: a marker cell PG does not have, plus all
    // three edge-detector sets non-empty and BACKED BY LIVE CLAIMS.
    lab.r
        .sketches
        .cell_mut(&Cell("stale-marker".into(), CapacityType::Spot))
        .boot_active
        .add(99.0);
    lab.r.prev_idle.insert("n1".into(), 1.0); // ancient idle-since
    lab.r.recorded_boot.insert("n1".into());
    lab.r.inflight_created.insert("n9".into(), cell());
    lab.r.hooks.on_acquire();

    // n1: registered FRESH (edge at t-5, inside the 30s gate), busy.
    // n9: live, in-flight, young.
    lab.tick(
        600,
        full_tick_scenario(
            vec![pod_json("p1", "node-n1", 4)],
            vec![nc_json("n1", 0, Some(595)), nc_json("n9", 595, None)],
            vec![],
        ),
    )
    .await;

    assert!(
        !lab.r.prev_idle.contains_key("n1"),
        "AMPLIFY: stale idle-since cleared (busy node not re-seeded)"
    );
    assert!(
        lab.idle_gaps().iter().all(|g| g.censored),
        "no uncensored gap: the stale entry was cleared BEFORE the \
         idle→busy observation, not consumed by it"
    );
    assert!(
        !lab.r.inflight_created.contains_key("n9"),
        "suppress: cleared on the ACQUISITION EDGE (merged_bug_004 — \
         the pre-acquire entry is previous-tenure tracking)"
    );
    assert!(!lab.r.reload_pending(), "latch dropped on Ok");
    assert_eq!(
        lab.boot_samples(),
        seeded + 1,
        "sketches == PG content + the fresh re-edge sample (recorded_boot \
         was cleared, and the recency gate ADMITS an in-window edge)"
    );
    assert!(
        lab.r
            .sketches
            .get(&Cell("stale-marker".into(), CapacityType::Spot))
            .is_none(),
        "stale standby sketch replaced by the PG reload"
    );
    assert!(
        lab.ack_calls()
            .iter()
            .any(|a| carries(&a.registered_cells, &cell())),
        "fresh in-window edge ships its ICE-clear"
    );
}

/// R1 acquire-Err arm (r43 merged_bug_016, the m1CalibAcquireClearOkOnly
/// shape): `prev_idle` STILL clears; `recorded_boot` is retained (its
/// re-arm rides the Ok arm with the sketch swap — atomic edge); the
/// latch holds (persist stays gated). merged_bug_004: the latched
/// buffers (`inflight_created`, `pending_evidence`) clear on the
/// ACQUISITION EDGE regardless of the reload outcome — their
/// pre-acquire content is previous-tenure state (an interim leader's
/// deliberate reaps must not read as Karpenter GC), and the edge,
/// not the Ok arm, is the one clear site. Live claims make it
/// non-vacuous: n1 is FRESH-registered and already in `recorded_boot`
/// — a wrongful Err-arm clear would re-edge it into a sample + an
/// ICE-clear ack; n9 is live in-flight and tracked pre-acquire — the
/// edge unconditionally drops that previous-tenure entry.
// r[verify ctrl.nodeclaim.lease-edge-polarity+4]
#[tokio::test]
async fn acquire_err_still_clears_prev_idle_keeps_suppress() {
    let mut lab = Lab::new().await;
    lab.r.prev_idle.insert("n1".into(), 1.0);
    lab.r.recorded_boot.insert("n1".into());
    lab.r.inflight_created.insert("n9".into(), cell());
    lab.r.hooks.on_acquire();
    lab.r.pg = lab.closed_pool().await;

    lab.tick(
        600,
        full_tick_scenario(
            vec![pod_json("p1", "node-n1", 4)],
            vec![nc_json("n1", 0, Some(595)), nc_json("n9", 595, None)],
            vec![],
        ),
    )
    .await;

    assert!(
        !lab.r.prev_idle.contains_key("n1") && lab.idle_gaps().is_empty(),
        "AMPLIFY: cleared even on Err — no over-counted gap recorded"
    );
    assert!(
        lab.r.recorded_boot.contains("n1"),
        "suppress: retained on Err"
    );
    assert_eq!(
        lab.boot_samples(),
        0,
        "no re-edge: the fresh registration was already recorded"
    );
    assert!(
        lab.ack_calls()
            .iter()
            .all(|a| a.registered_cells.is_empty()),
        "no ICE-clear shipped (retained recorded_boot suppressed the edge)"
    );
    assert!(
        !lab.r.inflight_created.contains_key("n9"),
        "latched buffer: cleared on the acquisition edge even when the \
         reload Errs (merged_bug_004 — previous-tenure tracking is \
         dropped exactly once, at the edge)"
    );
    assert!(lab.r.reload_pending(), "latch held for retry");
}

/// merged_bug_004 red: evidence buffered during the reload-Err retry
/// window is CURRENT-tenure, consume-once state — the eventual
/// reload-Ok must not destroy it. The edge fires on tick 1 (clears the
/// previous tenure's buffers exactly once); the reload Errs and
/// `reconcile_once` runs degraded, during which an ICE mark enters
/// `pending_evidence` and `cover_deficit` tracks a fresh claim in
/// `inflight_created`. Tick 2's reload-Ok must leave both intact:
/// pre-fix the Ok arm re-cleared both against the SAME epoch read the
/// edge already consumed — `left: tick-2 ack ships
/// unfulfillable_cells == []` (the mark destroyed before
/// `report_unfulfillable` could read it) and the degraded-window
/// claim untracked for good; `right: the ack carries the mark` (it
/// provably reached the scheduler; Ack-Ok then legitimately consumes
/// the buffer) and the claim stays tracked.
// r[verify ctrl.nodeclaim.lease-edge-polarity+4]
// r[verify ctrl.nodeclaim.inflight-conservation+3]
#[tokio::test]
async fn reload_ok_preserves_evidence_buffered_during_err_window() {
    let mut lab = Lab::new().await;
    lab.r.hooks.on_acquire();
    lab.r.pg = lab.closed_pool().await;

    // Tick 1: edge actions fire (previous tenure cleared once); the
    // reload Errs; the latch holds.
    lab.tick(600, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert!(lab.r.reload_pending(), "precondition: Err window open");

    // Degraded-window production (what reap_unhealthy/detect_vanished
    // and cover_deficit do on Err-window ticks): an ICE mark enters
    // the commit-on-Ack buffer; a freshly created claim is tracked.
    lab.r.pending_evidence.buffer_marks([cell()]);
    lab.r.inflight_created.insert("n-degraded".into(), cell());

    // Tick 2: PG recovered → reload Ok. The claim created during the
    // window is live and in-flight, so detect_vanished KEEPs it.
    lab.r.pg = lab.db.pool.clone();
    lab.tick(
        610,
        full_tick_scenario(vec![], vec![nc_json("n-degraded", 600, None)], vec![]),
    )
    .await;

    assert!(!lab.r.reload_pending(), "latch cleared on Ok");
    let last = lab.ack_calls().pop().expect("tick 2 acked");
    assert_eq!(
        last.unfulfillable_cells
            .iter()
            .map(|s| wire_cell(s))
            .collect::<Vec<_>>(),
        vec![cell()],
        "the Err-window ICE mark SURVIVED the reload-Ok and shipped on \
         the first healthy Ack (pre-fix: the Ok-arm clear destroyed it \
         and the ack went out empty)"
    );
    assert!(
        lab.r.pending_evidence.ice_cells().next().is_none(),
        "Ack-Ok is the one legitimate consume of the shipped mark"
    );
    assert!(
        lab.r.inflight_created.contains_key("n-degraded"),
        "current-tenure tracking born in the Err window survives the \
         reload-Ok (pre-fix: untracked for good — its vanish would \
         never have emitted an ICE mark)"
    );
}

/// merged_bug_005 red (ordering-inversion, end-to-end): an ICE mark
/// buffered behind a failed Ack must be SUPERSEDED by a strictly
/// newer `Registered=True` edge for the same cell — the cell provably
/// delivered capacity after the failure the mark recorded. `left:`
/// the retried Ack carried the cell in BOTH planes and the
/// scheduler's fixed clears-then-marks apply order re-masked the
/// healthy cell; `right:` the buffer holds only the newest polarity,
/// so the Ack ships the clear alone.
// r[verify ctrl.nodeclaim.evidence-ack-latch+3]
// r[verify ctrl.nodeclaim.ice-mark-clear+5]
#[tokio::test]
async fn newer_registration_supersedes_buffered_mark_end_to_end() {
    let mut lab = Lab::new().await;

    // Tick 1: c-ice carries Launched=False/LaunchFailed → ICE reap
    // (scripted DELETE) → the mark enters the buffer; the Ack fails →
    // the mark is retained (commit-on-Ack).
    lab.admin
        .fail_next_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    lab.tick(
        600,
        full_tick_scenario(
            vec![],
            vec![nc_json_ice("c-ice", 0)],
            vec![delete_scenario("/apis/karpenter.sh/v1/nodeclaims/c-ice")],
        ),
    )
    .await;
    let acks = lab.ack_calls();
    assert!(
        carries(&acks[0].unfulfillable_cells, &cell()),
        "tick 1 carried the mark to the wire (and the Ack failed)"
    );

    // Tick 2: a sibling claim in the SAME cell reaches Registered=True
    // inside the recency window — the consume-once success edge,
    // strictly newer than the buffered mark.
    lab.tick(
        610,
        full_tick_scenario(vec![], vec![nc_json("c-new", 600, Some(605))], vec![]),
    )
    .await;
    let acks = lab.ack_calls();
    let last = acks.last().expect("tick 2 acked");
    assert!(
        carries(&last.registered_cells, &cell()),
        "the newer registration ships its clear"
    );
    assert!(
        last.unfulfillable_cells.is_empty(),
        "the stale mark was superseded — in THIS direction (clear \
         newest) the mark is evicted, never shipped (merged_bug_005 \
         law, unchanged by the ordered-evidence model): {last:?}"
    );
}

/// merged_bug_003 red (end-to-end at the request boundary): same-tick
/// clear-then-mark — a `Registered=True` edge buffered by the
/// kube-only block, then an ICE reap of a sibling claim in the SAME
/// cell later in the SAME tick (real production order: clears are
/// buffered before the reap paths buffer marks). `left (pre-fix):
/// buffer_marks did registered_cells.remove(&c) — the buffer shipped
/// mark-only and the scheduler climbed from its stale rung (the
/// consume-once reset destroyed)` / `right: the Ack carries the cell
/// in BOTH planes with decoded clear epoch < mark epoch — the
/// scheduler's fixed clears-then-marks order + epoch gate realize
/// reset-then-step-0`.
// r[verify ctrl.nodeclaim.evidence-ack-latch+3]
// r[verify ctrl.nodeclaim.ice-mark-clear+5]
#[tokio::test]
async fn clear_then_mark_ships_both_planes_with_ordered_epochs() {
    let mut lab = Lab::new().await;

    // One tick: c-new reaches Registered=True inside the recency
    // window (clear produced FIRST, by the kube-only block) AND c-ice
    // is ICE-reaped (mark produced SECOND, by the reap path) — both
    // in cell().
    lab.tick(
        610,
        full_tick_scenario(
            vec![],
            vec![nc_json("c-new", 600, Some(605)), nc_json_ice("c-ice", 0)],
            vec![delete_scenario("/apis/karpenter.sh/v1/nodeclaims/c-ice")],
        ),
    )
    .await;

    let acks = lab.ack_calls();
    let last = acks.last().expect("tick acked");
    assert!(
        carries(&last.registered_cells, &cell()),
        "the buffered clear SURVIVES the newer mark and ships \
         (pre-fix: destroyed by registered_cells.remove): {last:?}"
    );
    assert!(
        carries(&last.unfulfillable_cells, &cell()),
        "the newer mark ships too: {last:?}"
    );
    let clear_e = last
        .registered_cells
        .iter()
        .find(|s| wire_cell(s) == cell())
        .and_then(|s| wire_epoch(s))
        .expect("clear entry carries its minted epoch");
    let mark_e = last
        .unfulfillable_cells
        .iter()
        .find(|s| wire_cell(s) == cell())
        .and_then(|s| wire_epoch(s))
        .expect("mark entry carries its minted epoch");
    assert!(
        clear_e < mark_e,
        "chronology on the wire: clear {clear_e} < mark {mark_e}"
    );
}

/// R2 recency gate: a stale (>3×TICK) Registered edge after the
/// acquire clear is recorded WITHOUT a sample and WITHOUT an ICE-clear
/// on the wire (noMassClearAfterFailover / m34CalibNoRecencyGate).
// r[verify ctrl.nodeclaim.ice-mark-clear+5]
#[tokio::test]
async fn post_acquire_stale_registration_records_without_clear_or_sample() {
    let mut lab = Lab::new().await;
    lab.r.hooks.on_acquire();
    lab.tick(600, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert!(lab.r.recorded_boot.is_empty());

    // Registered 5 minutes before the observing tick (300s > 30s gate).
    let stale = nc_json("n-old", 200, Some(310));
    lab.tick(610, full_tick_scenario(vec![], vec![stale], vec![]))
        .await;

    assert!(lab.r.recorded_boot.contains("n-old"), "record-only insert");
    assert_eq!(lab.boot_samples(), 0, "no sample past the recency gate");
    assert!(
        lab.ack_calls()
            .iter()
            .all(|a| a.registered_cells.is_empty()),
        "no ICE-clear shipped for a stale registration"
    );
}

/// R3 conservation, consolidate-only arm (r40 bug_012): the
/// controller's own stuck-reap is pruned from `inflight_created`
/// BEFORE detect_vanished, so the recovery tick ships no spurious
/// ICE mark for it.
// r[verify ctrl.nodeclaim.inflight-conservation+3]
#[tokio::test]
async fn consolidate_only_prunes_own_reaps_before_detect() {
    let mut lab = Lab::new().await;
    lab.r.inflight_created.insert("c-ice".into(), cell());
    lab.r.consecutive_bot_ticks = 4;
    lab.r.admin = admin_client(dead_channel());

    // ⊥ tick 5 → consolidate-only. The Launched=False/LaunchFailed
    // claim is ICE-reaped (scripted DELETE); its name must leave
    // inflight_created in the same tick.
    lab.tick(
        0,
        consolidate_tick_scenario(
            vec![],
            vec![nc_json_ice("c-ice", 0)],
            vec![delete_scenario("/apis/karpenter.sh/v1/nodeclaims/c-ice")],
        ),
    )
    .await;
    assert!(
        lab.r.inflight_created.is_empty(),
        "own reap pruned in consolidate-only (bug_012 close)"
    );

    // Recovery: c-ice is gone from live — detect_vanished must NOT
    // re-read the absence as Karpenter GC. bug_082 sibling: the
    // GENUINE mark from the consolidate-tick ICE reap (record_reap at
    // delete) is buffered across the outage and ships now — so the
    // recovery ack carries EXACTLY ONE entry for the cell. A vanish
    // misread would have added a second (the BTreeSet dedups by cell,
    // so the structural assertion is: the mark is present, and it is
    // the buffered reap mark, not a fresh detect_vanished product —
    // pinned by inflight_created being empty since the reap tick).
    lab.r.admin = admin_client(lab.admin_channel.clone());
    lab.tick(10, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert_eq!(lab.r.consecutive_bot_ticks, 0);
    let last = lab.ack_calls().last().cloned().expect("recovery ack");
    assert_eq!(
        last.unfulfillable_cells
            .iter()
            .map(|s| wire_cell(s))
            .collect::<Vec<_>>(),
        vec![cell()],
        "the consolidate-tick reap's buffered mark ships once on recovery \
         (no spurious vanish-misread duplicate, no dropped mark)"
    );
}

/// R3 conservation, vanish arm (r40 bug_020): a tracked claim absent
/// from live (Karpenter GC) marks its cell; a tracked claim still
/// in-flight stays tracked.
// r[verify ctrl.nodeclaim.inflight-conservation+3]
#[tokio::test]
async fn vanish_is_marked_own_reap_is_not() {
    let mut lab = Lab::new().await;
    let cell_gone = Cell("mid-ebs-x86".into(), CapacityType::Spot);
    lab.r
        .inflight_created
        .insert("c-gone".into(), cell_gone.clone());
    lab.r.inflight_created.insert("c-fly".into(), cell());

    // c-fly is live and in-flight (young, no failure condition);
    // c-gone is absent → vanish.
    lab.tick(
        5,
        full_tick_scenario(vec![], vec![nc_json("c-fly", 0, None)], vec![]),
    )
    .await;

    assert!(
        !lab.r.inflight_created.contains_key("c-gone"),
        "vanished claim dropped"
    );
    assert!(
        lab.r.inflight_created.contains_key("c-fly"),
        "in-flight claim KEPT (bug_020 arm)"
    );
    let acks = lab.ack_calls();
    assert!(
        acks.iter()
            .any(|a| carries(&a.unfulfillable_cells, &cell_gone)),
        "vanish marked its cell on the wire; acks: {acks:?}"
    );
}

/// W9-BB (bug_094, the provenance half): an ambiguous-commit delete —
/// the controller's own `BootTimeout` reap whose DELETE RPC errs but
/// commits server-side — must classify next tick as THIS controller's
/// reap (no ICE evidence on the wire), never as Karpenter teardown.
/// The mask it must not mint is exactly the one the pinned
/// `record_reap` test forbids for `BootTimeout`. Pre-fix red
/// (verbatim): `an ambiguous-commit BootTimeout delete must never
/// ICE-mask (...): [.., AckSpawnedIntentsRequest { ..,
/// unfulfillable_cells: ["mid-ebs-x86:spot@1781162843070"], .. }]` —
/// tick 2 shipped the false mask through the vanish fold.
// r[verify ctrl.nodeclaim.ice-mark-clear+5]
#[tokio::test]
async fn ambiguous_commit_delete_classifies_as_self_reap_not_ice() {
    let mut lab = Lab::new().await;
    lab.r.inflight_created.insert("c-bt".into(), cell());

    // Tick 1: c-bt is boot-stuck (Launched=True, never Registered,
    // age 200s > the 60s default timeout) → BootTimeout reap; the
    // DELETE errs 503 AFTER the apiserver committed it.
    lab.tick(
        200,
        full_tick_scenario(
            vec![],
            vec![nc_json_boot_stuck("c-bt", 0)],
            vec![Scenario::k8s_error(
                Method::DELETE,
                "/apis/karpenter.sh/v1/nodeclaims/c-bt",
                503,
                "ServiceUnavailable",
                "etcd leader changed",
            )],
        ),
    )
    .await;
    // The Err arm tombstones the attempt; the claim stays tracked
    // (present, not yet terminating in this tick's view).
    assert!(lab.r.inflight_created.contains_key("c-bt"));
    assert!(
        lab.r.delete_tombstones.contains("c-bt"),
        "the ambiguous attempt's provenance survives the tick"
    );

    // Tick 2: the commit materialized — the claim is observed
    // terminating (never Registered). classify skips terminating
    // claims, so only the vanish fold adjudicates this observation.
    lab.tick(
        210,
        full_tick_scenario(
            vec![],
            vec![nc_json_boot_stuck_terminating("c-bt", 0)],
            vec![],
        ),
    )
    .await;

    let acks = lab.ack_calls();
    assert!(
        acks.iter().all(|a| a.unfulfillable_cells.is_empty()),
        "an ambiguous-commit BootTimeout delete must never ICE-mask \
         (the teardown is this controller's own reap, not Karpenter \
         capacity evidence): {acks:?}"
    );
    assert!(
        lab.r.inflight_created.is_empty(),
        "the confirmed exit leaves tracking"
    );
    assert!(
        lab.r.delete_tombstones.is_empty(),
        "the confirmed exit consumed the tombstone"
    );
}

/// W11-AI (bug_043 — `ctrl.pool.fold-clock`, R29): the tombstone
/// grace is denominated in CONSUMER FOLD EXECUTIONS, and prune runs
/// only AFTER the consult. Pre-fix, TOMBSTONE_TTL aged on the
/// unconditionally-incremented wall tick counter while the vanish
/// fold was SKIPPED on failed-LIST ticks, and prune_expired ran
/// BEFORE detect_vanished — so a tombstone stamped just before a
/// ≥3-tick foldless window was dropped before the first fold ever
/// consulted it, and classify_vanish(None, None) minted GcVanish
/// (the false ICE mask + reaped_total{reason=vanished}) for the
/// controller's own BootTimeout self-reap. The correlated
/// apiserver-disruption path (ambiguous delete error + failed LISTs
/// in the same outage) makes exactly this sequence realistic.
///
/// Proposition: a tombstone is never pruned unconsulted — it
/// survives every foldless window to its first real consult, which
/// classifies SelfReap(BootTimeout): counter under boot-timeout,
/// never vanished, never masked. (The book's shorthand for the
/// post-fix class is the consequence-equivalent BootFailureTeardown
/// row; the actual classify_vanish row with provenance armed is
/// SelfReap — counter parity either way, divergence recorded.)
// r[verify ctrl.pool.fold-clock]
// r[verify ctrl.pool.delete-outcome]
#[test]
fn tombstone_survives_foldless_list_failure_window_to_first_consult() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let mut lab = rt.block_on(Lab::new());
    lab.r.inflight_created.insert("c-bt".into(), cell());

    // Tick 1: boot-stuck c-bt (age 200s > the 60s default timeout) →
    // BootTimeout reap; the DELETE errs 503 AFTER committing.
    rt.block_on(lab.tick(
        200,
        full_tick_scenario(
            vec![],
            vec![nc_json_boot_stuck("c-bt", 0)],
            vec![Scenario::k8s_error(
                Method::DELETE,
                "/apis/karpenter.sh/v1/nodeclaims/c-bt",
                503,
                "ServiceUnavailable",
                "etcd leader changed",
            )],
        ),
    ));
    assert!(lab.r.delete_tombstones.contains("c-bt"));

    // Ticks 2-4: the apiserver outage continues — every Pools LIST
    // fails, the tick body warns and returns, and the vanish fold
    // NEVER RUNS. The wall tick counter keeps advancing: this is the
    // ≥TTL foldless window.
    for t in [210u64, 220, 230] {
        rt.block_on(lab.tick(
            t,
            vec![Scenario::k8s_error(
                Method::GET,
                "/apis/rio.build/v1alpha1/pools",
                500,
                "InternalError",
                "etcd leader election in progress",
            )],
        ));
    }
    assert!(
        lab.r.delete_tombstones.contains("c-bt"),
        "the tombstone must survive the foldless window (it was never \
         consulted): the grace is denominated in fold executions, not \
         wall ticks"
    );

    // Tick 5: recovery — c-bt is fully GC'd (absent). The FIRST real
    // fold since the stamp must consult the tombstone: SelfReap, the
    // original reason's counter, NO mask.
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        rt.block_on(lab.tick(240, full_tick_scenario(vec![], vec![], vec![])));
    });

    let acks = lab.ack_calls();
    assert!(
        acks.iter().all(|a| a.unfulfillable_cells.is_empty()),
        "a provenance-known self-reap must never ICE-mask, even across \
         a foldless window (bug_043 red: the false GcVanish): {acks:?}"
    );
    // ppppp: snapshot exactly once.
    let snap = snapshotter.snapshot().into_vec();
    let count_of = |reason: &str| -> Option<u64> {
        snap.iter().find_map(|(k, _, _, v)| {
            let key = k.key();
            (key.name() == "rio_controller_nodeclaim_reaped_total"
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == reason))
            .then(|| match v {
                DebugValue::Counter(c) => *c,
                _ => 0,
            })
        })
    };
    assert_eq!(
        count_of("boot-timeout"),
        Some(1),
        "the ORIGINAL reason's counter fires at the first consult"
    );
    assert_eq!(
        count_of("vanished"),
        None,
        "never the vanish-attributed counter (bug_043 red)"
    );
    assert!(
        lab.r.delete_tombstones.is_empty() && lab.r.inflight_created.is_empty(),
        "the confirmed exit consumed tombstone and tracking"
    );
}

/// W11-AI sibling — the OTHER skip path of the population product:
/// pre-threshold ⊥ ticks (scheduler unreachable, streak below
/// BOT_TICKS_BEFORE_CONSOLIDATE_ONLY) run kube-only observations and
/// NO vanish fold. Same law: the tombstone survives the ⊥ window to
/// its first consult.
// r[verify ctrl.pool.fold-clock]
#[test]
fn tombstone_survives_bot_tick_window_to_first_consult() {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let mut lab = rt.block_on(Lab::new());
    lab.r.inflight_created.insert("c-bt".into(), cell());

    // Tick 1: the ambiguous BootTimeout reap (as above).
    rt.block_on(lab.tick(
        200,
        full_tick_scenario(
            vec![],
            vec![nc_json_boot_stuck("c-bt", 0)],
            vec![Scenario::k8s_error(
                Method::DELETE,
                "/apis/karpenter.sh/v1/nodeclaims/c-bt",
                503,
                "ServiceUnavailable",
                "etcd leader changed",
            )],
        ),
    ));
    assert!(lab.r.delete_tombstones.contains("c-bt"));

    // Ticks 2-5: scheduler unreachable, streak 1..4 — each ⊥ tick
    // makes exactly the two kube-only LISTs and never folds.
    {
        let _e = rt.enter();
        lab.r.admin = admin_client(dead_channel());
    }
    for t in [210u64, 220, 230, 240] {
        rt.block_on(lab.tick(t, bot_tick_scenario(vec![], vec![])));
    }
    assert!(
        lab.r.delete_tombstones.contains("c-bt"),
        "the ⊥ window is foldless — the fold-denominated grace must not move"
    );

    // Tick 6: recovery; c-bt absent → first consult → SelfReap, no mask.
    lab.r.admin = admin_client(lab.admin_channel.clone());
    rt.block_on(lab.tick(250, full_tick_scenario(vec![], vec![], vec![])));
    let acks = lab.ack_calls();
    assert!(
        acks.iter().all(|a| a.unfulfillable_cells.is_empty()),
        "no false mask after the ⊥ window: {acks:?}"
    );
    assert!(
        lab.r.delete_tombstones.is_empty(),
        "consumed at the consult"
    );
}

/// R4 reload latch: a closed-pool acquire gates persist (PG rows
/// byte-unchanged through a degraded tick), the Ok retry reloads and
/// un-gates, and a later sample-bearing tick persists again.
// r[verify ctrl.nodeclaim.lease-edge-polarity+4]
#[tokio::test]
async fn reload_err_gates_persist_and_retries_next_tick() {
    let mut lab = Lab::new().await;
    seed_previous_leader(&lab.db).await;
    let baseline = lab.pg_rows().await;
    lab.r.hooks.on_acquire();
    lab.r.pg = lab.closed_pool().await;

    // Degraded tick: LISTs run, persist is gated.
    lab.tick(0, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert!(lab.r.reload_pending());
    assert_eq!(lab.pg_rows().await, baseline, "stale overwrite prevented");

    // Pool restored: reload Ok, latch clears.
    lab.r.pg = lab.db.pool.clone();
    lab.tick(10, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert!(!lab.r.reload_pending(), "latch cleared on Ok retry");

    // A fresh Registered edge (inside the recency gate) records a
    // sample; the tick-end persist updates the rows.
    let fresh = nc_json("n-new", 0, Some(15));
    lab.tick(20, full_tick_scenario(vec![], vec![fresh], vec![]))
        .await;
    assert_eq!(lab.boot_samples(), 6, "5 reloaded + 1 fresh");
    assert_ne!(lab.pg_rows().await, baseline, "persist resumed");
}

/// R5+R6 cleanup-pending polarity: `prev_extra_cells` /
/// `prev_unplaced_extras` survive the acquire edge INTO the tick body
/// (the trailing zero-write happens — observed via a local
/// DebuggingRecorder), are consumed exactly once, and the next tick
/// writes nothing for the vanished cell.
// r[verify ctrl.nodeclaim.lease-edge-polarity+4]
#[test]
fn acquire_keeps_cleanup_sets_one_trailing_write_then_drop() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let extra = Cell("vanished-hw".into(), CapacityType::Spot);

    let mut lab = rt.block_on(Lab::new());
    lab.r.prev_extra_cells.insert(extra.clone());
    lab.r.prev_unplaced_extras.insert(extra.clone());
    lab.r.hooks.on_acquire();

    // Tick 1 under a local recorder: the acquire-Ok edge must NOT
    // clear the cleanup sets — the same tick's gauge pass consumes
    // them as one trailing zero-write each.
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        rt.block_on(lab.tick(0, full_tick_scenario(vec![], vec![], vec![])));
    });
    let label = extra.to_string();
    let wrote = |name: &str| {
        snapshotter
            .snapshot()
            .into_vec()
            .iter()
            .any(|(k, _, _, v)| {
                k.key().name() == name
                    && k.key().labels().any(|l| l.value() == label)
                    && matches!(v, DebugValue::Gauge(g) if g.0 == 0.0)
            })
    };
    assert!(
        wrote("rio_controller_nodeclaim_terminating_age_max_seconds"),
        "prev_extra_cells survived acquire → one trailing zero-write"
    );
    assert!(
        wrote("rio_controller_ffd_unplaced_cores"),
        "prev_unplaced_extras survived acquire → one trailing zero-write"
    );
    assert!(
        !lab.r.prev_extra_cells.contains(&extra) && !lab.r.prev_unplaced_extras.contains(&extra),
        "consumed exactly once (fields dropped the cell)"
    );

    // Tick 2 under a fresh recorder: the vanished cell gets NO write.
    let recorder2 = DebuggingRecorder::new();
    let snapshotter2 = recorder2.snapshotter();
    metrics::with_local_recorder(&recorder2, || {
        rt.block_on(lab.tick(10, full_tick_scenario(vec![], vec![], vec![])));
    });
    assert!(
        !snapshotter2
            .snapshot()
            .into_vec()
            .iter()
            .any(|(k, _, _, _)| k.key().labels().any(|l| l.value() == label)),
        "no write for the vanished cell after the trailing tick"
    );
}

/// R7 loss edge: `on_lose` unarms the gate on the SAME tick (before
/// the standby skip), with zero kube/admin/PG traffic.
// r[verify ctrl.nodeclaim.placeable-gate+5]
#[tokio::test]
async fn loss_unarms_gate_same_tick() {
    let mut lab = Lab::new().await;
    // Arm: a healthy tick publishes (Some(∅) is armed).
    lab.tick(0, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert!(lab.gate.snapshot().is_some(), "gate armed after FFD tick");

    lab.r.hooks.on_lose();
    lab.leader_flag.store(false, Ordering::SeqCst);
    lab.r.pg = lab.closed_pool().await; // would error if touched
    let acks_before = lab.ack_calls().len();
    lab.tick(10, Vec::new()).await; // empty queue: zero kube traffic

    assert!(lab.gate.snapshot().is_none(), "gate unarmed same tick");
    assert_eq!(lab.ack_calls().len(), acks_before, "no admin traffic");
}

/// R7 standby: a standby tick has no effects and freezes every
/// counter/field (empty verifier + closed pool prove "no traffic").
// r[verify ctrl.nodeclaim.lease-edge-polarity+4]
#[tokio::test]
async fn standby_tick_no_effect_and_frozen_counters() {
    let mut lab = Lab::new().await;
    lab.leader_flag.store(false, Ordering::SeqCst);
    lab.r.prev_idle.insert("n1".into(), 5.0);
    lab.r.recorded_boot.insert("n1".into());
    lab.r.consecutive_bot_ticks = 3;
    lab.r.tick_counter = 7;
    lab.r.pg = lab.closed_pool().await;

    lab.tick(0, Vec::new()).await;

    assert_eq!(lab.r.prev_idle.len(), 1);
    assert!(lab.r.recorded_boot.contains("n1"));
    assert_eq!(lab.r.consecutive_bot_ticks, 3, "streak frozen on standby");
    assert_eq!(lab.r.tick_counter, 7, "tick_counter frozen on standby");
}

/// R8 `consecutive_bot_ticks` (retain-safe class): frozen across
/// loss/standby/acquire, advanced only by leader ⊥ ticks, threshold 5
/// pinned structurally, reset on the first successful poll.
// r[verify ctrl.nodeclaim.consolidate-only-degraded+3]
#[tokio::test]
async fn bot_streak_frozen_across_acquire_resets_on_success() {
    let mut lab = Lab::new().await;
    lab.r.admin = admin_client(dead_channel());
    for t in [0u64, 10, 20] {
        lab.tick(t, bot_tick_scenario(vec![], vec![])).await;
    }
    assert_eq!(lab.r.consecutive_bot_ticks, 3);

    // Loss → standby tick: frozen.
    lab.r.hooks.on_lose();
    lab.leader_flag.store(false, Ordering::SeqCst);
    lab.tick(30, Vec::new()).await;
    assert_eq!(lab.r.consecutive_bot_ticks, 3, "frozen across loss");

    // Re-acquire (reload Ok against live PG): still frozen at 3; the
    // next two leader ⊥ ticks walk 4 → 5, and 5 enters
    // consolidate-only on that same tick.
    lab.leader_flag.store(true, Ordering::SeqCst);
    lab.r.hooks.on_acquire();
    lab.tick(40, bot_tick_scenario(vec![], vec![])).await;
    assert_eq!(
        lab.r.consecutive_bot_ticks, 4,
        "frozen across acquire, then +1"
    );
    lab.tick(50, consolidate_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert_eq!(
        lab.r.consecutive_bot_ticks, 5,
        "BOT_TICKS_BEFORE_CONSOLIDATE_ONLY=5 entered consolidate-only"
    );

    // First successful poll resets.
    lab.r.admin = admin_client(lab.admin_channel.clone());
    lab.tick(60, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert_eq!(lab.r.consecutive_bot_ticks, 0, "reset on success");
}

/// R9 wedge (retain-safe class): expiry evidence fed through the real
/// tick survives the acquire edge and still drives a Dead reap one
/// tick later; `tick_counter` advances on every leader tick.
///
/// merged_bug_024 restructure: the node's claim is REGISTERED from
/// tick 1 — the admission authority refuses (and tombstones)
/// fleet-absent attribution, so the retired shape of this test
/// (evidence admitted while NO claim existed, claim materializing
/// later) is now inadmissible-by-law, not retained-by-luck. The R9
/// property pinned is sharper: HALF the wedge pair predates the
/// acquire edge, so the tick-2 reap proves the pre-acquire anchor was
/// retained (without retention, one in-window anchor cannot reap).
// r[verify ctrl.nodeclaim.lease-edge-polarity+4]
#[tokio::test]
async fn wedge_evidence_survives_acquire() {
    let mut lab = Lab::new().await;
    let attempt = |intent: &str| OpenAttempt {
        intent_id: intent.into(),
        source_node: "node-c-w".into(),
        deadline_secs: 60,
        assigned_at_age_secs: 120,
        attempt_kind: rio_proto::types::AttemptKind::Build as i32,
        ..Default::default()
    };
    let claim = nc_json("c-w", 0, Some(1));
    lab.set_open_attempts(vec![attempt("drv-a")]);
    let tc0 = lab.r.tick_counter;
    // Tick 1: the claim is registered; ONE expiry anchors (below the
    // cluster threshold — no reap yet).
    lab.tick(0, full_tick_scenario(vec![], vec![claim.clone()], vec![]))
        .await;

    // Acquire edge; the second distinct derivation expires after it.
    lab.r.hooks.on_acquire();
    lab.set_open_attempts(vec![attempt("drv-a"), attempt("drv-b")]);

    // Tick 2: the pair completes → classify(Dead) from evidence whose
    // FIRST anchor predates the acquire → scripted DELETE.
    lab.tick(
        30,
        full_tick_scenario(
            vec![],
            vec![claim],
            vec![delete_scenario("/apis/karpenter.sh/v1/nodeclaims/c-w")],
        ),
    )
    .await;
    assert_eq!(lab.r.tick_counter, tc0 + 2, "tick_counter leader-monotonic");
}

/// W11-AG (bug_042 — the tombstone-consumer half of
/// `ctrl.pool.delete-outcome`): a Dead reap whose DELETE errs non-404
/// but committed server-side tombstones the attempt (W9-BB plane) —
/// and because the claim is REGISTERED, the vanish fold (which only
/// consumes `inflight_created` exits) never consults it. Pre-fix the
/// tombstone expired UNCONSUMED: the REQUIRED `reaped_nodes` wedge
/// eviction never fired (the dead node's expiry evidence stayed
/// wedge-admissible the whole ~60-90s finalizer window —
/// `registered_fleet` has no terminating exclusion) and
/// `reaped_total{reason=dead}` permanently undercounted. Post-fix the
/// registered-tombstone sweep matches the terminating observation and
/// applies the FULL original consequence within the window.
// r[verify ctrl.pool.delete-outcome]
#[test]
fn err_committed_dead_reap_consequence_fires_within_finalizer_window() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let mut lab = rt.block_on(Lab::new());
    let attempt = |intent: &str| OpenAttempt {
        intent_id: intent.into(),
        source_node: "node-c-w".into(),
        deadline_secs: 60,
        assigned_at_age_secs: 120,
        attempt_kind: rio_proto::types::AttemptKind::Build as i32,
        ..Default::default()
    };
    let claim = nc_json("c-w", 0, Some(1));

    // Tick 1: one expiry anchors (below the cluster threshold).
    lab.set_open_attempts(vec![attempt("drv-a")]);
    rt.block_on(lab.tick(0, full_tick_scenario(vec![], vec![claim.clone()], vec![])));

    // Tick 2: the pair completes → classify(Dead) → the DELETE errs
    // 503 AFTER the apiserver committed it → tombstone (Dead, with
    // the backing node carried in the consequence packet).
    lab.set_open_attempts(vec![attempt("drv-a"), attempt("drv-b")]);
    rt.block_on(lab.tick(
        30,
        full_tick_scenario(
            vec![],
            vec![claim],
            vec![Scenario::k8s_error(
                Method::DELETE,
                "/apis/karpenter.sh/v1/nodeclaims/c-w",
                503,
                "ServiceUnavailable",
                "etcd leader changed",
            )],
        ),
    ));
    assert!(
        lab.r.delete_tombstones.contains("c-w"),
        "the ambiguous Dead attempt's provenance survives the tick"
    );
    assert!(
        lab.r.pending_wedge_evictions.is_empty(),
        "no consequence before confirmation"
    );

    // Tick 3: the commit materialized — the claim is observed
    // REGISTERED + terminating (the ~60-90s finalizer). classify
    // skips terminating claims and the vanish fold never consults
    // registered names: ONLY the registered-tombstone sweep can
    // adjudicate this observation.
    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        rt.block_on(lab.tick(
            40,
            full_tick_scenario(
                vec![],
                vec![nc_json_registered_terminating("c-w", 0, 1)],
                vec![],
            ),
        ));
    });

    assert!(
        lab.r.pending_wedge_evictions.contains("node-c-w"),
        "the REQUIRED wedge eviction fires within the finalizer window \
         (bug_042 red: the tombstone expired unconsumed and the dead \
         node's evidence stayed admissible): {:?}",
        lab.r.pending_wedge_evictions
    );
    // ppppp: snapshot exactly once.
    let snap = snapshotter.snapshot().into_vec();
    let dead_count = snap.into_iter().find_map(|(k, _, _, v)| {
        let key = k.key();
        (key.name() == "rio_controller_nodeclaim_reaped_total"
            && key
                .labels()
                .any(|l| l.key() == "reason" && l.value() == "dead"))
        .then_some(v)
    });
    assert_eq!(
        dead_count,
        Some(DebugValue::Counter(1)),
        "reaped_total{{reason=dead}} counts the confirmed reap \
         (bug_042 red: the permanent undercount)"
    );
    assert!(
        lab.r.delete_tombstones.is_empty(),
        "the confirmed exit consumed the tombstone"
    );
}

/// R10 (r43 bug_023 close): consolidate-only runs the SHARED kube-only
/// observation block — idle→busy pruning with the uncensored gap,
/// in-window Registered samples with the clear DISCARDED — and reaps
/// idle past threshold with `placeable` empty.
// r[verify ctrl.nodeclaim.consolidate-only-degraded+3]
#[tokio::test]
async fn consolidate_only_runs_kube_observations_and_reaps_with_empty_placeable() {
    let mut lab = Lab::new().await;
    lab.r.consecutive_bot_ticks = 5;
    lab.r.admin = admin_client(dead_channel());
    let t = 1000u64;
    // n-busy: idle since t-50, now busy (pod bound) → prune + uncensored gap.
    // n-new: Registered at t-5 (inside the 30s gate) → sample, no clear.
    // n-idle: idle since t-5000 (> the 300s policy floor) → reaped.
    lab.r
        .prev_idle
        .insert("n-busy".into(), (T0 + t - 50) as f64);
    lab.r
        .prev_idle
        .insert("n-idle".into(), (T0 + t - 5000) as f64);

    lab.tick(
        t,
        consolidate_tick_scenario(
            vec![pod_json("p1", "node-n-busy", 4)],
            vec![
                nc_json("n-busy", 0, Some(10)),
                nc_json("n-new", 900, Some(t - 5)),
                nc_json("n-idle", 0, Some(10)),
            ],
            vec![delete_scenario("/apis/karpenter.sh/v1/nodeclaims/n-idle")],
        ),
    )
    .await;

    assert!(!lab.r.prev_idle.contains_key("n-busy"), "idle→busy pruned");
    let gaps = lab.idle_gaps();
    assert!(
        gaps.iter()
            .any(|g| !g.censored && (g.gap_secs - 50.0).abs() < 1.0),
        "uncensored 50s gap recorded; got {gaps:?}"
    );
    assert!(
        gaps.iter().any(|g| g.censored),
        "the reap recorded its censored gap"
    );
    assert_eq!(lab.boot_samples(), 1, "in-window Registered edge sampled");
    assert!(lab.r.recorded_boot.contains("n-new"));
    assert!(
        lab.ack_calls().is_empty(),
        "clears DISCARDED in consolidate-only (no admin traffic at all)"
    );
}

/// W11-AF (bug_112, the 404-parity row of `ctrl.pool.delete-outcome`):
/// an idle reap whose DELETE returns 404 (Karpenter GC raced the
/// controller) discharges the FULL `Ok` consequence — the censored
/// `IdleGapEvent`, the `reaped_total{reason=idle}` counter, and the
/// backing-node wedge-eviction feed — exactly like
/// `health::reap_unhealthy`'s 404 arm. Pre-fix the lane's 404 arm was
/// `=> {}`: the eviction feed, the counter, and the censored sample
/// all silently skipped for the ~60-90s finalizer window.
// r[verify ctrl.pool.delete-outcome]
#[test]
fn idle_reap_404_discharges_the_full_ok_consequence() {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("rt");
    let mut lab = rt.block_on(Lab::new());
    lab.r.consecutive_bot_ticks = 5;
    {
        // dead_channel's lazy connector spawns onto the ambient
        // reactor — enter the rt for the construction only.
        let _e = rt.enter();
        lab.r.admin = admin_client(dead_channel());
    }
    let t = 1000u64;
    // n-idle: idle since t-5000 (> the 300s policy floor) → reap fires;
    // the DELETE comes back 404 (GC raced).
    lab.r
        .prev_idle
        .insert("n-idle".into(), (T0 + t - 5000) as f64);

    let recorder = DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    metrics::with_local_recorder(&recorder, || {
        rt.block_on(lab.tick(
            t,
            consolidate_tick_scenario(
                vec![],
                vec![nc_json("n-idle", 0, Some(10))],
                vec![Scenario::k8s_error(
                    Method::DELETE,
                    "/apis/karpenter.sh/v1/nodeclaims/n-idle",
                    404,
                    "NotFound",
                    "nodeclaims.karpenter.sh \"n-idle\" not found",
                )],
            ),
        ));
    });

    // The full Ok consequence, all three halves (the red asserts the
    // halves the pre-fix `=> {}` arm dropped):
    assert!(
        lab.idle_gaps().iter().any(|g| g.censored),
        "404 reap records its censored gap (arm parity)"
    );
    assert!(
        lab.r.pending_wedge_evictions.contains("node-n-idle"),
        "404 reap feeds the wedge eviction stash (arm parity): {:?}",
        lab.r.pending_wedge_evictions
    );
    // ppppp: snapshot exactly once.
    let snap = snapshotter.snapshot().into_vec();
    let idle_count = snap.into_iter().find_map(|(k, _, _, v)| {
        let key = k.key();
        (key.name() == "rio_controller_nodeclaim_reaped_total"
            && key
                .labels()
                .any(|l| l.key() == "reason" && l.value() == "idle"))
        .then_some(v)
    });
    assert_eq!(
        idle_count,
        Some(DebugValue::Counter(1)),
        "404 reap increments reaped_total{{reason=idle}} (arm parity)"
    );
    // The completed reap left no tombstone (it is not ambiguous).
    assert!(
        lab.r.delete_tombstones.is_empty(),
        "a completed 404 reap carries no provenance obligation"
    );
}

/// W11-AF sibling (the third arm): an idle DELETE erring non-404 is
/// AMBIGUOUS — the lane tombstones the attempt (reason `Idle`, with
/// its consequence packet) and applies NO consequence yet. Pre-fix
/// the Err arm was warn-only: a committed-but-errored idle delete
/// lost its provenance entirely (the bug_042 shape on a second lane).
// r[verify ctrl.pool.delete-outcome]
#[tokio::test]
async fn idle_reap_ambiguous_err_tombstones_the_attempt() {
    let mut lab = Lab::new().await;
    lab.r.consecutive_bot_ticks = 5;
    lab.r.admin = admin_client(dead_channel());
    let t = 1000u64;
    lab.r
        .prev_idle
        .insert("n-idle".into(), (T0 + t - 5000) as f64);

    lab.tick(
        t,
        consolidate_tick_scenario(
            vec![],
            vec![nc_json("n-idle", 0, Some(10))],
            vec![Scenario::k8s_error(
                Method::DELETE,
                "/apis/karpenter.sh/v1/nodeclaims/n-idle",
                503,
                "ServiceUnavailable",
                "etcd leader changed",
            )],
        ),
    )
    .await;

    assert!(
        lab.r.delete_tombstones.contains("n-idle"),
        "the ambiguous idle attempt's provenance survives the tick"
    );
    assert_eq!(
        lab.r.delete_tombstones.reason("n-idle"),
        Some(health::ReapReason::Idle),
        "the tombstone carries the lane's own reason letter"
    );
    assert!(
        !lab.r.pending_wedge_evictions.contains("node-n-idle"),
        "no consequence before confirmation (the delete may not have committed)"
    );
    assert!(
        !lab.idle_gaps().iter().any(|g| g.censored),
        "no censored sample before confirmation"
    );
}

/// FIX-T13 (⊥-tick close, boot half): a Registered edge INSIDE a
/// pre-threshold outage window is observed on the ⊥ tick (5s ≤ the
/// 30s recency gate) and its sample recorded; an edge first observed
/// only at recovery (35s stale) stays record-only — both boundary
/// sides pinned in one test. RED pre-fix: the ⊥ arm skips all
/// observations, so c1's edge ages past the gate and the sample is
/// lost (`boot_samples == 0`).
// r[verify ctrl.nodeclaim.consolidate-only-degraded+3]
#[tokio::test]
async fn bot_tick_records_registered_edge_inside_window() {
    let mut lab = Lab::new().await;
    // t=0: c1 in-flight, listed.
    lab.tick(
        0,
        full_tick_scenario(vec![], vec![nc_json("c1", 0, None)], vec![]),
    )
    .await;
    lab.r.admin = admin_client(dead_channel());

    // c1 registers at t=15; the ⊥ ticks at 20/30/40 can observe it.
    let c1 = || nc_json("c1", 0, Some(15));
    for t in [20u64, 30, 40] {
        lab.tick(t, bot_tick_scenario(vec![], vec![c1()])).await;
    }
    assert_eq!(
        lab.boot_samples(),
        1,
        "the in-window edge (5s ≤ 3×TICK gate) records ON the ⊥ tick"
    );
    assert!(lab.r.recorded_boot.contains("c1"));

    // Recovery at t=50: c2's edge (also t=15) is first observed 35s
    // stale → record-only, no second sample and no clear for IT. c1's
    // in-window edge was consumed on a ⊥ tick — its ICE-clear is
    // BUFFERED (merged_bug_007) and ships on this recovery Ack.
    lab.r.admin = admin_client(lab.admin_channel.clone());
    lab.tick(
        50,
        full_tick_scenario(vec![], vec![c1(), nc_json("c2", 0, Some(15))], vec![]),
    )
    .await;
    assert_eq!(lab.boot_samples(), 1, "stale edge stays record-only");
    assert!(lab.r.recorded_boot.contains("c2"));
    assert!(
        lab.ack_calls()
            .iter()
            .any(|a| carries(&a.registered_cells, &cell())),
        "the ⊥-tick edge's buffered ICE-clear ships on the recovery Ack \
         (pre-buffer this asserted all-empty: the discard was the pin)"
    );
}

/// FIX-T14 (⊥-tick close, idle half — the idleConflationRun trace):
/// an idle→busy→idle cycle DURING a pre-threshold outage prunes and
/// re-seeds `prev_idle` on the ⊥ ticks, so the threshold-crossing
/// recovery tick does NOT over-reap the fresh idle spell (no DELETE
/// is scripted — an over-reap fails the scenario match). RED pre-fix:
/// the cycle is unobserved, `prev_idle` keeps the t=0 seed
/// (over-estimate), and the assertions on the re-seeded timestamp and
/// the uncensored gap fail.
// r[verify ctrl.nodeclaim.consolidate-only-degraded+3]
#[tokio::test]
async fn bot_tick_prunes_idle_to_busy() {
    let mut lab = Lab::new().await;
    let n = || nc_json("n-i", 0, Some(1)); // stale edge: record-only
    // t=0: n-i idle → seeded.
    lab.tick(0, full_tick_scenario(vec![], vec![n()], vec![]))
        .await;
    assert_eq!(lab.r.prev_idle.get("n-i").copied(), Some((T0) as f64));

    lab.r.admin = admin_client(dead_channel());
    // ⊥ t=10: busy → prune + uncensored 10s gap.
    lab.tick(
        10,
        bot_tick_scenario(vec![pod_json("p1", "node-n-i", 4)], vec![n()]),
    )
    .await;
    // ⊥ t=20: idle again → re-seeded at t=20.
    lab.tick(20, bot_tick_scenario(vec![], vec![n()])).await;

    // Recovery at t=310: idle basis is t=20 (290s < the 300s floor) —
    // NO DELETE scripted; a pre-fix over-reap (basis t=0, 310s) would
    // issue one and fail the scenario match.
    lab.r.admin = admin_client(lab.admin_channel.clone());
    lab.tick(310, full_tick_scenario(vec![], vec![n()], vec![]))
        .await;

    assert_eq!(
        lab.r.prev_idle.get("n-i").copied(),
        Some((T0 + 20) as f64),
        "re-seeded on the ⊥ tick (fresh spell, not the pre-outage one)"
    );
    let gaps = lab.idle_gaps();
    assert!(
        gaps.iter()
            .any(|g| !g.censored && (g.gap_secs - 10.0).abs() < 1.0),
        "uncensored busy-edge gap recorded on the ⊥ tick; got {gaps:?}"
    );
    assert!(gaps.iter().all(|g| !g.censored), "no reap, no censored gap");
}

/// FIX-T15: the fixed ⊥ arm performs exactly `[Pods LIST, NodeClaims
/// LIST]` — no create/reap/ack/publish — pinning the fix's wire cost
/// forever (the verifier rejects anything else).
// r[verify ctrl.nodeclaim.consolidate-only-degraded+3]
#[tokio::test]
async fn bot_tick_makes_exactly_two_lists_no_effects() {
    let mut lab = Lab::new().await;
    lab.r.admin = admin_client(dead_channel());
    // An idle registered node + a stale-ish prev_idle entry: even so,
    // a ⊥ tick must not reap (observations only, no effects).
    lab.r.prev_idle.insert("n-i".into(), (T0) as f64);
    lab.tick(
        400,
        bot_tick_scenario(vec![], vec![nc_json("n-i", 0, Some(1))]),
    )
    .await;
    assert!(
        lab.gate.snapshot().is_none(),
        "placeable gate not (re)published on a ⊥ tick"
    );
    assert!(lab.ack_calls().is_empty(), "no ack attempted");
    assert_eq!(lab.r.consecutive_bot_ticks, 1);
}

// r[verify sched.snapshot.binding-presence]
/// bug_285 controller half: `report_unfulfillable` ALWAYS attaches the
/// binding snapshot — even on the all-empty tick (pre-fix: the
/// all-four-empty early return suppressed exactly the scale-to-zero
/// tick, so the scheduler never heard "zero bound pods" and kept its
/// stale map for the whole idle window). The legacy field 5 is never
/// dual-written (R9).
#[tokio::test(flavor = "multi_thread")]
async fn report_unfulfillable_always_ships_the_snapshot() {
    let mut lab = Lab::new().await;

    // The scale-to-zero shape: nothing ICE'd, nothing registered, no
    // observations, zero bound pods.
    lab.r
        .report_unfulfillable(vec![], vec![])
        .await
        .expect("ack sent");

    let acks = lab.ack_calls();
    assert_eq!(acks.len(), 1, "the all-empty tick still Acks");
    let snap = acks[0]
        .binding_snapshot
        .as_ref()
        .expect("the snapshot is PRESENT (empty ≠ absent)");
    assert!(snap.bound.is_empty(), "present-and-empty = clear");
    assert!(
        acks[0].bound_intents.is_empty(),
        "legacy field 5 is never dual-written (R9)"
    );

    // A bound pod travels inside the snapshot, not field 5.
    lab.r
        .report_unfulfillable(
            vec![rio_proto::types::BoundIntent {
                intent_id: "drv-285".into(),
                node_name: "node-1".into(),
                deadline_secs: 0,
            }],
            vec![],
        )
        .await
        .expect("ack sent");
    let acks = lab.ack_calls();
    let snap = acks[1].binding_snapshot.as_ref().expect("present");
    assert_eq!(snap.bound.len(), 1);
    assert_eq!(snap.bound[0].intent_id, "drv-285");
    assert!(acks[1].bound_intents.is_empty());
}

// r[verify ctrl.nodeclaim.evidence-buffered]
/// merged_bug_007's recorded red, kept as the regression pin: a
/// pre-threshold ⊥ tick consumes a fresh Registered edge
/// (`recorded_boot` marks it — consume-once) but its ICE-clear is
/// scheduler-bound; pre-fix the `let _ =` discarded it permanently and
/// the recovery Ack shipped nothing. The `#[must_use]` buffer drains
/// it into the next healthy Ack.
#[tokio::test]
async fn bot_tick_registered_edge_survives_to_recovery_ack() {
    let mut lab = Lab::new().await;
    lab.r.admin = admin_client(dead_channel());

    // Pre-threshold ⊥ tick (streak 1): a FRESH Registered edge is
    // observed kube-only. The boot sample records locally — but the
    // ICE-clear (registered cell) is scheduler-bound evidence.
    lab.tick(
        600,
        bot_tick_scenario(vec![], vec![nc_json("n-fresh", 0, Some(595))]),
    )
    .await;
    assert!(
        lab.r.recorded_boot.contains("n-fresh"),
        "the edge was consumed on the ⊥ tick"
    );

    // Recovery: the buffered ICE-clear must ship on the next healthy
    // Ack — the edge is consume-once and cannot re-fire.
    lab.r.admin = admin_client(lab.admin_channel.clone());
    lab.tick(610, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert!(
        lab.ack_calls()
            .iter()
            .any(|a| carries(&a.registered_cells, &cell())),
        "the ⊥-tick Registered edge's ICE-clear must survive to the recovery Ack"
    );
}

// r[verify ctrl.nodeclaim.evidence-buffered]
/// The buffer is cleared on the lease-acquire edge (suppress
/// polarity): evidence captured under a previous tenure must not ship
/// a stale ICE-clear after re-acquisition.
#[tokio::test]
async fn pending_evidence_cleared_on_acquire_edge() {
    let mut lab = Lab::new().await;
    let mut pre_tenure = TickEvidence::default();
    pre_tenure.buffer_clears([cell()]);
    lab.r.pending_evidence.merge(pre_tenure);
    lab.r.hooks.on_acquire();
    lab.tick(600, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert!(
        lab.ack_calls()
            .iter()
            .all(|a| a.registered_cells.is_empty()),
        "stale pre-tenure evidence must not ship after acquire (Ok-arm clear, \
         suppress polarity — same class as recorded_boot)"
    );
}

// r[verify ctrl.nodeclaim.acquire-edge-token+1]
/// bug_346's recorded red, kept as the regression pin: an idle spell
/// seeded during a reload-Err loop must SURVIVE subsequent Err ticks —
/// pre-fix the boolean latch re-ran `prev_idle.clear()` every tick of
/// a PG outage ("left: Some(...220.0) / right: Some(...200.0)" — the
/// seed restarted every tick), disabling idle consolidation entirely
/// while the under-reap contract claimed one cycle.
#[tokio::test]
async fn idle_spell_survives_reload_err_loop() {
    let mut lab = Lab::new().await;
    lab.r.hooks.on_acquire();
    lab.r.pg = lab.closed_pool().await;

    // Tick 1 (reload Err): n1 is idle → seeded into prev_idle at t=600.
    let idle_nc = || nc_json("n1", 0, Some(10));
    lab.tick(600, full_tick_scenario(vec![], vec![idle_nc()], vec![]))
        .await;
    let seeded = lab.r.prev_idle.get("n1").copied();
    assert!(seeded.is_some(), "idle spell seeded on the first Err tick");

    // Ticks 2-3 (still Err): the seed must SURVIVE — the acquire-edge
    // clear fires once per acquisition, not once per Err retry.
    lab.tick(610, full_tick_scenario(vec![], vec![idle_nc()], vec![]))
        .await;
    lab.tick(620, full_tick_scenario(vec![], vec![idle_nc()], vec![]))
        .await;
    assert_eq!(
        lab.r.prev_idle.get("n1").copied(),
        seeded,
        "the idle spell survives the reload-Err loop (the clear is an \
         acquire-EDGE action, not a per-tick action)"
    );
}

// r[verify ctrl.nodeclaim.evidence-ack-latch+3]
/// merged_bug_045 (commit-on-Ack): buffered kube-only evidence
/// survives an Ack failure and ships on the next successful Ack.
/// Recorded red (pre-fix): the buffer was mem::take'n into the wire
/// shapes BEFORE the RPC — the failed-Ack tick lost the batch, and
/// tick 2's Ack carried empty registered_cells.
#[tokio::test(flavor = "multi_thread")]
async fn evidence_survives_ack_failure_until_committed() {
    let mut lab = Lab::new().await;

    // Tick 1: n1 fresh-Registered inside the recency window → a
    // registered-cell ICE-clear enters the buffer; the Ack fails.
    lab.admin
        .fail_next_ack
        .store(true, std::sync::atomic::Ordering::SeqCst);
    lab.tick(
        600,
        full_tick_scenario(vec![], vec![nc_json("n1", 0, Some(595))], vec![]),
    )
    .await;
    let acks = lab.ack_calls();
    assert_eq!(acks.len(), 1, "tick 1 attempted exactly one Ack");
    assert!(
        carries(&acks[0].registered_cells, &cell()),
        "the failed Ack carried the payload to the wire"
    );

    // Tick 2: nothing new observed (n1 edge already recorded). The
    // retained buffer MUST re-ship.
    lab.tick(
        610,
        full_tick_scenario(vec![], vec![nc_json("n1", 0, Some(595))], vec![]),
    )
    .await;
    let acks = lab.ack_calls();
    assert_eq!(acks.len(), 2, "tick 2 acked");
    assert!(
        carries(&acks[1].registered_cells, &cell()),
        "evidence lost on Ack-Err: tick 2 must re-ship the buffered \
         ICE-clear (registered_cells: {:?})",
        acks[1].registered_cells
    );

    // Tick 3: the Ack-Ok committed the buffer — nothing re-ships.
    lab.tick(
        620,
        full_tick_scenario(vec![], vec![nc_json("n1", 0, Some(595))], vec![]),
    )
    .await;
    let acks = lab.ack_calls();
    assert_eq!(acks.len(), 3);
    assert!(
        acks[2].registered_cells.is_empty(),
        "committed evidence must not re-ship forever: {:?}",
        acks[2].registered_cells
    );
}

/// bug_082: the ICE-mark plane (`unfulfillable_cells`) is consume-once
/// at its producers (`record_reap` fires at claim delete;
/// `detect_vanished` removes the tracking entry as it emits) — a
/// failed `AckSpawnedIntents` must retain it for the next tick exactly
/// like its sibling planes (registered_cells, observed_types). The
/// pre-fix Err arm warned "buffered evidence retained" while the mark,
/// built from the consumed parameter, was already gone.
// r[verify ctrl.nodeclaim.evidence-ack-latch+3]
#[tokio::test]
async fn ice_mark_survives_ack_failure() {
    let mut lab = Lab::new().await;
    lab.admin.fail_next_ack.store(true, Ordering::SeqCst);

    // Tick 1: a Launched=False/LaunchFailed claim is ICE-reaped
    // (scripted DELETE) -> record_reap produces the mark -> the Ack
    // carrying it FAILS (programmed).
    lab.tick(
        0,
        full_tick_scenario(
            vec![],
            vec![nc_json_ice("c-ice", 0)],
            vec![delete_scenario("/apis/karpenter.sh/v1/nodeclaims/c-ice")],
        ),
    )
    .await;
    let first = lab.ack_calls();
    assert!(
        first
            .last()
            .is_some_and(|a| carries(&a.unfulfillable_cells, &cell())),
        "tick-1 ack must carry the fresh mark (and fail): {first:?}"
    );

    // Tick 2: nothing new happens — the retained mark must ship again.
    lab.tick(10, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    let acks = lab.ack_calls();
    let last = acks.last().expect("tick-2 ack");
    assert!(
        carries(&last.unfulfillable_cells, &cell()),
        "ICE mark dropped on Ack failure: tick-2 unfulfillable_cells = {:?}",
        last.unfulfillable_cells
    );
}

// r[verify ctrl.nodeclaim.placement-outcome+1]
/// live_051(c) red R26 / witness W7-Y — certifies: *a `NoHostingClass`
/// intent produces an ack whose `rejected` entry carries its
/// `intent_id` + reason + actionable detail — through the production
/// cover fold to the WIRE ARTIFACT* (the R4-A form), with
/// kill-isolation: a masked-population intent produces ZERO `rejected`
/// entries (the surviving no-wire half of the WO-S7-3 derivation).
///
/// Pre-fix red (verbatim): `left: ack carries spawned/
/// unfulfillable_cells only; the drop is tally-only; the scheduler
/// never learns (rejected == []) / right: rejected == [{drv-unhostable,
/// NO_HOSTING_CLASS, detail naming the configured classes}]`. The live
/// measurement this kills: the drv re-emits Ready forever at
/// ~25 drops/min while cover.rs:209 counts and nobody answers.
#[tokio::test]
async fn no_hosting_class_drop_answers_a_typed_verdict() {
    let mut lab = Lab::new().await;
    // Global ceilings loaded (so cover_deficit runs), ZERO usable
    // hosting class for the riscv intent — the config-gap population.
    // One decoy class is configured so the detail string's
    // configured-classes census is non-empty (operator-actionable).
    let mut classes = std::collections::HashMap::new();
    classes.insert(
        "mid-ebs-x86".to_string(),
        rio_proto::types::HwClassLabels {
            labels: vec![rio_proto::types::NodeLabelMatch {
                key: "kubernetes.io/arch".into(),
                value: "amd64".into(),
            }],
            ..Default::default()
        },
    );
    lab.r.hw_config.set(classes, (192, 768 << 30));
    *lab.admin.spawn_intents.write().unwrap() = rio_proto::types::GetSpawnIntentsResponse {
        intents: vec![
            // Unhostable: no class admits riscv64 (config gap) → the
            // verdict population.
            rio_proto::types::SpawnIntent {
                intent_id: "drv-unhostable".into(),
                cores: 4,
                mem_bytes: 1 << 30,
                system: "riscv64-none".into(),
                ready: Some(true),
                ..Default::default()
            },
            // Masked-ready: hosting cell exists but is ICE-masked →
            // counted as ready_all_cells_ice_masked, NEVER a verdict
            // (the kill-isolation population).
            rio_proto::types::SpawnIntent {
                intent_id: "drv-masked".into(),
                cores: 4,
                mem_bytes: 1 << 30,
                system: "x86_64-linux".into(),
                ready: Some(true),
                hw_class_names: vec!["mid-ebs-x86".into()],
                node_affinity: vec![rio_proto::types::NodeSelectorTerm {
                    match_expressions: vec![rio_proto::types::NodeSelectorRequirement {
                        key: "karpenter.sh/capacity-type".into(),
                        operator: "In".into(),
                        values: vec!["spot".into()],
                    }],
                }],
                ..Default::default()
            },
        ],
        // The scheduler's own mask covers drv-masked's only cell —
        // information the scheduler already owns, hence no verdict.
        ice_masked_cells: vec!["mid-ebs-x86:spot".into()],
        ..Default::default()
    };
    // The pool-coverage axis must admit the intents (an uncovered
    // intent drops as `no_pool_covers` BEFORE the cover fold — a
    // different, pre-existing outcome): one Builder pool covering
    // both systems.
    let pools = serde_json::json!({
        "apiVersion": "rio.build/v1alpha1",
        "kind": "Pool",
        "metadata": {"name": "builders", "namespace": "rio"},
        "spec": {
            "kind": "Builder",
            "maxConcurrent": 10,
            "features": [],
            "systems": ["riscv64-none", "x86_64-linux"],
            "image": "x",
        },
    });
    let scenarios = vec![
        Scenario::ok(
            Method::GET,
            "/apis/rio.build/v1alpha1/pools",
            list("PoolList", "rio.build/v1alpha1", vec![pools]),
        ),
        Scenario::ok(Method::GET, "/api/v1/pods", pod_list(vec![])),
        Scenario::ok(
            Method::GET,
            "/apis/karpenter.sh/v1/nodeclaims",
            nc_list(vec![]),
        ),
    ];
    lab.tick(0, scenarios).await;
    let acks = lab.ack_calls();
    let with_verdict: Vec<_> = acks.iter().filter(|a| !a.rejected.is_empty()).collect();
    assert_eq!(
        with_verdict.len(),
        1,
        "exactly one ack carries the verdicts: {acks:?}"
    );
    let rejected = &with_verdict[0].rejected;
    assert_eq!(
        rejected.len(),
        1,
        "ONLY the config-gap intent: {rejected:?}"
    );
    assert_eq!(rejected[0].intent_id, "drv-unhostable");
    assert_eq!(
        rejected[0].reason,
        i32::from(rio_proto::types::IntentVerdictReason::NoHostingClass)
    );
    assert!(
        rejected[0].detail.contains("riscv64-none") && rejected[0].detail.contains("mid-ebs-x86"),
        "detail names the unmatched system and the configured classes: {}",
        rejected[0].detail
    );
    assert!(
        !rejected.iter().any(|v| v.intent_id == "drv-masked"),
        "masked population stays off the wire (kill-isolation)"
    );
}

// r[verify ctrl.nodeclaim.ice-mark-clear+5]
/// live_050(b) red R4-A / W7-D leg A — certifies: *never-registered-
/// terminating transit through the production retain + reconcile ships
/// the mark AT THE WIRE ARTIFACT* — `AckSpawnedIntentsRequest.
/// unfulfillable_cells` carries the encoded cell event. Pre-fix red
/// (verbatim): `left: unfulfillable_cells == [] (no mark shipped; the
/// Terminating arm misread launch-failure teardown as deliberate) /
/// right: carries the encoded cell event`. Leg B (the scheduler-side
/// consumption pin, R4-B) lives at actor/tests/sla_contract.rs over
/// the shared `rio_common::cell_wire` codec — the pinned-composition
/// convention (W7-D).
#[tokio::test]
async fn never_registered_vanish_ships_the_mark_on_the_wire() {
    let mut lab = Lab::new().await;
    // cover_deficit created it last tick; never observed Registered.
    lab.r.inflight_created.insert("nc-doomed".into(), cell());
    // This tick observes it TERMINATING without ever Registering
    // (Karpenter terminal launch failure mid-finalize).
    lab.tick(
        600,
        full_tick_scenario(vec![], vec![nc_json_terminating("nc-doomed", 595)], vec![]),
    )
    .await;
    let last = lab.ack_calls().pop().expect("tick acked");
    assert!(
        carries(&last.unfulfillable_cells, &cell()),
        "launch-failure teardown ships the mark on the wire: {:?}",
        last.unfulfillable_cells
    );
    assert!(
        !lab.r.inflight_created.contains_key("nc-doomed"),
        "exited tracking through the typed exit alphabet"
    );
}
