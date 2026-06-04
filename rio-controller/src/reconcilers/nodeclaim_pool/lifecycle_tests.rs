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
        rio_auth::hmac::ServiceTokenInterceptor::new(None, "rio-controller"),
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
// r[verify ctrl.nodeclaim.lease-edge-polarity+3]
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
        "suppress: cleared on Ok (live in-flight claim no longer tracked)"
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
            .any(|a| a.registered_cells.contains(&cell().to_string())),
        "fresh in-window edge ships its ICE-clear"
    );
}

/// R1 acquire-Err arm (r43 merged_bug_016, the m1CalibAcquireClearOkOnly
/// shape): `prev_idle` STILL clears; the suppress fields are retained;
/// the latch holds (persist stays gated). Live claims make it
/// non-vacuous: n1 is FRESH-registered and already in `recorded_boot`
/// — a wrongful Err-arm clear would re-edge it into a sample + an
/// ICE-clear ack; n9 is live in-flight — a wrongful clear would
/// untrack it for good (nothing re-adds an untracked live claim).
// r[verify ctrl.nodeclaim.lease-edge-polarity+3]
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
        lab.r.inflight_created.contains_key("n9"),
        "suppress: retained on Err"
    );
    assert!(lab.r.reload_pending(), "latch held for retry");
}

/// R2 recency gate: a stale (>3×TICK) Registered edge after the
/// acquire clear is recorded WITHOUT a sample and WITHOUT an ICE-clear
/// on the wire (noMassClearAfterFailover / m34CalibNoRecencyGate).
// r[verify ctrl.nodeclaim.ice-mark-clear]
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
// r[verify ctrl.nodeclaim.inflight-conservation+2]
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
    // re-read the absence as Karpenter GC (no unfulfillable_cells).
    lab.r.admin = admin_client(lab.admin_channel.clone());
    lab.tick(10, full_tick_scenario(vec![], vec![], vec![]))
        .await;
    assert_eq!(lab.r.consecutive_bot_ticks, 0);
    assert!(
        lab.ack_calls()
            .iter()
            .all(|a| a.unfulfillable_cells.is_empty()),
        "no spurious ICE mark for the controller's own reap"
    );
}

/// R3 conservation, vanish arm (r40 bug_020): a tracked claim absent
/// from live (Karpenter GC) marks its cell; a tracked claim still
/// in-flight stays tracked.
// r[verify ctrl.nodeclaim.inflight-conservation+2]
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
            .any(|a| a.unfulfillable_cells.contains(&cell_gone.to_string())),
        "vanish marked its cell on the wire; acks: {acks:?}"
    );
}

/// R4 reload latch: a closed-pool acquire gates persist (PG rows
/// byte-unchanged through a degraded tick), the Ok retry reloads and
/// un-gates, and a later sample-bearing tick persists again.
// r[verify ctrl.nodeclaim.lease-edge-polarity+3]
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
// r[verify ctrl.nodeclaim.lease-edge-polarity+3]
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
    assert!(
        lab.gate.retain(&mut Vec::new()),
        "gate armed after FFD tick"
    );

    lab.r.hooks.on_lose();
    lab.leader_flag.store(false, Ordering::SeqCst);
    lab.r.pg = lab.closed_pool().await; // would error if touched
    let acks_before = lab.ack_calls().len();
    lab.tick(10, Vec::new()).await; // empty queue: zero kube traffic

    assert!(!lab.gate.retain(&mut Vec::new()), "gate unarmed same tick");
    assert_eq!(lab.ack_calls().len(), acks_before, "no admin traffic");
}

/// R7 standby: a standby tick has no effects and freezes every
/// counter/field (empty verifier + closed pool prove "no traffic").
// r[verify ctrl.nodeclaim.lease-edge-polarity+3]
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
// r[verify ctrl.nodeclaim.lease-edge-polarity+3]
#[tokio::test]
async fn wedge_evidence_survives_acquire() {
    let mut lab = Lab::new().await;
    lab.set_open_attempts(vec![
        OpenAttempt {
            intent_id: "drv-a".into(),
            source_node: "node-c-w".into(),
            deadline_secs: 60,
            assigned_at_age_secs: 120,
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
            ..Default::default()
        },
        OpenAttempt {
            intent_id: "drv-b".into(),
            source_node: "node-c-w".into(),
            deadline_secs: 60,
            assigned_at_age_secs: 120,
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
            ..Default::default()
        },
    ]);
    let tc0 = lab.r.tick_counter;
    // Tick 1: evidence observed (no claim on the node yet).
    lab.tick(0, full_tick_scenario(vec![], vec![], vec![]))
        .await;

    // Acquire edge; ledger view now empty — retention must carry.
    lab.r.hooks.on_acquire();
    lab.set_open_attempts(vec![]);

    // Tick 2: a registered claim materializes on the wedged node →
    // classify(Dead) from RETAINED evidence → scripted DELETE.
    let claim = nc_json("c-w", 0, Some(1)); // stale edge: record-only
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
            .any(|a| a.registered_cells.contains(&cell().to_string())),
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
        !lab.gate.retain(&mut Vec::new()),
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
    let lab = Lab::new().await;

    // The scale-to-zero shape: nothing ICE'd, nothing registered, no
    // observations, zero bound pods.
    lab.r
        .report_unfulfillable(&[], &[], vec![], vec![])
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
            &[],
            &[],
            vec![],
            vec![rio_proto::types::BoundIntent {
                intent_id: "drv-285".into(),
                node_name: "node-1".into(),
                deadline_secs: 0,
            }],
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
            .any(|a| a.registered_cells.contains(&cell().to_string())),
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
    lab.r.pending_evidence.registered_cells.insert(cell());
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

// r[verify ctrl.nodeclaim.acquire-edge-token]
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
