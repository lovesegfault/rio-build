//! CR-1 actor-boundary state-machine contract tests.
//!
//! Round 1 fixed primitives so they *can* satisfy a stated invariant
//! (ε_h seed = `hash(drv)` + `MemoEntry.pinned_explore`; SolveCache
//! keyed on `model_key_hash`; `IceBackoff::clear` resets on success).
//! Round 2
//! found every production caller violated the precondition that makes
//! the invariant hold (`inputs_gen` bumped unconditionally every 60s;
//! no LRU→SolveCache eviction; `clear()` wired to Pending not
//! Registered). The unit tests added in r1 exercise the primitive in
//! isolation — the test *is* the caller, so it cannot catch a caller
//! that bumps too often, never evicts, or clears on the wrong edge.
//!
//! These tests assert the doc-claimed invariants as *externally
//! observable* properties of the [`DagActor`] surface the controller
//! talks to: [`DagActor::compute_spawn_intents`],
//! [`DagActor::handle_ack_spawned_intents`], and the housekeeping
//! [`DagActor::maybe_refresh_estimator`] wiring. They would have
//! caught all three round-2 bugs and the round-1 bugs they shadow.
//!
//! Determinism of the forecast budget gate (r5 bug_025) is asserted
//! at the same actor boundary by `forecast_budget_deterministic` in
//! `misc.rs` — kept there because it needs the forecast-frontier
//! fixture, but it is a contract test in the same sense.

use super::*;
use crate::sla::metrics::counter_map;
use metrics_util::debugging::DebuggingRecorder;
use std::collections::BTreeMap;

/// The three counters `solve_intent_for` may emit. After §Third-strike
/// and arm-on-ack, `compute_spawn_intents` is **side-effect-free except
/// idempotent memo fill and debounced emits of these** — every other
/// counter write is a regression of merged_bug_001 / the validator's
/// r3 BLOCKED finding (per-poll over-emission).
///
/// The debounce gate is **per-counter**, not uniformly `was_miss`:
/// `_hw_cost_unknown` and `_infeasible{BestEffort.why}` are
/// `was_miss`-gated (memo inputs); `_hw_ladder_exhausted` and
/// `_infeasible{CapacityExhausted}` are ICE-edge-gated per R5B2
/// (read-time state, NOT in `inputs_gen`); `_infeasible` on the
/// hw-agnostic path is `fit_content_hash`-anchored per R5B3. Each gate
/// bounds the counter to ≤1 per `model_key` per edge — the (1a) `≤
/// |Ready drvs|` assertion holds for all of them.
const ONCE_PER_MISS: &[&str] = &[
    "rio_scheduler_sla_infeasible_total",
    "rio_scheduler_sla_hw_ladder_exhausted_total",
    "rio_scheduler_sla_hw_cost_unknown_total",
];

/// `(intent_id → node_affinity)` from one `compute_spawn_intents` poll.
/// `BTreeMap` so equality is order-insensitive on intent_id.
fn affinity_map(actor: &DagActor) -> BTreeMap<String, Vec<rio_proto::types::NodeSelectorTerm>> {
    actor
        .compute_spawn_intents(&Default::default())
        .intents
        .into_iter()
        .map(|i| (i.intent_id, i.node_affinity))
        .collect()
}

/// One housekeeping refresh cycle: `maybe_refresh_estimator` early-
/// returns on 5/6 ticks; six calls guarantees exactly one refresh.
async fn refresh_cycle(actor: &mut DagActor) {
    for _ in 0..6 {
        actor.maybe_refresh_estimator().await;
    }
}

/// **Selector stability** (`r[sched.sla.hw-class.epsilon-explore+6]`):
/// `SpawnIntent.node_affinity` is a pure function of `(drv_hash,
/// inputs_gen)` — N controller polls with no input change return
/// identical selectors for every intent, AND a no-op
/// `maybe_refresh_estimator` (no `hw_perf_samples` change) does NOT
/// bump `inputs_gen` → selectors STILL identical. A real PG row insert
/// flips `hw_changed` → `inputs_gen` bumps → selectors MAY re-roll.
///
/// Would have caught r1 bug_049 (per-call `rand::rng()` re-roll →
/// selector-drift reap churn) AND r2 merged_bug_028 (unconditional
/// bump every 60s → ε_h re-rolls before Karpenter provisions).
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn contract_selector_stability() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // PG-derived hw table so `maybe_refresh_estimator` is a true no-op
    // on subsequent cycles. 3 distinct pods × 3 classes → `pod_ids=3`
    // (trusted); 1 tenant < `FLEET_MEDIAN_MIN_TENANTS` → factor gated
    // to `[1.0; K]`. Stable across reloads.
    for h in ["intel-6", "intel-7", "intel-8"] {
        for p in 0..3 {
            sqlx::query(
                "INSERT INTO hw_perf_samples (hw_class, pod_id, factor) \
                 VALUES ($1, $2, '{\"alu\":1.0}')",
            )
            .bind(h)
            .bind(format!("pod-{h}-{p}"))
            .execute(&db.pool)
            .await?;
        }
    }

    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: test_hw_sla_config(),
            ..Default::default()
        },
    );
    actor.sla_tiers = actor.sla_config.solve_tiers();
    actor.sla_ceilings = actor.sla_config.ceilings();
    // ε_h > 0 so the explore branch is reachable — its determinism is
    // exactly what r1 bug_049 broke. ε=0 is the memo only (trivially
    // stable).
    actor.sla_config.hw_explore_epsilon = 0.2;
    seed_fit(&actor, "test-pkg");

    // Warm-up: first refresh loads PG → hw table populated.
    // empty→populated is a solve-relevant change; capture g0 AFTER.
    refresh_cycle(&mut actor).await;
    let g0 = actor.solve_inputs().2;

    for i in 0..10 {
        actor.test_inject_ready(&format!("d{i}"), Some("test-pkg"), "x86_64-linux", false);
    }

    // ── (1a) 8× poll, no state change → identical selectors ────────────
    // Side-effect-free except idempotent memo + debounced emits:
    // capture the full counter map before/after; ONLY the three
    // `ONCE_PER_MISS` counters may have moved, and each by ≤ |Ready
    // drvs| (the per-key debounce bound — 10 drvs share 1 model_key,
    // so the actual bound is 1; ≤10 is the loose form that survives
    // fixture reshuffles). Any other counter delta is a per-poll side
    // effect the validator's r3 BLOCKED finding flagged.
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let polls: Vec<_> = {
        let _g = metrics::set_default_local_recorder(&rec);
        (0..8).map(|_| affinity_map(&actor)).collect()
    };
    let after = counter_map(&snap);
    for (name, &post) in &after {
        if post == 0 {
            continue;
        }
        assert!(
            ONCE_PER_MISS.contains(&name.as_str()),
            "8× no-change poll moved counter `{name}` (0→{post}) — \
             compute_spawn_intents must be side-effect-free except \
             debounced emits of {ONCE_PER_MISS:?}"
        );
        assert!(
            post <= polls[0].len() as u64,
            "`{name}` moved by {post} > |Ready drvs|={} — per-key \
             debounce bound violated (per-poll over-emission; \
             merged_bug_001 shape)",
            polls[0].len()
        );
    }
    assert_eq!(polls[0].len(), 10, "all 10 Ready drvs intent-eligible");
    assert!(
        polls[0].values().any(|a| !a.is_empty()),
        "precondition: solve_full path active (hw table populated)"
    );
    for (n, w) in polls.windows(2).enumerate() {
        assert_eq!(
            w[0],
            w[1],
            "poll {n}→{}: node_affinity must be identical for fixed (drv_hash, inputs_gen)",
            n + 1
        );
    }
    assert_eq!(
        actor.solve_inputs().2,
        g0,
        "compute_spawn_intents is read-only on inputs_gen"
    );
    assert!(
        actor.dispatched_cells.is_empty(),
        "compute_spawn_intents is read-only on dispatched_cells — \
         arm-on-ack, not arm-on-emit (merged_bug_002: 8× poll with no \
         ack must leave it empty)"
    );

    // ── (1b) no-op refresh → solve_relevant_hash same → still g0 ──────
    refresh_cycle(&mut actor).await;
    assert_eq!(
        actor.solve_inputs().2,
        g0,
        "no-op maybe_refresh_estimator must NOT change derived inputs_gen"
    );
    let after_noop = affinity_map(&actor);
    assert_eq!(
        after_noop, polls[0],
        "selectors stable across no-op refresh — the controller sees the same fingerprint"
    );

    // ── (1c) cross 2→3 trust threshold → trust bool flips → ≠ g0 ──────
    // intel-9 starts at 2 pods (untrusted); +1 row crosses HW_MIN_PODS.
    // Factor stays [1.0;K] (1 tenant < FLEET_MEDIAN_MIN_TENANTS), but
    // `pod_ids >= HW_MIN_PODS` flips false→true — solve-relevant.
    for p in 0..2 {
        sqlx::query(
            "INSERT INTO hw_perf_samples (hw_class, pod_id, factor) \
             VALUES ('intel-9', $1, '{\"alu\":1.0}')",
        )
        .bind(format!("pod-intel-9-{p}"))
        .execute(&db.pool)
        .await?;
    }
    refresh_cycle(&mut actor).await;
    assert_eq!(
        actor.solve_inputs().2,
        g0,
        "intel-9 at 2 pods: untrusted → bool stays false → unchanged"
    );
    sqlx::query(
        "INSERT INTO hw_perf_samples (hw_class, pod_id, factor) \
         VALUES ('intel-9', 'pod-intel-9-2', '{\"alu\":1.0}')",
    )
    .execute(&db.pool)
    .await?;
    refresh_cycle(&mut actor).await;
    let g1 = actor.solve_inputs().2;
    assert_ne!(
        g1, g0,
        "2→3 crosses HW_MIN_PODS → trust bool flips → derived inputs_gen changes"
    );
    let at_g1 = affinity_map(&actor);
    assert_eq!(affinity_map(&actor), at_g1, "deterministic at new gen");

    // ── (1d) pod_ids 3→4 within trusted, factor unchanged → UNCHANGED ──
    // merged_bug_011: old `content_hash` hashed raw `pod_ids`; this
    // would have changed g1 every 60s in steady state.
    sqlx::query(
        "INSERT INTO hw_perf_samples (hw_class, pod_id, factor) \
         VALUES ('intel-7', 'pod-intel-7-3', '{\"alu\":1.0}')",
    )
    .execute(&db.pool)
    .await?;
    refresh_cycle(&mut actor).await;
    assert_eq!(
        actor.solve_inputs().2,
        g1,
        "pod_ids 3→4, trust bool stays true, factor unchanged → inputs_gen UNCHANGED"
    );
    assert_eq!(
        affinity_map(&actor),
        at_g1,
        "selectors unchanged — no ε_h re-roll on pod_ids monotone bump"
    );

    // ── (1e) stale_clamp flip → CostTable solve-relevant → ≠ g1 ───────
    // bug_026: `apply_stale_clamp` flipped without bump; derived
    // inputs_gen reflects it with no caller action.
    actor
        .cost_table
        .write()
        .apply_stale_clamp(crate::sla::cost::STALE_CLAMP_AFTER_SECS + 1.0);
    let g2 = actor.solve_inputs().2;
    assert_ne!(
        g2, g1,
        "stale_clamp false→true → derived inputs_gen changes"
    );
    Ok(())
}

/// **SolveCache bounded by live fits**
/// (`r[sched.sla.hw-class.admissible-set]`): the memo's doc-claimed
/// bound "│live SlaEstimator keys│ × │overrides│" only holds if LRU
/// eviction propagates via the `on_evict` hook. Churn N≫cap distinct
/// pnames through the SAME wiring [`DagActor::maybe_refresh_estimator`]
/// uses, then poll once — `solve_cache.len()` MUST stay ≤
/// `live_fit_count()`.
///
/// Would have caught r2 merged_bug_017 (no eviction → orphaned entries
/// forever; `solve_intent_for` short-circuits on `fit.as_ref()?` so
/// nothing ever overwrites them).
// r[verify sched.sla.hw-class.admissible-set]
#[tokio::test]
async fn contract_solve_cache_bounded_by_live_fits() {
    const CAP: usize = 5;
    const CHURN: usize = 20;

    let db = TestDb::new(&MIGRATOR).await;
    let mut cfg = test_hw_sla_config();
    cfg.max_keys_per_tenant = CAP;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: cfg,
            ..Default::default()
        },
    );
    actor.sla_tiers = actor.sla_config.solve_tiers();
    actor.sla_ceilings = actor.sla_config.ceilings();
    let mut m = std::collections::HashMap::new();
    m.insert("intel-6".into(), 1.0);
    m.insert("intel-7".into(), 1.4);
    m.insert("intel-8".into(), 2.0);
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));

    // Churn fits via the shared `on_fit_evicted` (same body
    // `maybe_refresh_estimator` + `SlaEvict` use). `seed()` is a no-op
    // on_evict, which is exactly the r2 bug shape.
    for i in 0..CHURN {
        let pname = format!("pkg{i}");
        let fit = make_fit(&pname);
        actor
            .sla_estimator
            .insert(&fit.key.clone(), fit, |k| actor.on_fit_evicted(k));
        actor.test_inject_ready(&format!("d{i}"), Some(&pname), "x86_64-linux", false);
    }

    // One controller poll: `solve_intent_for` runs for every Ready drv.
    // Only the CAP surviving fits hit `get_or_insert_with`; the
    // CHURN-CAP evicted ones short-circuit on `cached(k)==None` (no
    // new entry) AND their old entries were dropped via `on_evict`.
    let _ = actor.compute_spawn_intents(&Default::default());

    let live = actor.sla_estimator.live_fit_count();
    assert_eq!(live, CAP, "SlaEstimator LRU bounded at max_keys_per_tenant");
    assert!(
        actor.solve_cache.len() <= live,
        "solve_cache.len()={} > live_fit_count()={} — on_evict hook not propagating",
        actor.solve_cache.len(),
        live
    );

    // bug_024: explicit `SlaEvict` (operator `rio-cli sla reset`)
    // propagates via the SAME `on_fit_evicted` — orphaned Schmitt
    // `prev_a` would otherwise survive reset. Evict every live key;
    // solve_cache must drain to 0.
    let before = actor.solve_cache.len();
    assert!(before > 0, "precondition: memo populated");
    for i in (CHURN - CAP)..CHURN {
        let key = make_fit(&format!("pkg{i}")).key;
        let (tx, _rx) = tokio::sync::oneshot::channel();
        actor.handle_admin(crate::actor::command::AdminQuery::SlaEvict { key, reply: tx });
    }
    assert_eq!(
        actor.solve_cache.len(),
        0,
        "SlaEvict must propagate via on_fit_evicted — memo drains"
    );
}

/// **ICE step doubles without clear**
/// (`r[sched.sla.hw-class.ice-mask]`): the actor-boundary form of
/// `ice_step_doubles_across_mark_without_clear` — the `SpawnIntent`
/// echoed back via `handle_ack_spawned_intents` is one
/// `compute_spawn_intents` actually emitted (the realistic controller
/// loop), not hand-constructed. Three `{spawned, unfulfillable, []}`
/// acks → `step==2`; one `{[], [], registered}` ack → `step==None`.
///
/// Would have caught r2 bug_008 (`clear()` wired to Pending ack →
/// `clear→mark` every tick → step stuck at 0).
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn contract_ice_step_doubles_then_clears_on_registered() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.test_inject_ready("d0", Some("test-pkg"), "x86_64-linux", false);

    // Controller flow: poll → spawn Job for the emitted intent → ack
    // it back. `node_affinity` is non-empty (solve_full fired) so the
    // old `term_to_cell(spawned[0].node_affinity[0])` clear-loop would
    // have parsed a real cell out of it.
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d0")
        .expect("d0 emitted")
        .clone();
    assert!(
        !intent.node_affinity.is_empty(),
        "precondition: solve_full path active"
    );

    let cell: crate::sla::config::Cell = ("intel-6".into(), CapacityType::Spot);
    for _ in 0..3 {
        actor.handle_ack_spawned_intents(
            std::slice::from_ref(&intent),
            &["intel-6:spot".into()],
            &[],
            &[],
            &[],
        );
    }
    assert_eq!(
        actor.ice.step(&cell),
        Some(2),
        "spawned-ack must NOT clear; backoff doubles across consecutive marks"
    );
    // Arm-on-ack: the spawned echo populates `dispatched_cells` with
    // the FULL `cells` vec recovered from the parallel
    // `(hw_class_names, node_affinity)` wire form — single-cell case
    // round-trips as a 1-vec.
    assert_eq!(
        actor
            .dispatched_cells
            .get("d0")
            .as_deref()
            .map(|v| v.as_slice()),
        Some(&[(intent.hw_class_names[0].clone(), CapacityType::Spot)][..]),
        "spawned-ack arms dispatched_cells from the wire form"
    );

    actor.handle_ack_spawned_intents(&[], &[], &["intel-6:spot".into()], &[], &[]);
    assert_eq!(
        actor.ice.step(&cell),
        None,
        "registered_cells is the success edge → clears"
    );
}

/// **R24B7 B2 — fold + wire-format**: `observed_instance_types` in the
/// controller's `Cell::to_string` `"h:od"` form folds into
/// `CostTable.cells`. Pins the wire-format round-trip (B6a
/// `"on-demand"` vs `"od"` lesson — controller emits `"od"`, scheduler
/// `parse_cell` accepts both). The `ActorCommand::AckSpawnedIntents`
/// match arm at `actor/mod.rs` is exhaustive (no `..`), so dropping
/// the field there is a compile error; this asserts the
/// `handle_ack_spawned_intents` body actually folds it.
// r[verify sched.sla.cost-instance-type-feedback]
#[tokio::test]
async fn ack_observed_instance_types_folds_into_cost_table() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::ObservedInstanceType;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let spot: crate::sla::config::Cell = ("mid-ebs-x86".into(), CapacityType::Spot);
    let od: crate::sla::config::Cell = ("mid-ebs-x86".into(), CapacityType::Od);
    assert!(actor.cost_table.read().menu(&spot).is_empty());

    // Controller's `Cell::to_string` form: `"h:spot"` / `"h:od"`
    // (sketch.rs:90 via `as_str()`).
    actor.handle_ack_spawned_intents(
        &[],
        &[],
        &[],
        &[
            ObservedInstanceType {
                cell: "mid-ebs-x86:spot".into(),
                instance_type: "c7i.8xlarge".into(),
                cores: 32,
                mem_bytes: 64 << 30,
            },
            ObservedInstanceType {
                cell: "mid-ebs-x86:od".into(),
                instance_type: "m7i.8xlarge".into(),
                cores: 32,
                mem_bytes: 128 << 30,
            },
        ],
        &[],
    );

    let ct = actor.cost_table.read();
    assert_eq!(ct.menu(&spot).len(), 1);
    assert_eq!(ct.menu(&spot)[0].name, "c7i.8xlarge");
    assert_eq!(ct.menu(&spot)[0].cores, 32);
    assert_eq!(ct.menu(&od).len(), 1, "controller 'od' form parses");
}

/// **`bound_intents` round-trips into `authoritative_binding`**
/// (`r[sched.admin.hung-node-detector+3]`). Exercises the
/// `handle_ack_spawned_intents` body so a forgotten destructure or
/// wire-field rename surfaces here, not as a silently-empty
/// `authoritative_binding` (the B6a wire-format-mismatch lesson). The
/// `intent_id` is the controller's `INTENT_ID_ANNOTATION` value
/// (= drv_hash); `node_name` is kube `spec.nodeName`.
///
/// Also pins the wholesale-rebuild invariant (mb_012): a NON-empty
/// `bound_intents` is the authoritative snapshot — entries absent from
/// it are dropped (replaces the old per-tick `.retain(dag.node…)`
/// sweep). An EMPTY `bound_intents` is the per-pool reconciler's "no
/// snapshot in this Ack" signal → no-op on the map.
#[tokio::test]
async fn ack_bound_intents_populates_authoritative_binding() {
    use rio_proto::types::BoundIntent;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    assert!(actor.authoritative_binding.is_empty());

    let bi = |id: &str, node: &str| BoundIntent {
        intent_id: id.into(),
        node_name: node.into(),
    };
    let abc = crate::state::DrvHash::from("abc123");
    let def = crate::state::DrvHash::from("def456");

    actor.handle_ack_spawned_intents(
        &[],
        &[],
        &[],
        &[],
        &[
            bi("abc123", "ip-10-0-1-5.ec2.internal"),
            bi("def456", "ip-10-0-1-6.ec2.internal"),
        ],
    );

    assert_eq!(
        actor
            .authoritative_binding
            .get(&abc)
            .map(|b| b.node.as_str()),
        Some("ip-10-0-1-5.ec2.internal")
    );
    assert_eq!(actor.authoritative_binding.len(), 2);
    // DAG empty in bare_actor_hw → tenant captured as None on first Ack.
    assert!(
        actor
            .authoritative_binding
            .get(&abc)
            .unwrap()
            .tenant
            .is_none()
    );

    // Wholesale-rebuild: second Ack omitting `def456` → that entry
    // dropped (the Ack IS the authoritative snapshot; deleted pods
    // disappear from the controller's `PodRequestedCache`).
    actor.handle_ack_spawned_intents(
        &[],
        &[],
        &[],
        &[],
        &[bi("abc123", "ip-10-0-1-5.ec2.internal")],
    );
    assert_eq!(actor.authoritative_binding.len(), 1);
    assert!(!actor.authoritative_binding.contains_key(&def));
    assert!(actor.authoritative_binding.contains_key(&abc));

    // Empty `bound_intents` = "this Ack carries no binding snapshot"
    // (per-pool reconciler at pool/jobs.rs sends `vec![]`; the
    // nodeclaim_pool reconciler owns the stream) → map unchanged.
    actor.handle_ack_spawned_intents(&[], &[], &[], &[], &[]);
    assert_eq!(
        actor.authoritative_binding.len(),
        1,
        "empty bound_intents must be a no-op (per-pool ack), not a wipe"
    );
    assert!(actor.authoritative_binding.contains_key(&abc));
}

/// `observe_instance_types` is gated on the shared `cost_was_leader`
/// latch (the `interrupt_housekeeping` edge-reload owner). Before the
/// reload, writes to `cost_table` would be clobbered by `*cost.write()
/// = CostTable::load(...)` — and the controller's `observe_registered`
/// is edge-detected, so a clobbered observation isn't re-sent.
// r[verify sched.sla.cost-instance-type-feedback]
#[tokio::test]
async fn ack_observed_instance_types_gated_on_cost_was_leader() {
    use rio_proto::types::ObservedInstanceType;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let spot: crate::sla::config::Cell =
        ("mid-ebs-x86".into(), crate::sla::config::CapacityType::Spot);

    let observed = [ObservedInstanceType {
        cell: "mid-ebs-x86:spot".into(),
        instance_type: "c7i.8xlarge".into(),
        cores: 32,
        mem_bytes: 64 << 30,
    }];

    // Pre-reload (was_leader=false): write is dropped.
    actor
        .cost_was_leader
        .store(false, std::sync::atomic::Ordering::Relaxed);
    actor.handle_ack_spawned_intents(&[], &[], &[], &observed, &[]);
    assert!(
        actor.cost_table.read().menu(&spot).is_empty(),
        "observation must NOT land on pre-reload table"
    );

    // Post-reload (was_leader=true via interrupt_housekeeping's
    // poller_tick_prelude Ok-arm): write applies.
    actor
        .cost_was_leader
        .store(true, std::sync::atomic::Ordering::Relaxed);
    actor.handle_ack_spawned_intents(&[], &[], &[], &observed, &[]);
    assert_eq!(actor.cost_table.read().menu(&spot).len(), 1);
}

/// **Ack records the FULL A'** (`r[sched.sla.hw-class.ice-mask]`): a
/// `SpawnIntent` whose `node_affinity` is an OR over `|A'|>1` cells must
/// arm `dispatched_cells` with the FULL parallel `(hw_class_names,
/// node_affinity)` vec, not `cells[0]`. The pod may land on `cells[i≠0]`;
/// the heartbeat-edge consumer needs the whole set to decide whether the
/// signal is unambiguous (bug_030).
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn contract_ack_spawned_records_full_a_prime() {
    use crate::sla::config::{CapacityType, Cell};
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let term = |h: &str, cap: &str| NodeSelectorTerm {
        match_expressions: vec![
            NodeSelectorRequirement {
                key: "rio.build/hw-class".into(),
                operator: "In".into(),
                values: vec![h.into()],
            },
            NodeSelectorRequirement {
                key: "karpenter.sh/capacity-type".into(),
                operator: "In".into(),
                values: vec![cap.into()],
            },
        ],
    };
    let intent = SpawnIntent {
        intent_id: "d".into(),
        hw_class_names: vec!["h0".into(), "h1".into()],
        node_affinity: vec![term("h0", "spot"), term("h1", "spot")],
        ..Default::default()
    };

    actor.handle_ack_spawned_intents(std::slice::from_ref(&intent), &[], &[], &[], &[]);

    let got: std::collections::HashSet<Cell> = actor
        .dispatched_cells
        .get("d")
        .expect("ack arms dispatched_cells")
        .iter()
        .cloned()
        .collect();
    assert_eq!(got.len(), 2, "full A' recorded, not just cells[0]");
    let want: std::collections::HashSet<Cell> = [
        ("h0".into(), CapacityType::Spot),
        ("h1".into(), CapacityType::Spot),
    ]
    .into_iter()
    .collect();
    assert_eq!(got, want);
}

/// **Metrics once per miss** (`r[sched.sla.hw-class.admissible-set]`):
/// the three `solve_intent_for` counter emits are gated on `was_miss` —
/// N polls at fixed `(model_key, inputs_gen)` increment by exactly 1
/// (the first miss), not N. An `inputs_gen` change is a fresh miss →
/// +1 more.
///
/// Direct guard for the validator's r3 BLOCKED finding: three emit
/// sites survived arm-on-ack + derived `inputs_gen` and still fired
/// per-poll (`ladder_exhausted` outside the memo closure; ε_h's
/// unmemoized `solve_full` re-emitting `hw_cost_unknown`; `intent_for`
/// fallback re-emitting `infeasible`). Would have caught merged_bug_001
/// (`BestEffort` not memoized → re-solve + re-emit every poll) and the
/// "Memoized — fires once per (key, inputs_gen)" comment being false
/// on two of three paths.
// r[verify sched.sla.hw-class.admissible-set]
#[tokio::test]
async fn contract_metrics_once_per_miss() {
    use crate::sla::config::CapacityType;
    const POLLS: usize = 5;
    const INFEASIBLE: &str = "rio_scheduler_sla_infeasible_total";
    const LADDER: &str = "rio_scheduler_sla_hw_ladder_exhausted_total";

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);

    // ── (a) BestEffort via DiskCeiling: `_infeasible_total` ────────────
    // disk_p90=300GiB > max_disk=200GiB → `solve_full` returns
    // `BestEffort{why=DiskCeiling}`. The hw-aware path memoizes that
    // result; `was_miss` gates `why.emit()`.
    let mut fit = make_fit("disk-hog");
    fit.disk_p90 = Some(crate::sla::types::DiskBytes(300 << 30));
    actor.sla_estimator.seed(fit);
    actor.test_inject_ready("d-disk", Some("disk-hog"), "x86_64-linux", false);

    let _ = counter_map(&snap); // drain anything from setup
    for _ in 0..POLLS {
        let _ = actor.compute_spawn_intents(&Default::default());
    }
    let d = counter_map(&snap);
    assert_eq!(
        d.get(INFEASIBLE).copied().unwrap_or(0),
        1,
        "`_infeasible_total` must fire once per (model_key, inputs_gen) \
         miss, not once per poll — {POLLS}× poll at fixed inputs_gen \
         incremented by ≠1 (BestEffort not memoized OR `was_miss` not \
         gating `why.emit()`)"
    );

    // ── (b) ladder exhausted via all-ICE-masked ───────────────────────
    // Feasible solve, but every (h, cap) cell ICE-masked → A\masked = ∅
    // → `ladder_exhausted_total{exit=all_masked}` +
    // `infeasible_total{reason=capacity_exhausted}`. Both gated on the
    // `MemoEntry.ice_exhausted` rising edge (R5B2); poll 1 is the
    // false→true edge → emit; polls 2+ stay true → no edge → silent.
    // The `test-pkg` fit from `bare_actor_hw` is feasible.
    for h in ["intel-6", "intel-7", "intel-8"] {
        for cap in CapacityType::ALL {
            actor.ice.mark(&(h.into(), cap));
        }
    }
    assert!(
        actor
            .ice
            .exhausted(["intel-6", "intel-7", "intel-8"].map(String::from).iter()),
        "precondition: all H × cap masked"
    );
    actor.test_inject_ready("d-ice", Some("test-pkg"), "x86_64-linux", false);

    for _ in 0..POLLS {
        let _ = actor.compute_spawn_intents(&Default::default());
    }
    let d = counter_map(&snap);
    assert_eq!(
        d.get(LADDER).copied().unwrap_or(0),
        1,
        "`_hw_ladder_exhausted_total` must fire once on the ICE-edge \
         (false→true), not once per poll"
    );
    assert_eq!(
        d.get(INFEASIBLE).copied().unwrap_or(0),
        1,
        "`CapacityExhausted.emit()` paired with ladder_exhausted must \
         also fire once on the ICE-edge"
    );

    // ── (c) inputs_gen change → fresh miss → `was_miss`-gated emits +1 ─
    // `apply_stale_clamp` flips a CostTable solve-relevant field →
    // derived `inputs_gen` changes → next poll is a miss for both
    // model_keys. `BestEffort.why` is `was_miss`-gated → re-emits.
    // R5B2: `ladder_exhausted` is now ICE-edge-gated, NOT `was_miss`-
    // gated — `MemoEntry.ice_exhausted` is per-key (carried across the
    // staleness miss), and the ICE state hasn't changed → no edge → no
    // re-emit. `CapacityExhausted` is paired with it, so also silent.
    let g_before = actor.solve_inputs().2;
    actor
        .cost_table
        .write()
        .apply_stale_clamp(crate::sla::cost::STALE_CLAMP_AFTER_SECS + 1.0);
    assert_ne!(actor.solve_inputs().2, g_before, "inputs_gen changed");

    let _ = actor.compute_spawn_intents(&Default::default());
    let d = counter_map(&snap);
    assert_eq!(
        d.get(LADDER).copied().unwrap_or(0),
        0,
        "ICE-edge-gated: inputs_gen change is NOT an ICE edge → ladder \
         does NOT re-emit (R5B2: was_miss is the wrong gate for \
         read-time state)"
    );
    // `disk-hog` (BestEffort.why, was_miss-gated) re-emits; `test-pkg`
    // (CapacityExhausted, ICE-edge-gated) does NOT.
    assert_eq!(
        d.get(INFEASIBLE).copied().unwrap_or(0),
        1,
        "fresh inputs_gen → BestEffort.why re-emits (was_miss-gated); \
         CapacityExhausted does NOT (ICE-edge-gated)"
    );

    // ── (d) re-poll at the new gen → no further emits ─────────────────
    for _ in 0..POLLS {
        let _ = actor.compute_spawn_intents(&Default::default());
    }
    let d = counter_map(&snap);
    assert_eq!(
        d.get(LADDER).copied().unwrap_or(0),
        0,
        "memo hit at new gen"
    );
    assert_eq!(
        d.get(INFEASIBLE).copied().unwrap_or(0),
        0,
        "memo hit at new gen"
    );

    // ── (e) bug_012: refit changing ONLY `hw_bias` → fresh miss ───────
    // `fit_content_hash` is the per-key staleness field; R5B4 added
    // `hw_bias` to it (was correctness-by-coincidence via `sum_w`).
    // Re-seed `disk-hog` with every solve-input field IDENTICAL except
    // `hw_bias["intel-7"]=1.2` → `fit_content_hash` differs → memo
    // miss at unchanged `inputs_gen` → `was_miss`-gated `BestEffort.why`
    // re-emits exactly once. Unit-level `fit_content_hash_covers_hw_bias`
    // proves the hash differs; THIS proves the actor honours it as a
    // staleness field (would have caught r5 bug_012 at the boundary).
    let mut fit = make_fit("disk-hog");
    fit.disk_p90 = Some(crate::sla::types::DiskBytes(300 << 30));
    fit.hw_bias.insert("intel-7".into(), 1.2);
    actor.sla_estimator.seed(fit);
    for _ in 0..POLLS {
        let _ = actor.compute_spawn_intents(&Default::default());
    }
    let d = counter_map(&snap);
    assert_eq!(
        d.get(INFEASIBLE).copied().unwrap_or(0),
        1,
        "hw_bias-only refit → fit_content_hash changes → was_miss → \
         BestEffort.why re-emits once (bug_012: omitting hw_bias from \
         the hash would leave this at 0 — stale memo served forever)"
    );
}

/// **R5B3 / merged_bug_008** — `intent_for`'s `_infeasible_total` emit
/// was unreachable under `hwCostSource: ""` (the helm default): the
/// hw-aware gate is `false`, so `was_miss` stays `false` initial →
/// `suppress = !was_miss = true` → metric flat zero. The fix gives the
/// hw-agnostic path its OWN once-per-`(mkh, fit_content_hash)` anchor
/// (`SolveCache::infeasible_static_fh`): emit once, suppress repeat
/// polls, re-arm on refit, sweep on `on_fit_evicted`.
#[tokio::test]
async fn contract_metrics_once_per_miss_hw_agnostic() {
    const POLLS: usize = 8;
    const INFEASIBLE: &str = "rio_scheduler_sla_infeasible_total";

    let db = TestDb::new(&MIGRATOR).await;
    // `test_sla_config()` with no hw-factor table seeded — the
    // `!hw.is_empty()` gate at solve_intent_for is false → hw-agnostic
    // intent_for path. Tier p90=1200.
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: test_sla_config(),
            ..Default::default()
        },
    );
    actor.sla_tiers = actor.sla_config.solve_tiers();
    actor.sla_ceilings = actor.sla_config.ceilings();

    // S=2000 > p90 bound=1200: T(c)≥S ∀c → solve_tier BestEffort →
    // classify_ceiling=SerialFloor. n_eff/span force the solve branch
    // (not explore).
    let mut fit = make_fit("synth-serial");
    fit.fit = crate::sla::types::DurationFit::Amdahl {
        s: crate::sla::types::RefSeconds(2000.0),
        p: crate::sla::types::RefSeconds(0.0),
    };
    actor.sla_estimator.seed(fit);
    actor.test_inject_ready("d-ser", Some("synth-serial"), "x86_64-linux", false);

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);

    // ── 8× poll at fixed fit → exactly 1 emit ─────────────────────────
    for _ in 0..POLLS {
        let _ = actor.compute_spawn_intents(&Default::default());
    }
    let d = counter_map(&snap);
    assert_eq!(
        d.get(INFEASIBLE).copied().unwrap_or(0),
        1,
        "`_infeasible_total{{serial_floor}}` must fire exactly once \
         across {POLLS}× poll under hw_cost_source=None — was 0 \
         (suppressed forever via `!was_miss`) before R5B3; would be \
         {POLLS} (per-poll noise) without the `infeasible_static_fh` \
         anchor"
    );

    // ── refit (fit_content_hash changes) → re-arm → +1 ────────────────
    let mut refit = make_fit("synth-serial");
    refit.fit = crate::sla::types::DurationFit::Amdahl {
        s: crate::sla::types::RefSeconds(2500.0),
        p: crate::sla::types::RefSeconds(0.0),
    };
    actor.sla_estimator.seed(refit);
    for _ in 0..POLLS {
        let _ = actor.compute_spawn_intents(&Default::default());
    }
    let d = counter_map(&snap);
    assert_eq!(
        d.get(INFEASIBLE).copied().unwrap_or(0),
        1,
        "refit changes fit_content_hash → anchor re-arms → exactly 1 \
         more emit across {POLLS}× poll"
    );

    // ── on_fit_evicted sweeps the anchor → re-arm ─────────────────────
    actor.on_fit_evicted(&crate::sla::types::ModelKey {
        pname: "synth-serial".into(),
        system: "x86_64-linux".into(),
        tenant: String::new(),
    });
    // Re-seed the SAME fit (same fh as last poll): anchor gone → emits
    // once again. Without the `remove_model_key` sweep of
    // `infeasible_static_fh`, this would be 0 (orphaned suppress).
    let mut refit = make_fit("synth-serial");
    refit.fit = crate::sla::types::DurationFit::Amdahl {
        s: crate::sla::types::RefSeconds(2500.0),
        p: crate::sla::types::RefSeconds(0.0),
    };
    actor.sla_estimator.seed(refit);
    for _ in 0..POLLS {
        let _ = actor.compute_spawn_intents(&Default::default());
    }
    let d = counter_map(&snap);
    assert_eq!(
        d.get(INFEASIBLE).copied().unwrap_or(0),
        1,
        "on_fit_evicted sweeps infeasible_static_fh → same-fh re-seed \
         emits once (not orphan-suppressed)"
    );
}

/// **R7B1 / bug 035** — under `hw_cost_source=None` (helm default), the
/// `infeasible_static_fh` debounce was recorded BEFORE `intent_for`'s
/// hints early-returns. A serial drv (`enable_parallel_building =
/// Some(false)`) and its non-serial sibling at the same `(pname,
/// system, tenant)` reach the debounce with identical `(mkh, ovr, fh)`;
/// if `dag.iter_nodes()` (HashMap order) yields the serial drv first it
/// burns the slot then early-returns without emitting → sibling
/// suppressed → metric flat zero.
///
/// Different `DrvHash` values → different SipHash placements →
/// different iteration order in one process. At least one `k` yields 0
/// at 4ef92abf. After R7B1 (`intent_for` returns `IntentDecision`;
/// record AFTER the early-returns) every `k` yields exactly 1.
#[tokio::test]
async fn contract_infeasible_static_hints_independent() {
    use crate::sla::metrics::infeasible_counts;

    let db = TestDb::new(&MIGRATOR).await;
    // S=2000 > p90=1200 → solve_tier BestEffort → classify_ceiling =
    // SerialFloor. Shared by both drvs (same `(pname, system, tenant)`
    // → same `(mkh, ovr, fh)`).
    let mut fit = make_fit("synth-hint");
    fit.fit = crate::sla::types::DurationFit::Amdahl {
        s: crate::sla::types::RefSeconds(2000.0),
        p: crate::sla::types::RefSeconds(0.0),
    };

    for k in 0..8 {
        let mut actor = bare_actor_cfg(
            db.pool.clone(),
            DagActorConfig {
                sla: test_sla_config(),
                ..Default::default()
            },
        );
        actor.sla_tiers = actor.sla_config.solve_tiers();
        actor.sla_ceilings = actor.sla_config.ceilings();
        // hw-factor table unseeded → `!hw.is_empty()` gate false →
        // hw-agnostic intent_for path.
        actor.sla_estimator.seed(fit.clone());

        let ser = format!("d-ser-{k:02x}");
        let par = format!("d-par-{k:02x}");
        actor.test_inject_ready(&ser, Some("synth-hint"), "x86_64-linux", false);
        actor.dag.node_mut(&ser).unwrap().enable_parallel_building = Some(false);
        actor.test_inject_ready(&par, Some("synth-hint"), "x86_64-linux", false);

        let rec = DebuggingRecorder::new();
        let snap = rec.snapshotter();
        let _g = metrics::set_default_local_recorder(&rec);

        let _ = actor.compute_spawn_intents(&Default::default());
        let m = infeasible_counts(&snap);
        assert_eq!(
            m.get("serial_floor").copied().unwrap_or(0),
            1,
            "k={k}: `_infeasible_total{{serial_floor}}` must fire \
             exactly once regardless of which drv `iter_nodes()` \
             yields first — was 0 (serial drv burned the \
             `infeasible_static_fh` slot then early-returned at \
             solve.rs `enable_parallel_building` without emitting; \
             sibling suppressed) before R7B1"
        );
    }
}

/// **§Fourth-strike Option 2 falsification** — `h_explore` is pinned
/// in `MemoEntry.pinned_explore`, decoupled from `inputs_gen`. With
/// Option 1 (quantize) in place, steady-state `inputs_gen` doesn't
/// churn → the pin-survives-churn property is **unfalsifiable** unless
/// the test FORCES `inputs_gen` to change via a non-noise path.
/// `apply_stale_clamp` flips a CostTable solve-relevant bool — same
/// mechanism the (1e) step of `contract_selector_stability` uses.
///
/// (a) ε_h forced (ε=1.0) → poll → capture `h_explore`. Force
///     `inputs_gen` change. Re-poll → assert `h_explore` IDENTICAL.
///     This is the property Option 2 exists to guarantee and the seed
///     `^ inputs_gen` term broke.
/// (b) Graduation: bump `factor[h_explore]` so it dominates → enters
///     A. Re-poll → graduation filter `!in_a.contains(h)` clears the
///     pin → `h_explore` re-drawn from `H\A`, MUST differ.
///
/// Would have caught: r1 bug_049, r2 mb_028, r3 mb_011, r3 bug_026,
/// r3 bug_009, r5 mb_018 (the ε_h half — Option 1 covers the
/// memo-thrash half).
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn contract_h_explore_stable_across_inputs_gen_churn() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // ε=1.0: every drv hits the explore branch — the seeded coin is
    // `hash(drv_hash)` only, so the SAME drv hits or misses
    // consistently regardless of `inputs_gen`.
    actor.sla_config.hw_explore_epsilon = 1.0;
    actor.test_inject_ready("d-pin", Some("test-pkg"), "x86_64-linux", false);
    let state = actor.dag.node("d-pin").unwrap();

    // Distinct hw_class names from one solve — for an ε_h hit this is
    // `{h_explore}` (1 element); for the unrestricted memo it's `A`.
    let h_of = |intent: &crate::state::SolvedIntent| -> std::collections::BTreeSet<String> {
        intent.hw_class_names.iter().cloned().collect()
    };

    // ── (a) pin survives `inputs_gen` churn ───────────────────────────
    let (hw, cost, g0) = actor.solve_inputs();
    let h0 = h_of(&actor.solve_intent_for(state, &hw, &cost, g0));
    assert_eq!(
        h0.len(),
        1,
        "ε=1.0 + |H|>1 → explore branch fires; A' ⊆ {{h_explore}}×{{spot,od}}"
    );
    let h0 = h0.into_iter().next().unwrap();
    // Force `inputs_gen` change via stale_clamp flip — a real
    // CostTable solve-relevant bool, NOT a synthetic `g0+1`. This is
    // the path Option 1's quantization does NOT smooth (it's a
    // discrete flip), so the test exercises Option 2 even with
    // Option 1 in place.
    actor
        .cost_table
        .write()
        .apply_stale_clamp(crate::sla::cost::STALE_CLAMP_AFTER_SECS + 1.0);
    let (hw, cost, g1) = actor.solve_inputs();
    assert_ne!(g1, g0, "stale_clamp flip → derived inputs_gen changes");
    let h1 = h_of(&actor.solve_intent_for(state, &hw, &cost, g1));
    assert_eq!(
        h1.into_iter().next().as_deref(),
        Some(h0.as_str()),
        "Option 2: `h_explore` pinned in MemoEntry — IDENTICAL across \
         `inputs_gen` churn. Pre-Opt2 the seed `^ inputs_gen` term \
         re-rolled the draw here, and `reap_stale_for_intents` would \
         reap the explore Job mid-provisioning."
    );
    // Re-poll at g1 → still h0 (determinism + pin both hold).
    for _ in 0..3 {
        assert_eq!(
            h_of(&actor.solve_intent_for(state, &hw, &cost, g1))
                .into_iter()
                .next()
                .as_deref(),
            Some(h0.as_str()),
        );
    }

    // ── (b) graduation: pinned class enters A → pin clears ────────────
    // Bump factor[h0] → 100× faster → h0 dominates 𝔼[cost] → A
    // contains h0 (and only h0 — others are 100× more expensive,
    // far outside τ). The graduation filter `!in_a.contains(h)`
    // clears the pin; the next ε_h hit re-draws from H\A. Since
    // A={h0}, pool = H\{h0} → new draw MUST differ from h0. The pin
    // naturally carries forward across the seed_hw `inputs_gen` bump
    // (Option 2 just demonstrated in (a)); no manual reseeding.
    let mut m = std::collections::HashMap::new();
    for h in actor.sla_config.hw_classes.keys() {
        m.insert(h.clone(), if *h == h0 { 100.0 } else { 1.0 });
    }
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));
    let (hw, cost, g2) = actor.solve_inputs();
    assert_ne!(g2, g1, "factor change → derived inputs_gen changes");
    // Precondition: ε=0 poll at g2 fills the memo with the new A
    // (pin h0 carried forward through the staleness miss; ε=0 means
    // the explore branch is skipped so `hw_class_names` IS A).
    actor.sla_config.hw_explore_epsilon = 0.0;
    let in_a = h_of(&actor.solve_intent_for(state, &hw, &cost, g2));
    assert!(
        in_a.contains(&h0),
        "precondition: factor[{h0}]=100 → {h0} ∈ A; A={in_a:?}"
    );
    // ε=1.0 poll at g2 (memo hit): reads carried-forward pin=h0,
    // computes in_a from the memoized A (includes h0) → filter clears
    // → re-draw from H\A.
    actor.sla_config.hw_explore_epsilon = 1.0;
    let h2 = h_of(&actor.solve_intent_for(state, &hw, &cost, g2));
    let h2 = h2.into_iter().next().expect("ε=1.0 hit");
    assert_ne!(
        h2, h0,
        "graduation filter: pinned `{h0}` ∈ A → pin clears → re-drawn \
         from H\\A; got `{h2}`. Without the `!in_a.contains(h)` release \
         valve, hot pnames would explore exactly one class per process \
         lifetime."
    );
    assert!(
        !in_a.contains(&h2) || in_a.len() == actor.sla_config.hw_classes.len(),
        "re-drawn `{h2}` ∈ H\\A (or A=H → H\\{{cheapest}})"
    );
    // Re-draw is the new pin: stable on next poll.
    assert_eq!(
        h_of(&actor.solve_intent_for(state, &hw, &cost, g2))
            .into_iter()
            .next()
            .as_deref(),
        Some(h2.as_str()),
        "re-drawn pin is stable"
    );
}

/// **R8B1 / bug_014** — ε_h restricted `solve_full` passed `prev_a=∅`,
/// so heads-drvs lost the Schmitt deadband: every cell got `τ_enter`
/// only. `(h_explore, od)` flipped at the `[1+τ, 1+1.3τ]` boundary on
/// every `inputs_gen` epoch → `selector_fingerprint` drift → reap churn.
///
/// 4-poll falsification (τ=0.15 → τ_enter=1.15, τ_stay=1.195):
/// 1. od/spot=1.14 → 2 terms ({spot,od}); writes pinned_explore_a.
/// 2. od/spot=1.16 (deadband) → STILL 2 terms. **Red @ 4434b117**:
///    `prev_a=∅` → τ_enter applies → 1.16>1.15 → 1 term.
/// 3. od/spot=1.20 → 1 term ({spot}).
/// 4. od/spot=1.18 (deadband) → STILL 1 term. **Red against the
///    broken-guard variant** (`prev.pinned_explore != *pin` only):
///    poll-3's Hit had pin unchanged → no write → stale prev_a={spot,od}
///    from poll 1 → od τ_stay=1.195 → 2 terms. The widened guard
///    `|| prev.pinned_explore_a != cells` makes poll 3 write {spot}.
// r[verify sched.sla.hw-class.admissible-set]
#[tokio::test]
async fn contract_h_explore_schmitt_carries_prev_a() {
    use crate::sla::config::CapacityType;
    use crate::sla::cost::{CostTable, RatioEma};
    use crate::sla::solve::{self, SolveFullResult};

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // 2-hw_class fixture: h_main cheap (price 0.001) → unrestricted A =
    // {h_main}; h_exp dear (price 1.0) → ∉ A → ε_h pool = H\A = {h_exp}
    // (single element). resolve_h_explore deterministically pins h_exp
    // every poll (mkh^ovr-seeded; pool size 1).
    let h_main: String = "intel-8".into();
    let h_exp: String = "intel-6".into();
    actor
        .sla_config
        .hw_classes
        .retain(|k, _| *k == h_main || *k == h_exp);
    // ε=1.0 → drv_hash-seeded coin (snapshot.rs:840-846) always heads.
    actor.sla_config.hw_explore_epsilon = 1.0;
    // τ=0.15 explicit (cfg_hw parity): deadband (1.15, 1.195].
    actor.sla_config.hw_cost_tolerance = 0.15;
    let tau = actor.sla_config.hw_cost_tolerance;
    actor.test_inject_ready("d-schmitt", Some("test-pkg"), "x86_64-linux", false);
    let fit = make_fit("test-pkg");

    // Per-poll cost table: λ_spot(h_exp)→0 via huge denominator so
    // e_od/e_spot == price_od/price_spot (no `1/(1-p)` retry-factor
    // skew on spot). h_main priced 0.001 so it dominates the
    // unrestricted e_min by ~1000× → h_exp ∉ A. Per-cell price change
    // hashes into solve_relevant_hash → inputs_gen bumps each poll.
    // Per-class ceilings = global ceil (no capacity-reject in this
    // fixture).
    let set_ratio = |a: &DagActor, od_over_spot: f64| {
        let mut ct = a.cost_table.write();
        *ct = CostTable::from_parts(
            [
                ((h_main.clone(), CapacityType::Spot), 0.001),
                ((h_main.clone(), CapacityType::Od), 0.001),
                ((h_exp.clone(), CapacityType::Spot), 1.0),
                ((h_exp.clone(), CapacityType::Od), od_over_spot),
            ]
            .into(),
            [(
                h_exp.clone(),
                RatioEma {
                    numerator: 0.0,
                    denominator: 1e15,
                    updated_at: 0.0,
                },
            )]
            .into(),
        );
    };
    // Fixture-sanity (per solve.rs:2167-2173): compute e_od/e_spot
    // from solve_full's all_candidates so a mis-tuned fixture (e.g.
    // λ-skew creeping back in) fails LOUDLY here, not silently green.
    let sla_tiers = actor.sla_tiers.clone();
    let sla_ceilings = actor.sla_ceilings.clone();
    let sla_config = actor.sla_config.clone();
    let e_ratio = |hw: &crate::sla::hw::HwTable, cost: &CostTable| -> f64 {
        let SolveFullResult::Feasible(m) = solve::solve_full(
            &fit,
            &sla_tiers,
            hw,
            cost,
            &sla_ceilings,
            &sla_config,
            std::slice::from_ref(&h_exp),
            &std::collections::HashSet::new(),
            true,
        ) else {
            panic!("h_exp restricted solve must be feasible")
        };
        let e = |cap| {
            m.all_candidates
                .iter()
                .find(|c| c.cell.1 == cap)
                .unwrap_or_else(|| panic!("({h_exp},{cap:?}) candidate present"))
                .e_cost_upper
        };
        e(CapacityType::Od) / e(CapacityType::Spot)
    };

    let state = actor.dag.node("d-schmitt").unwrap();

    // ── poll 1: od/spot=1.14 ≤ τ_enter → 2 terms ({spot,od}) ─────────
    set_ratio(&actor, 1.14);
    let (hw, cost, g0) = actor.solve_inputs();
    let r = e_ratio(&hw, &cost);
    assert!(
        r <= 1.0 + tau,
        "fixture: e_od/e_spot={r:.4} ≤ τ_enter={:.3} (od IN fresh)",
        1.0 + tau
    );
    assert_eq!(
        actor
            .solve_intent_for(state, &hw, &cost, g0)
            .node_affinity
            .len(),
        2,
        "poll 1: od/spot=1.14 ≤ τ_enter → restricted A'={{spot,od}} (h_exp pinned)"
    );

    // ── poll 2: od/spot=1.16 (deadband) → STILL 2 terms ──────────────
    set_ratio(&actor, 1.16);
    let (hw, cost, g1) = actor.solve_inputs();
    assert_ne!(g1, g0, "menu price change → inputs_gen bump");
    let r = e_ratio(&hw, &cost);
    assert!(
        r > 1.0 + tau && r <= 1.0 + 1.3 * tau,
        "fixture: e_od/e_spot={r:.4} ∈ ({:.3}, {:.3}] deadband",
        1.0 + tau,
        1.0 + 1.3 * tau
    );
    assert_eq!(
        actor
            .solve_intent_for(state, &hw, &cost, g1)
            .node_affinity
            .len(),
        2,
        "poll 2: od/spot=1.16 in deadband; prev_a={{spot,od}} from poll 1 → \
         od τ_stay=1.195 → 2 terms. bug_014 @ 4434b117: snapshot.rs passed \
         prev_a=∅ → τ_enter=1.15 → 1.16>1.15 → 1 term."
    );

    // ── poll 3: od/spot=1.20 > τ_stay → 1 term ({spot}) ──────────────
    set_ratio(&actor, 1.20);
    let (hw, cost, g2) = actor.solve_inputs();
    assert_ne!(g2, g1);
    let r = e_ratio(&hw, &cost);
    assert!(
        r > 1.0 + 1.3 * tau,
        "fixture: e_od/e_spot={r:.4} > τ_stay={:.3} (od OUT even via prev_a)",
        1.0 + 1.3 * tau
    );
    assert_eq!(
        actor
            .solve_intent_for(state, &hw, &cost, g2)
            .node_affinity
            .len(),
        1,
        "poll 3: od/spot=1.20 > τ_stay → restricted A'={{spot}}"
    );

    // ── poll 4: od/spot=1.18 (deadband) → STILL 1 term ───────────────
    set_ratio(&actor, 1.18);
    let (hw, cost, g3) = actor.solve_inputs();
    assert_ne!(g3, g2);
    let r = e_ratio(&hw, &cost);
    assert!(
        r > 1.0 + tau && r <= 1.0 + 1.3 * tau,
        "fixture: e_od/e_spot={r:.4} ∈ ({:.3}, {:.3}] deadband",
        1.0 + tau,
        1.0 + 1.3 * tau
    );
    assert_eq!(
        actor
            .solve_intent_for(state, &hw, &cost, g3)
            .node_affinity
            .len(),
        1,
        "poll 4: od/spot=1.18 in deadband; prev_a={{spot}} from poll 3 → od \
         τ_enter=1.15 → 1 term. Broken-guard variant (`prev.pinned_explore \
         != *pin` only) would skip poll-3's write → stale prev_a={{spot,od}} \
         from poll 1 → od τ_stay=1.195 → 2 terms."
    );
}

/// **R17B0 / bug_001** — `Miss` reached via Feasible-all-masked at
/// `|pool|=1` discards the fresh A'. explore.rs:153's `_ =>` arm caught
/// BOTH `Feasible(m)`-all-masked (has `m.a.cells`) AND `BestEffort` (no
/// A'), dropping `m`; snapshot.rs's `next==prev_pin` reconstruction
/// committed the STALE `prev_explore_a` the solve was CALLED with.
///
/// 6-poll falsification (τ=0.15 → τ_enter=1.15, τ_stay=1.195;
/// `|pool|={h_exp}`):
/// 1. od/spot=1.14 → `Hit{spot,od}`. Stored prev_a={spot,od}.
/// 2. ICE-mask both (h_exp,*); od/spot=1.20 > τ_stay → restricted solve
///    `Feasible{spot}`, all-masked → `Miss`. **Fresh A'={spot}**.
///    Intent falls through to unrestricted memo (h_main present).
/// 3. clear ICE; od/spot=1.18 (deadband) → **1 term**. **Red @
///    36804895**: `Miss` carried only `next`; `next==prev_pin` →
///    committed stale `{spot,od}` from poll 1 → od τ_stay → 2 terms.
/// 4. od/spot=1.14 → `Hit{spot,od}`. Re-seeds prev_a={spot,od}.
/// 5. h_exp menu → no-fit (cores=0) → restricted `BestEffort` →
///    `Miss`. Singleton → **preserve** prev_a={spot,od}.
/// 6. restore menu; od/spot=1.16 (deadband) → **2 terms**. Regression
///    guard for the OTHER `Miss` arm: BestEffort-at-singleton must
///    preserve, not clear (clear → od τ_enter → 1.16>1.15 → 1 term).
// r[verify sched.sla.hw-class.admissible-set]
#[tokio::test]
async fn contract_h_explore_schmitt_across_ice_mask() {
    use crate::sla::config::CapacityType;
    use crate::sla::cost::{CostTable, RatioEma};
    use crate::sla::solve::{self, SolveFullResult};
    use std::cell::Cell as StdCell;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let h_main: String = "intel-8".into();
    let h_exp: String = "intel-6".into();
    actor
        .sla_config
        .hw_classes
        .retain(|k, _| *k == h_main || *k == h_exp);
    actor.sla_config.hw_explore_epsilon = 1.0;
    actor.sla_config.hw_cost_tolerance = 0.15;
    let tau = actor.sla_config.hw_cost_tolerance;
    actor.test_inject_ready("d-schmitt-ice", Some("test-pkg"), "x86_64-linux", false);
    let fit = make_fit("test-pkg");

    // Per-poll inputs_gen bump scaffolding: each `set_ratio` call
    // increments a counter folded into the spot price so the hash
    // changes even when `od_over_spot` repeats across polls (poll 4→5).
    // Step ≥1e-4 so `solve_relevant_hash`'s `(v*1e4).round()`
    // quantization sees it.
    let poll_n = StdCell::new(0u32);
    let set_ratio = |a: &DagActor, od_over_spot: f64| {
        let n = poll_n.get();
        poll_n.set(n + 1);
        let mut ct = a.cost_table.write();
        *ct = CostTable::from_parts(
            [
                ((h_main.clone(), CapacityType::Spot), 0.001),
                ((h_main.clone(), CapacityType::Od), 0.001),
                (
                    (h_exp.clone(), CapacityType::Spot),
                    1.0 + f64::from(n) * 1e-3,
                ),
                ((h_exp.clone(), CapacityType::Od), od_over_spot),
            ]
            .into(),
            [(
                h_exp.clone(),
                RatioEma {
                    numerator: 0.0,
                    denominator: 1e15,
                    updated_at: 0.0,
                },
            )]
            .into(),
        );
    };
    let sla_tiers = actor.sla_tiers.clone();
    let sla_ceilings = actor.sla_ceilings.clone();
    // Cloned with the un-clamped h_exp ceiling — `e_ratio` is only
    // called for `exp_feasible=true` polls so this snapshot suffices.
    let sla_config_feasible = actor.sla_config.clone();
    let e_ratio = |hw: &crate::sla::hw::HwTable, cost: &CostTable| -> f64 {
        let SolveFullResult::Feasible(m) = solve::solve_full(
            &fit,
            &sla_tiers,
            hw,
            cost,
            &sla_ceilings,
            &sla_config_feasible,
            std::slice::from_ref(&h_exp),
            &std::collections::HashSet::new(),
            true,
        ) else {
            panic!("h_exp restricted solve must be feasible")
        };
        let e = |cap| {
            m.all_candidates
                .iter()
                .find(|c| c.cell.1 == cap)
                .unwrap_or_else(|| panic!("({h_exp},{cap:?}) candidate present"))
                .e_cost_upper
        };
        e(CapacityType::Od) / e(CapacityType::Spot)
    };

    let state = actor.dag.node("d-schmitt-ice").unwrap();
    let cell_spot: crate::sla::config::Cell = (h_exp.clone(), CapacityType::Spot);
    let cell_od: crate::sla::config::Cell = (h_exp.clone(), CapacityType::Od);

    // ── poll 1: od/spot=1.14 ≤ τ_enter → Hit, 2 terms ────────────────
    set_ratio(&actor, 1.14);
    let (hw, cost, g0) = actor.solve_inputs();
    let r = e_ratio(&hw, &cost);
    assert!(r <= 1.0 + tau, "fixture: e_od/e_spot={r:.4} ≤ τ_enter");
    assert_eq!(
        actor
            .solve_intent_for(state, &hw, &cost, g0)
            .node_affinity
            .len(),
        2,
        "poll 1: od/spot=1.14 → Hit{{spot,od}} (h_exp pinned); prev_a={{spot,od}}"
    );

    // ── poll 2: ICE-mask (h_exp,*); od/spot=1.20 → Miss-Feasible ─────
    actor.ice.mark(&cell_spot);
    actor.ice.mark(&cell_od);
    assert!(
        actor.ice.masked_cells().contains(&cell_spot)
            && actor.ice.masked_cells().contains(&cell_od),
        "precondition: both (h_exp,*) ICE-masked"
    );
    set_ratio(&actor, 1.20);
    let (hw, cost, g1) = actor.solve_inputs();
    assert_ne!(g1, g0, "menu change → inputs_gen bump");
    let r = e_ratio(&hw, &cost);
    assert!(r > 1.0 + 1.3 * tau, "fixture: e_od/e_spot={r:.4} > τ_stay");
    let intent = actor.solve_intent_for(state, &hw, &cost, g1);
    assert!(
        intent.hw_class_names.contains(&h_main),
        "poll 2: Feasible{{spot}} all-masked → Miss → fall through to \
         unrestricted memo; h_main present. Got hw={:?}",
        intent.hw_class_names
    );

    // ── poll 3: clear ICE; od/spot=1.18 (deadband) → 1 term ──────────
    // **bug_001 falsification.**
    actor.ice.clear(&cell_spot);
    actor.ice.clear(&cell_od);
    assert!(actor.ice.masked_cells().is_empty(), "ICE cleared");
    set_ratio(&actor, 1.18);
    let (hw, cost, g2) = actor.solve_inputs();
    assert_ne!(g2, g1);
    let r = e_ratio(&hw, &cost);
    assert!(
        r > 1.0 + tau && r <= 1.0 + 1.3 * tau,
        "fixture: e_od/e_spot={r:.4} ∈ deadband"
    );
    assert_eq!(
        actor
            .solve_intent_for(state, &hw, &cost, g2)
            .node_affinity
            .len(),
        1,
        "poll 3: od/spot=1.18 in deadband; prev_a={{spot}} from poll 2's \
         Feasible-all-masked Miss → od τ_enter=1.15 → 1.18>1.15 → 1 term. \
         bug_001 @ 36804895: `_ =>` arm dropped `m.a.cells`; \
         `next==prev_pin` committed STALE {{spot,od}} from poll 1 → od \
         τ_stay=1.195 → 2 terms."
    );

    // ── poll 4: od/spot=1.14 → Hit, 2 terms; re-seed prev_a ──────────
    set_ratio(&actor, 1.14);
    let (hw, cost, g3) = actor.solve_inputs();
    assert_ne!(g3, g2);
    assert_eq!(
        actor
            .solve_intent_for(state, &hw, &cost, g3)
            .node_affinity
            .len(),
        2,
        "poll 4: od/spot=1.14 → Hit{{spot,od}}; prev_a={{spot,od}}"
    );

    // ── poll 5: h_exp class-ceiling → BestEffort → Miss-preserve ─────
    // Clamp h_exp's per-class ceiling so evaluate_cell returns
    // ClassCeiling for any (c*≥1, mem≥1). The observed-menu sample no
    // longer gates capacity (bug_033). `solve_intent_for` reads
    // `actor.sla_config` live.
    actor.sla_config.hw_classes.get_mut(&h_exp).unwrap().max_mem = Some(1);
    let state = actor.dag.node("d-schmitt-ice").unwrap();
    set_ratio(&actor, 1.14);
    let (hw, cost, g4) = actor.solve_inputs();
    assert_ne!(g4, g3);
    assert!(
        matches!(
            solve::solve_full(
                &fit,
                &sla_tiers,
                &hw,
                &cost,
                &sla_ceilings,
                &actor.sla_config,
                std::slice::from_ref(&h_exp),
                &std::collections::HashSet::new(),
                true,
            ),
            SolveFullResult::BestEffort { .. }
        ),
        "fixture: h_exp.max_mem=1 → restricted solve_full([h_exp]) BestEffort"
    );
    let intent = actor.solve_intent_for(state, &hw, &cost, g4);
    assert!(
        intent.hw_class_names.contains(&h_main),
        "poll 5: BestEffort → Miss → fall through; h_main present"
    );

    // ── poll 6: restore; od/spot=1.16 (deadband) → 2 terms ───────────
    // Regression guard for the BestEffort `Miss` arm: singleton →
    // preserve prev_a={spot,od} from poll 4; clear → od τ_enter → 1.
    actor.sla_config.hw_classes.get_mut(&h_exp).unwrap().max_mem = Some(256 << 30);
    let state = actor.dag.node("d-schmitt-ice").unwrap();
    set_ratio(&actor, 1.16);
    let (hw, cost, g5) = actor.solve_inputs();
    assert_ne!(g5, g4);
    let r = e_ratio(&hw, &cost);
    assert!(
        r > 1.0 + tau && r <= 1.0 + 1.3 * tau,
        "fixture: e_od/e_spot={r:.4} ∈ deadband"
    );
    assert_eq!(
        actor
            .solve_intent_for(state, &hw, &cost, g5)
            .node_affinity
            .len(),
        2,
        "poll 6: od/spot=1.16 in deadband; prev_a={{spot,od}} preserved \
         across poll 5's BestEffort-Miss (singleton) → od τ_stay → 2 terms. \
         Regression guard: a `Miss`-BestEffort arm that CLEARS prev_a → od \
         τ_enter → 1.16>1.15 → 1 term."
    );
}

/// **R6B4 / bug_012** — `FittedParams.n_eff` was changed to the
/// post-p̄-filter value (correct for `z_q`), but the dispatch gates at
/// `snapshot.rs:778` + `solve.rs:413` still test `< 3.0` with
/// PRE-filter calibration. A Capped fit with 5 ring samples but only 2
/// surviving the p̄ collinearity drop is a VALID fit (the comment at
/// ingest.rs:299-305 says so explicitly: "a 2-row post-filter fit gets
/// the widest prediction interval rather than being rejected outright")
/// — yet both gates reject it and dispatch at explore-ladder size.
///
/// This test seeds exactly that fit and asserts the actor dispatches
/// via `solve_full` (`node_affinity` non-empty) at `c* ≤ p̄`, NOT via
/// `explore::next` at `max_c`. Red on e23e1d1f: `n_eff=2.0 < 3.0` →
/// gate rejects → explore returns `max_c=32`, `node_affinity=[]`.
#[tokio::test]
async fn contract_dispatch_accepts_2row_postfilter_fit() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    // Capped fit, p̄=8: ring had 5 samples at c∈{4,8,16,32,32} (pre-
    // filter n_eff≈5, span=8), p̄ filter kept only c≤8 → 2 post-filter
    // rows → stored `n_eff` is the post-filter `2.0`. ExploreState
    // `max_c=32, min_c=4` so `frozen()` (span≥4) → explore would
    // dispatch at 32.
    let mut fit = make_fit("capped-2row");
    fit.fit = crate::sla::types::DurationFit::Capped {
        s: crate::sla::types::RefSeconds(30.0),
        p: crate::sla::types::RefSeconds(2000.0),
        p_bar: crate::sla::types::RawCores(8.0),
    };
    fit.n_eff_ring = crate::sla::types::RingNEff(5.0);
    fit.fit_df = crate::sla::types::FitDf(2.0);
    fit.n_distinct_c = 2;
    fit.sum_w = 2.0;
    fit.span = 8.0;
    fit.explore = crate::sla::types::ExploreState {
        distinct_c: 5,
        min_c: crate::sla::types::RawCores(4.0),
        max_c: crate::sla::types::RawCores(32.0),
        saturated: false,
        last_wall: crate::sla::types::WallSeconds(280.0),
    };
    actor.sla_estimator.seed(fit);
    actor.test_inject_ready("d-2row", Some("capped-2row"), "x86_64-linux", false);

    let state = actor.dag.node("d-2row").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);

    assert!(
        !intent.node_affinity.is_empty(),
        "Capped fit with 2 post-filter rows MUST reach `solve_full` \
         (non-Probe ⟹ n_eff_ring≥3 ∧ span≥4 already held at \
         ingest.rs:306). Got node_affinity=[] → snapshot.rs gate \
         rejected on post-filter n_eff and fell through to intent_for."
    );
    assert!(
        intent.cores <= 8,
        "fit-derived dispatch MUST respect p̄=8; got cores={} — \
         explore-ladder dispatched at max_c instead of c*≤p̄",
        intent.cores
    );
}

/// **F5** — `SolvedIntent.disk_headroom` carries the scheduler-side
/// `headroom(fit.n_eff_ring)` curve so the controller's
/// `pod_ephemeral_request` is variance-aware without reimplementing
/// it. Low `n_eff` (cold/noisy fit) → wide cushion; high `n_eff` →
/// tight; unfitted (no pname) → flat 1.5× fallback.
#[tokio::test]
async fn spawn_intent_carries_disk_headroom() {
    use crate::sla::fit::headroom;
    use crate::sla::types::RingNEff;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    // Fitted, n_eff=100 → headroom≈1.32 (tight: model is confident).
    let mut hi = make_fit("hi-neff");
    hi.n_eff_ring = RingNEff(100.0);
    actor.sla_estimator.seed(hi);
    // Fitted, n_eff=3 → headroom≈1.65 (wide: model is noisy).
    let mut lo = make_fit("lo-neff");
    lo.n_eff_ring = RingNEff(3.0);
    actor.sla_estimator.seed(lo);

    actor.test_inject_ready("d-hi", Some("hi-neff"), "x86_64-linux", false);
    actor.test_inject_ready("d-lo", Some("lo-neff"), "x86_64-linux", false);
    actor.test_inject_ready("d-cold", None, "x86_64-linux", false);

    let solve = |hash: &str| solve_intent(&actor, actor.dag.node(hash).unwrap()).disk_headroom;

    let h_hi = solve("d-hi");
    let h_lo = solve("d-lo");
    let h_cold = solve("d-cold");

    assert!(
        (h_hi - headroom(RingNEff(100.0))).abs() < 1e-9,
        "high-n_eff: want headroom(100)≈1.32, got {h_hi}"
    );
    assert!(
        (h_lo - headroom(RingNEff(3.0))).abs() < 1e-9,
        "low-n_eff: want headroom(3)≈1.65, got {h_lo}"
    );
    assert!(
        h_lo > h_hi,
        "low-n_eff fit MUST yield wider headroom than high-n_eff; \
         got lo={h_lo} hi={h_hi}"
    );
    assert_eq!(h_cold, 1.5, "unfitted (no pname) → flat fallback");
}

/// **R6B5 / merged_bug_011-A** — `pinned_explore` releases on
/// infeasible. Pre-fix: the pin is committed at :886-888 BEFORE the
/// `solve_full([h])` feasibility check at :891, and the graduation
/// filter at :867 only releases on `h ∈ A` or `h ∉ h_all` — neither
/// holds for an envelope-infeasible `h` (it's never in A by
/// definition). So a `BestEffort` draw is permanently pinned: every
/// subsequent ε_h hit reads `prev_pin = Some(h_dead)`, re-tries
/// `solve_full([h_dead])`, gets `BestEffort` again, falls through.
///
/// This test forces every `solve_full` to `BestEffort` (S=2000 >
/// p90=1200 → SerialFloor at every cell) so `pool = H\{cheapest}` (2
/// elements) and EVERY ε_h draw is infeasible. Tick once → record
/// `pinned_explore`; tick again → assert it CHANGED. Pre-fix: poll 1
/// commits `h0`, poll 2 reads `prev_pin=h0`, filter passes (h0 ∈
/// h_all, h0 ∉ in_a={}), uses `h0` again → stuck-same. Post-fix:
/// `resolve_h_explore` rotates on `Miss` → poll 1 commits
/// `next=h1≠h0`, poll 2 tries `h1`, rotates to `h0` → alternates.
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn contract_pinned_explore_releases_on_infeasible() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.hw_explore_epsilon = 1.0;
    // S=2000 > p90=1200 → T(c)≥S ∀c → every cell rejected on serial
    // floor → `solve_full` is `BestEffort` for the unrestricted memo
    // AND for every restricted `[h_explore]` solve.
    let mut fit = make_fit("infeasible");
    fit.fit = crate::sla::types::DurationFit::Amdahl {
        s: crate::sla::types::RefSeconds(2000.0),
        p: crate::sla::types::RefSeconds(0.0),
    };
    let mkh = crate::sla::solve::model_key_hash(&fit.key);
    actor.sla_estimator.seed(fit);
    actor.test_inject_ready("d-inf", Some("infeasible"), "x86_64-linux", false);
    let state = actor.dag.node("d-inf").unwrap();

    let (hw, cost, ig) = actor.solve_inputs();
    let _ = actor.solve_intent_for(state, &hw, &cost, ig);
    let p1 = actor
        .solve_cache
        .peek_entry(mkh, 0)
        .expect("ε_h block reached → MemoEntry exists")
        .pinned_explore;
    assert!(
        p1.is_some(),
        "precondition: ε=1.0 + |H|>1 + BestEffort memo → in_a={{}} → \
         pool=H\\{{cheapest}} (2 elements) → pin written"
    );

    let _ = actor.solve_intent_for(state, &hw, &cost, ig);
    let p2 = actor.solve_cache.peek_entry(mkh, 0).unwrap().pinned_explore;
    assert_ne!(
        p1, p2,
        "infeasible `h_explore` MUST release the pin (rotate to \
         pool\\{{h_tried}}), not stick. Pre-R6B5: pin committed at \
         :886-888 BEFORE feasibility check → poll 2 reads prev_pin={p1:?}, \
         graduation filter passes (∈h_all ∧ ∉in_a={{}}), re-tries same h."
    );
    // Three more ticks: pin keeps rotating (never stuck on any one
    // infeasible h). With pool.len()=2 it alternates p1↔p2.
    let mut prev = p2;
    for _ in 0..3 {
        let _ = actor.solve_intent_for(state, &hw, &cost, ig);
        let cur = actor.solve_cache.peek_entry(mkh, 0).unwrap().pinned_explore;
        assert_ne!(cur, prev, "rotation continues — never stuck");
        prev = cur;
    }
}

/// **R7B0 / merged_bug_001** — `pinned_explore` rotation covers the
/// FULL pool at `|pool|≥3`, not a 2-cycle. Same all-infeasible setup
/// as `_releases_on_infeasible` but with 4 hw_classes →
/// `pool=H\{cheapest}` has 3 elements. Drive 3·|pool| polls and
/// assert every pool element appears in `pinned_explore`. Pre-R7B0:
/// rotation `.choose(&mut pin_rng)` with pin_rng fresh-seeded +
/// unconsumed on `Some(h)=>h` → 2-cycle → one of the 3 starved.
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn contract_pinned_explore_covers_pool() {
    use crate::sla::config::{HwClassDef, NodeLabelMatch};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // 4th hw_class → |pool| = |H\{cheapest}| = 3. All hw factors = 1.0
    // so T(c)/factor = 2000 > p90=1200 at every cell → in_a = ∅ →
    // pool = H\{cheapest} (NOT H\A).
    actor.sla_config.hw_classes.insert(
        "intel-9".into(),
        HwClassDef {
            labels: vec![NodeLabelMatch {
                key: "rio.build/hw-class".into(),
                value: "intel-9".into(),
            }],
            max_cores: Some(actor.sla_config.max_cores as u32),
            max_mem: Some(actor.sla_config.max_mem),
            ..Default::default()
        },
    );
    let m: std::collections::HashMap<_, _> = ["intel-6", "intel-7", "intel-8", "intel-9"]
        .into_iter()
        .map(|h| (h.into(), 1.0))
        .collect();
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));
    actor.sla_config.hw_explore_epsilon = 1.0;
    // S=2000 > p90=1200 → SerialFloor at every cell → in_a=∅ →
    // pool = H\{cheapest} (3 elements).
    let mut fit = make_fit("infeasible-4");
    fit.fit = crate::sla::types::DurationFit::Amdahl {
        s: crate::sla::types::RefSeconds(2000.0),
        p: crate::sla::types::RefSeconds(0.0),
    };
    let mkh = crate::sla::solve::model_key_hash(&fit.key);
    actor.sla_estimator.seed(fit);
    actor.test_inject_ready("d-inf4", Some("infeasible-4"), "x86_64-linux", false);
    let state = actor.dag.node("d-inf4").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();

    let h_all: std::collections::BTreeSet<_> =
        actor.sla_config.hw_classes.keys().cloned().collect();
    assert_eq!(h_all.len(), 4, "precondition: 4 hw_classes");
    let cheapest = cost.cheapest_h(&h_all).expect("non-empty");
    let want_pool: std::collections::BTreeSet<_> =
        h_all.iter().filter(|h| **h != cheapest).cloned().collect();
    assert_eq!(want_pool.len(), 3, "precondition: |pool|=3");

    let mut seen = std::collections::BTreeSet::new();
    for _ in 0..(3 * want_pool.len()) {
        let _ = actor.solve_intent_for(state, &hw, &cost, ig);
        if let Some(p) = actor.solve_cache.peek_entry(mkh, 0).unwrap().pinned_explore {
            seen.insert(p);
        }
    }
    assert_eq!(
        seen, want_pool,
        "round-robin over sorted(pool) covers every element in |pool| \
         consecutive misses; pre-R7B0 2-cycle starves |pool|-2 of {want_pool:?}"
    );
}

/// **R6B5 / merged_bug_011-B** — pinned `h_explore` fully ICE-masked
/// routes around via the unrestricted memo. Pre-fix: `solve_full([h])`
/// is `Feasible` → early-return at :905 binds `memo` to the ≤2-cell
/// explore result. The masked-filter at :927-933 reduces it to `[]`;
/// the all-masked fallback at :966-969 returns `memo.a.cells` — which
/// is STILL the masked `{h_explore}` cells, not the unrestricted A.
/// The drv emits `node_affinity` over known-unfulfillable cells while
/// the unrestricted A (cached in `solve_cache[mkh][ovr].result`) sits
/// unused. No `_hw_ladder_exhausted_total` (only 2/|H×2| masked).
///
/// This test pins `h0`, masks both `(h0,*)` cells, then asserts the
/// emitted `hw_class_names` are NOT exclusively `{h0}` — at least one
/// cell from the unrestricted memo is offered.
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn contract_pinned_explore_routes_around_ice() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.hw_explore_epsilon = 1.0;
    actor.test_inject_ready("d-ice", Some("test-pkg"), "x86_64-linux", false);
    let state = actor.dag.node("d-ice").unwrap();

    let h_of = |intent: &crate::state::SolvedIntent| -> std::collections::BTreeSet<String> {
        intent.hw_class_names.iter().cloned().collect()
    };

    // Poll 1: ε=1.0 → explore branch fires, pins h0, emits `{h0}`.
    let (hw, cost, ig) = actor.solve_inputs();
    let h0_set = h_of(&actor.solve_intent_for(state, &hw, &cost, ig));
    assert_eq!(h0_set.len(), 1, "ε=1.0 explore → A' ⊆ {{h_explore}}×{{*}}");
    let h0 = h0_set.into_iter().next().unwrap();
    // Unrestricted A (ε=0 read of the memo) — what the fallback SHOULD
    // route to. The memo is already filled; ε=0 skips the explore
    // block and returns it directly.
    actor.sla_config.hw_explore_epsilon = 0.0;
    let in_a = h_of(&actor.solve_intent_for(state, &hw, &cost, ig));
    assert!(
        !in_a.is_empty() && !in_a.iter().all(|h| *h == h0),
        "precondition: unrestricted A has at least one h ≠ {h0} \
         (otherwise the route-around has nowhere to go); A={in_a:?}"
    );
    actor.sla_config.hw_explore_epsilon = 1.0;

    // Mask both (h0,*) cells — the controller's `unfulfillable_cells`
    // ack path.
    for cap in CapacityType::ALL {
        actor.ice.mark(&(h0.clone(), cap));
    }
    assert!(
        actor
            .ice
            .masked_cells()
            .contains(&(h0.clone(), CapacityType::Spot))
            && actor
                .ice
                .masked_cells()
                .contains(&(h0.clone(), CapacityType::Od)),
        "precondition: both (h0,*) ICE-masked"
    );

    // Poll 2: pin=h0, solve_full([h0]) Feasible, but both cells masked.
    let emitted = h_of(&actor.solve_intent_for(state, &hw, &cost, ig));
    assert!(
        emitted.iter().any(|h| *h != h0),
        "pinned `{h0}` fully ICE-masked → MUST route around via the \
         unrestricted memo. Got hw_class_names={emitted:?} — all `{h0}` \
         (the masked cells). Pre-R6B5: early-return at :905 binds `memo` \
         to the 2-cell explore result; all-masked fallback at :966 \
         re-emits those masked cells instead of the cached unrestricted A."
    );
    assert!(
        emitted.iter().all(|h| in_a.contains(h)),
        "routed-around cells ⊆ unrestricted A; got {emitted:?}, A={in_a:?}"
    );
}

/// **R6B5 / bug_004** — `pinned_explore` is independent of which drv
/// writes it first. The pin is stored at `(mkh, ovr)` granularity;
/// its VALUE must be a pure function of `(mkh, ovr, pool)`. Pre-R6B5
/// the value was `pool.choose(&mut per-drv-rng)` — whichever heads-drv
/// `dag.iter_nodes()` (HashMap, RandomState) reached first seeded the
/// shared slot from its OWN `drv_hash`. Two scheduler replicas (or one
/// across a restart) with the same `(mkh, ovr)` but different
/// drv-hash populations would pin different `h_explore` →
/// `reap_stale_for_intents` churns the explore Job on every leader
/// flip. REVIEW.md §HashMap-iteration-order, write-side.
///
/// Actor-boundary mirror of `explore::resolve_pool_permutation_
/// independent` (which proves `resolve_h_explore` itself is
/// pool-order-independent): two independent actor instances, DISJOINT
/// drv-hash sets, ONE shared `(mkh, ovr)` (same pname/system/tenant,
/// no override), ε=1.0 → both `compute_spawn_intents` runs MUST
/// commit identical `pinned_explore` for that key. NOT "different DAG
/// insertion orders on one actor" — `dag.nodes` is HashMap with
/// per-process RandomState, so insertion order is irrelevant and that
/// test would be vacuous. Two actors = two RandomStates = the real
/// nondeterminism axis.
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn contract_pinned_explore_first_writer_independent() {
    let db = TestDb::new(&MIGRATOR).await;
    let mkh = crate::sla::solve::model_key_hash(&make_fit("test-pkg").key);

    // Two independent actors: each its own DagActor (own `dag` HashMap
    // with own RandomState, own `solve_cache`). Both share pname
    // "test-pkg" → same `mkh`; `ovr=0` (no override). Drv-hash sets
    // are disjoint AND multi-element so the per-actor first-writer is
    // (a) different across actors and (b) iteration-order-dependent
    // within each.
    let mut a = bare_actor_hw(db.pool.clone());
    let mut b = bare_actor_hw(db.pool.clone());
    a.sla_config.hw_explore_epsilon = 1.0;
    b.sla_config.hw_explore_epsilon = 1.0;
    for i in 0..5 {
        a.test_inject_ready(
            &format!("drv-a-{i:02}"),
            Some("test-pkg"),
            "x86_64-linux",
            false,
        );
        b.test_inject_ready(
            &format!("drv-b-{i:02}"),
            Some("test-pkg"),
            "x86_64-linux",
            false,
        );
    }

    // One controller poll each → ε=1.0 → every drv hits the explore
    // branch; the first to reach `update_entry` writes the pin.
    let _ = a.compute_spawn_intents(&Default::default());
    let _ = b.compute_spawn_intents(&Default::default());

    let pin_a = a
        .solve_cache
        .peek_entry(mkh, 0)
        .expect("actor a: ε=1.0 + |H|>1 → explore reached → MemoEntry exists")
        .pinned_explore;
    let pin_b = b
        .solve_cache
        .peek_entry(mkh, 0)
        .expect("actor b: MemoEntry exists")
        .pinned_explore;
    assert!(
        pin_a.is_some(),
        "precondition: pool=H\\A non-empty → pin written"
    );
    assert_eq!(
        pin_a, pin_b,
        "two actors, disjoint drv-hash sets, same (mkh,ovr) → \
         `pinned_explore` MUST be identical. Pre-R6B5 the pin VALUE was \
         seeded from per-drv `drv_hash` → first-writer-dependent → \
         got a={pin_a:?} b={pin_b:?}. Post-R6B5 seed is `mkh ^ ovr` → \
         pure function of the storage key."
    );

    // And the emitted `h_explore` (observable on the wire) agrees:
    // every intent's `hw_class_names` is `{pin}` for both actors.
    let h_of = |actor: &DagActor| -> std::collections::BTreeSet<String> {
        actor
            .compute_spawn_intents(&Default::default())
            .intents
            .into_iter()
            .flat_map(|i| i.hw_class_names)
            .collect()
    };
    assert_eq!(
        h_of(&a),
        h_of(&b),
        "wire-visible `hw_class_names` agree across actors — the \
         controller's `reap_stale_for_intents` sees the same fingerprint \
         regardless of which replica answered"
    );
}

/// **R6B6 / bug 021** — `InterruptRunaway` is reachable from
/// `solve_full` at the actor boundary. Pre-fix: `classify_best_effort`
/// reads ONE mixed-cap `rejects` vec; `cap_c.max(1.0)` (R5B6) means OD
/// can NEVER produce `LambdaGate | CLoExceedsCap`, so `all(λ-adjacent)`
/// over the mixed vec is structurally always-false → falls through to
/// `classify_ceiling` → emits `core_ceiling` instead.
///
/// This test sets λ runaway (every spot cell `LambdaGate`) + OD
/// `ClassCeiling` (the unrelated config-drift reason OD failed) → the
/// semantic case observability.md:156 documents. Red on 6eab30da:
/// `infeasible_counts["interrupt_runaway"] == 0`, `core_ceiling == 1`.
#[tokio::test]
async fn contract_interrupt_runaway_reachable() {
    use crate::sla::cost::RatioEma;
    use crate::sla::metrics::infeasible_counts;
    use crate::sla::solve::InfeasibleReason;

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.hw_explore_epsilon = 0.0;

    // λ_hat ≈ (1e6 + 86400·seed)/(1 + 86400) ≈ 11.6/s. make_fit's S=30
    // → T(cap_c) ≥ 30 → p(cap_c) = 1-e^{-11.6·30} ≈ 1.0 > 0.5 →
    // every (h, Spot) cell LambdaGate.
    // OD: λ=0, c_lo=1, envelope feasible (S=30 vs p90=1200), mem 6GiB
    // < global ceil 256GiB, but per-class max_mem=1 → ClassCeiling
    // (the configured per-class catalog ceiling; observed-menu sample
    // no longer gates capacity).
    {
        let mut ct = actor.cost_table.write();
        *ct = crate::sla::cost::CostTable::from_parts(
            std::collections::HashMap::new(),
            ["intel-6", "intel-7", "intel-8"]
                .into_iter()
                .map(|h| {
                    (
                        h.into(),
                        RatioEma {
                            numerator: 1e6,
                            denominator: 1.0,
                            updated_at: 0.0,
                        },
                    )
                })
                .collect(),
        );
    }
    for h in ["intel-6", "intel-7", "intel-8"] {
        actor.sla_config.hw_classes.get_mut(h).unwrap().max_mem = Some(1);
    }
    actor.test_inject_ready("d-runaway", Some("test-pkg"), "x86_64-linux", false);

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);

    let _ = actor.compute_spawn_intents(&Default::default());
    let m = infeasible_counts(&snap);
    assert_eq!(
        m.get(InfeasibleReason::InterruptRunaway.as_str())
            .copied()
            .unwrap_or(0),
        1,
        "λ runaway (every spot LambdaGate) + OD ClassCeiling → \
         `why == InterruptRunaway`. Pre-R6B6: classify_best_effort's \
         `all(λ-adjacent)` reads mixed-cap rejects; OD's ClassCeiling \
         poisons it → classify_ceiling → CoreCeiling. Got {m:?}"
    );
    assert_eq!(
        m.get(InfeasibleReason::CoreCeiling.as_str())
            .copied()
            .unwrap_or(0),
        0,
        "OD's ClassCeiling is reported via classify_ceiling SEPARATELY (it \
         isn't here — envelope feasible + mem under global ceil → CoreCeiling \
         is the wrong label for 'spot λ-gated'). Got {m:?}"
    );
}

/// **R19B0 / bug_001** — `compute_spawn_intents` output order is
/// deterministic across `(ready, priority)` ties. The outer
/// `sort_unstable_by` at snapshot.rs:~505 keys on `(ready, prio)` only;
/// equal-prio intents (sourced from HashMap-order `dag.iter_nodes()`)
/// fall through to `Equal` and `sort_unstable_by` does NOT preserve
/// input order on ties → two scheduler replicas (or one across a
/// restart) emit the same drvs in DIFFERENT order → controller's
/// `.take(headroom)` truncates a different subset. Separately, the
/// forecast pass's bug_025 `(prio, c*, hash)` key is destroyed by the
/// re-sort. REVIEW.md §HashMap-iteration: tiebreak `(cores desc,
/// intent_id asc)`.
///
/// TWO-ACTOR pattern (mirrors
/// [`contract_pinned_explore_first_writer_independent`]): NOT
/// same-actor re-insert — `dag.nodes` is std HashMap with per-process
/// RandomState, so re-inserting the same keys on ONE actor lands in
/// the same buckets → same iter order → vacuous. Two actors = two
/// RandomStates = the real nondeterminism axis.
#[tokio::test]
async fn contract_spawn_intents_order_deterministic_across_ties() {
    let db = TestDb::new(&MIGRATOR).await;

    // Per-pname forced_cores so each drv solves to a DISTINCT `cores`
    // (override path → hw-agnostic `intent_for`; deterministic, no
    // ε_h). All at `priority=5.0` → the old `(ready, prio)` key
    // returns Equal for every pair within a `ready` partition.
    let overrides = [
        ("p32", 32.0),
        ("p16", 16.0),
        ("p04", 4.0),
        ("p08", 8.0),
        ("p02", 2.0),
    ]
    .into_iter()
    .map(|(pn, c)| crate::db::SlaOverrideRow {
        pname: pn.into(),
        cores: Some(c),
        ..Default::default()
    })
    .collect::<Vec<_>>();

    let build = || {
        use crate::sla::config::CapacityType;
        let mut sla = test_sla_config();
        sla.lead_time_seed
            .insert(("intel-7".into(), CapacityType::Spot), 200.0);
        sla.max_forecast_cores_per_tenant = 2_000;
        let mut actor = bare_actor_cfg(
            db.pool.clone(),
            DagActorConfig {
                sla,
                ..Default::default()
            },
        );
        actor.sla_estimator.seed_overrides(overrides.clone());
        // 3 Ready, all priority=5.0, cores={32,4,16}. Hash strings
        // chosen so `intent_id asc` ≠ `cores desc` (proves the
        // tiebreak is cores-first, not just intent_id).
        for (h, pn) in [("r-a", "p32"), ("r-b", "p04"), ("r-c", "p16")] {
            actor.test_inject_ready(h, Some(pn), "x86_64-linux", false);
            actor.test_set_priority(h, 5.0);
        }
        // 2 forecast (Queued, dep on Running with eta≈30s <
        // max_lead=200), priority=5.0, cores={8,2}.
        actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Running);
        actor.test_set_running_eta("dep", 100.0, 70, 8);
        for (h, pn) in [("f-a", "p08"), ("f-b", "p02")] {
            actor.test_inject_ready(h, Some(pn), "x86_64-linux", false);
            actor
                .dag
                .node_mut(h)
                .unwrap()
                .set_status_for_test(DerivationStatus::Queued);
            actor.test_inject_edge(h, "dep");
            actor.test_set_priority(h, 5.0);
        }
        actor
    };

    let order = |actor: &DagActor| -> Vec<(String, bool, u32)> {
        actor
            .compute_spawn_intents(&Default::default())
            .intents
            .into_iter()
            .map(|i| (i.intent_id, i.ready.unwrap_or(true), i.cores))
            .collect()
    };

    let a = order(&build());
    let b = order(&build());

    // (a) determinism: two independent actors (own RandomState each)
    // emit identical intent_id order. At ad5d288e: tied-prio Ready
    // entries iter differently across the two HashMaps → fails.
    assert_eq!(
        a, b,
        "two actors, identical drv set, all priority=5.0 → output \
         order MUST be identical. Pre-R19B0 the outer sort has no \
         tiebreak past (ready, prio) → HashMap order leaks → \
         a={a:?} b={b:?}"
    );

    // (b) within ready=false: forecast pass's `(prio, c*, hash)` key
    // survives the outer re-sort → cores desc.
    let forecast: Vec<_> = a.iter().filter(|(_, r, _)| !r).collect();
    assert_eq!(
        forecast.iter().map(|(_, _, c)| *c).collect::<Vec<_>>(),
        vec![8, 2],
        "forecast partition: cores desc (bug_025 key preserved); got {forecast:?}"
    );

    // (c) within ready=true: cores desc, intent_id asc.
    let ready: Vec<_> = a.iter().filter(|(_, r, _)| *r).collect();
    assert_eq!(
        ready
            .iter()
            .map(|(id, _, c)| (id.as_str(), *c))
            .collect::<Vec<_>>(),
        vec![("r-a", 32), ("r-c", 16), ("r-b", 4)],
        "Ready partition: cores desc (r-a=32, r-c=16, r-b=4); got {ready:?}"
    );

    // Ready before forecast (the existing `ready desc` head key).
    assert!(
        a.iter().position(|(_, r, _)| !r).unwrap_or(a.len())
            > a.iter().rposition(|(_, r, _)| *r).unwrap_or(0),
        "all Ready intents precede all forecast intents; got {a:?}"
    );
}

/// **R19B2 / bug 033** — a `forced_mem`-only override (`--mem 200G`, no
/// `--cores`/`--tier`) MUST route the hw-agnostic `intent_for` arm,
/// same as `forced_cores`/`tier`. Pre-fix: the gate at snapshot.rs:770
/// excluded `forced_cores`/`tier` only → `--mem`-only entered
/// `solve_full`, which menu-fits cells against fit-derived mem (~6GiB),
/// then `forced_mem=200GiB` was overlaid post-match at :1043 →
/// `node_affinity` over cells checked at 6GiB, `mem_bytes=200GiB` →
/// permanently-Pending pod when no admitted cell's menu reaches 200GiB.
///
/// Post-fix: `forced_mem` joins the gate → `node_affinity` empty
/// (hw-agnostic) and `mem_bytes` is the forced value clamped at
/// `max_mem`. `intent_for` honors `forced_mem` internally; the post-hoc
/// overlay is deleted as unreachable.
#[tokio::test]
async fn contract_forced_mem_only_override_is_hw_agnostic() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // bare_actor_hw: hwCostSource=Static, 3 hw_classes, populated hw
    // table, fitted "test-pkg" (mem.p90=6GiB) — solve_full reachable.
    actor.test_inject_ready("d-mem-ovr", Some("test-pkg"), "x86_64-linux", false);
    // `--mem 200G` only: no cores, no tier.
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "test-pkg".into(),
            mem_bytes: Some(200 << 30),
            ..Default::default()
        }]);

    let state = actor.dag.node("d-mem-ovr").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);

    let max_mem = actor.sla_ceilings.max_mem;
    assert!(
        intent.node_affinity.is_empty(),
        "forced_mem-only override MUST gate solve_full off (hw-agnostic \
         intent_for arm). Got node_affinity={:?} → entered solve_full; \
         affinity menu-checked at fit-mem≈6GiB, request at 200GiB → \
         pod permanently Pending when admitted cells' menus < 200GiB.",
        intent.node_affinity
    );
    assert_eq!(
        intent.mem_bytes,
        (200u64 << 30).min(max_mem),
        "forced_mem MUST reach the intent (intent_for honors it \
         internally; the post-hoc overlay is deleted). max_mem={max_mem}"
    );
}

/// **bug_008** — bypass-path `--capacity` with a system NO configured
/// hw-class can host (`reference_hw_class_for_system → None`) MUST
/// emit empty `(hw_class_names, node_affinity)` so the controller's
/// `fallback_cell` reaches its OWN `None` → `no_hosting_class` metric.
/// The bug_039 fix's `.map_or_else(|| reference_hw_class.clone(), ..)`
/// fallback emitted the un-arch-matched reference into
/// `cells_to_selector_terms`, producing `nodeAffinity arch In [wrong]`
/// ANDed with the pod's nodeSelector — bug_039's permanently-Pending
/// symptom one input-space step removed (§Verifier-one-step-removed).
#[tokio::test]
async fn contract_bypass_capacity_no_arch_match_emits_empty() {
    use crate::sla::config::{ARCH_LABEL, NodeLabelMatch};

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Make every hw_class explicitly amd64 so the unmappable-system
    // case (riscv64-linux → system_to_k8s_arch=None) AND the
    // no-class-hosts-arch case both reduce to `None` here. Either
    // branch of `reference_hw_class_for_system` returning `None` must
    // emit empty.
    for d in actor.sla_config.hw_classes.values_mut() {
        d.labels.push(NodeLabelMatch {
            key: ARCH_LABEL.into(),
            value: "amd64".into(),
        });
    }
    actor.test_inject_ready("d-rv", Some("test-pkg"), "riscv64-linux", false);
    // `--cores=16` (bypass field) + `--capacity=on-demand`.
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "test-pkg".into(),
            cores: Some(16.0),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        }]);

    let state = actor.dag.node("d-rv").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);

    assert!(
        intent.hw_class_names.is_empty() && intent.node_affinity.is_empty(),
        "no-arch-match MUST emit empty so controller fallback_cell hits \
         no_hosting_class; got hw_class_names={:?} node_affinity={:?} — \
         a non-empty result here means the un-arch-matched \
         reference_hw_class was emitted (bug_039 on the None arm).",
        intent.hw_class_names,
        intent.node_affinity
    );
}

/// **bug_035** — `_hw_cost_unknown_total` fires once per `(key,
/// inputs_gen)` epoch, NOT twice on the memo-miss tick when ε_h hits.
/// The unrestricted `solve_full(.., &h_all, .., true)` already covers
/// `tiers × h_all × {spot,od}`; the restricted `solve_full(.., {h}, ..)`
/// iterates a strict subset, so its `emit_metrics` is unconditionally
/// redundant. Pre-fix: `was_miss` (true on the miss tick) gates the
/// restricted emit → 2× over `(h_explore, *)` ClassCeiling cells.
///
/// `|h_all|=3`, `|tiers|=1`, h0/h1 feasible, h2 per-class max_mem=1GiB
/// → in_a={h0,h1} → pool={h2} (singleton, deterministic). ε=1.0 forces
/// the explore branch. Expect 2 (one tier × {spot,od}); pre-fix: 4.
#[tokio::test]
async fn contract_hw_cost_unknown_once_per_epoch() {
    const HW_COST_UNKNOWN: &str = "rio_scheduler_sla_hw_cost_unknown_total";

    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.hw_explore_epsilon = 1.0;
    assert_eq!(actor.sla_tiers.len(), 1, "fixture: |tiers|=1");
    assert_eq!(actor.sla_config.hw_classes.len(), 3, "fixture: |h_all|=3");
    // intel-8 only: per-class max_mem=1GiB → make_fit("test-pkg")
    // mem (Independent{p90: 6 GiB}) > 1GiB → ClassCeiling. The
    // observed-menu sample no longer gates capacity (bug_033).
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-8")
        .unwrap()
        .max_mem = Some(1 << 30);
    actor.test_inject_ready("d-nofit", Some("test-pkg"), "x86_64-linux", false);
    let state = actor.dag.node("d-nofit").unwrap();

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);

    let (hw, cost, ig) = actor.solve_inputs();
    let _ = actor.solve_intent_for(state, &hw, &cost, ig);
    let d = counter_map(&snap);
    assert_eq!(
        d.get(HW_COST_UNKNOWN).copied().unwrap_or(0),
        2,
        "`_hw_cost_unknown_total` fires once per ClassCeiling cell per \
         (key, inputs_gen): 1 tier × {{spot,od}} on intel-8 = 2. Pre-fix \
         the ε_h restricted solve re-emits over `{{h}} ⊆ h_all` on the \
         miss tick → 4. Got {d:?}"
    );
}

/// **bug_019 / STRIKE-6** — bypass-path `--capacity` + `--cores=48` on
/// a system whose `reference_hw_class` has `max_cores=32` MUST emit a
/// `hw_class_names` set whose every member's per-class ceiling hosts
/// `(cores, mem)`. Pre-fix: `reference_hw_class_for_system` arch-matches
/// only (no size args) → emits the 32-core reference class → controller
/// `assign_to_cells` skips `fallback_cell` (non-empty `hw_class_names`)
/// → `cover::sizing` `exceeds_cell_cap`-drops it forever. Post-fix:
/// the producer size-filters AND the post-finalize chokepoint strips
/// any unhosting class regardless of producer.
#[tokio::test]
async fn contract_bypass_capacity_oversized_cores_emits_hosting_class() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // reference_hw_class=intel-6 with max_cores=32; intel-7/8 stay at
    // the global 64. `--cores=48` fits intel-7/8, not intel-6.
    actor.sla_config.reference_hw_class = "intel-6".into();
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-6")
        .unwrap()
        .max_cores = Some(32);
    assert_eq!(
        actor.sla_ceilings.max_cores as u32, 64,
        "fixture: global=64"
    );

    actor.test_inject_ready("d-big", Some("test-pkg"), "x86_64-linux", false);
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "test-pkg".into(),
            cores: Some(48.0),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        }]);

    let state = actor.dag.node("d-big").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);

    assert_eq!(intent.cores, 48, "forced cores reach the intent");
    assert!(
        !intent.hw_class_names.iter().any(|h| h == "intel-6"),
        "hw_class_names MUST NOT contain a class whose max_cores < cores. \
         Got {:?} with cores={} — intel-6.max_cores=32 cannot host 48; \
         controller would exceeds_cell_cap-drop forever (bug_019).",
        intent.hw_class_names,
        intent.cores
    );
    assert!(
        !intent.hw_class_names.is_empty(),
        "a hosting class exists (intel-7/8 max_cores=64 ≥ 48); the \
         producer should pick one, not emit empty. Got {:?}",
        intent.hw_class_names
    );
    assert_eq!(
        intent.node_affinity.len(),
        intent.hw_class_names.len(),
        "terms and names stay parallel through the chokepoint"
    );
}

/// **bug_019 §one-step-removed (a) inverse** — `--cores` larger than
/// EVERY configured class's `max_cores` MUST emit empty
/// `hw_class_names` so the controller's `fallback_cell` reaches its OWN
/// `None` → `no_hosting_class`. Pre-fix: `reference_hw_class_for_system`
/// returns the arch-matched reference regardless of size → non-empty →
/// controller never reaches `fallback_cell` → `exceeds_cell_cap` loop
/// instead of the operator-visible `no_hosting_class` signal.
#[tokio::test]
async fn contract_bypass_capacity_oversized_no_class_hosts_emits_empty() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.reference_hw_class = "intel-6".into();
    // Every class capped at 32; global at 64. `--cores=48` fits global,
    // fits NO per-class.
    for d in actor.sla_config.hw_classes.values_mut() {
        d.max_cores = Some(32);
    }

    actor.test_inject_ready("d-huge", Some("test-pkg"), "x86_64-linux", false);
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "test-pkg".into(),
            cores: Some(48.0),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        }]);

    let state = actor.dag.node("d-huge").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);

    assert_eq!(intent.cores, 48);
    assert!(
        intent.hw_class_names.is_empty() && intent.node_affinity.is_empty(),
        "no class hosts cores=48 (all max_cores=32) → MUST emit empty so \
         controller fallback_cell hits no_hosting_class. Got names={:?} \
         terms={:?} — non-empty here means exceeds_cell_cap loop instead \
         of the operator-visible metric.",
        intent.hw_class_names,
        intent.node_affinity
    );
}

/// **STRIKE-6 §one-step-removed (b) next-phase**: post-chokepoint
/// `(node_affinity, hw_class_names)` round-trips through
/// `handle_ack_spawned_intents`' `zip(hw_class_names, node_affinity)`
/// cell-reconstruction. The chokepoint shrinks both in lockstep, so a
/// shrunk pair must still be aligned — `names[i]` is the `h` whose
/// label conjunction produced `terms[i]`.
#[tokio::test]
async fn contract_chokepoint_preserves_term_name_alignment() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.reference_hw_class = "intel-6".into();
    // intel-6=32, intel-7=64, intel-8=64. cores=48 → intel-6 stripped.
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-6")
        .unwrap()
        .max_cores = Some(32);

    actor.test_inject_ready("d-align", Some("test-pkg"), "x86_64-linux", false);
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "test-pkg".into(),
            cores: Some(48.0),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        }]);

    let state = actor.dag.node("d-align").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);

    // Every surviving `(term, name)` pair: the term's hw-class label
    // value matches the name. This is the round-trip invariant
    // `handle_ack_spawned_intents` relies on.
    for (term, name) in intent.node_affinity.iter().zip(&intent.hw_class_names) {
        let hw_label = term
            .match_expressions
            .iter()
            .find(|r| r.key == "rio.build/hw-class")
            .expect("every term has hw-class label");
        assert_eq!(
            &hw_label.values[0], name,
            "term/name misaligned post-chokepoint — zip would reconstruct wrong cells"
        );
        let (cc, _) = actor.sla_config.class_ceilings(name, &Default::default());
        assert!(
            intent.cores <= cc,
            "every surviving class must host cores={}; {name}.max_cores={cc}",
            intent.cores
        );
    }
}

/// **§13c T10**: `required_features` routes via `hwClass.provides_features`
/// instead of the pre-§13c `state.required_features.is_empty()` bypass.
/// kvm intent + `{metal-x86: provides=[kvm], intel-*: provides=[]}` ⇒
/// `hw_class_names == ["metal-x86"]` only. Non-kvm intent ⇒ excludes
/// metal (∅-guard). Pre-fix: kvm → `[]` (gate kicked it to hw-agnostic);
/// non-kvm → ⊇ metal (no partition).
// r[verify sched.sla.hwclass.provides]
#[tokio::test]
async fn contract_kvm_routes_via_provides_features() {
    use crate::sla::config::{HwClassDef, NodeLabelMatch};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Add a metal hwClass with provides=[kvm]. The 3 intel-* classes
    // from bare_actor_hw have provides=[] (default).
    actor.sla_config.hw_classes.insert(
        "metal-x86".into(),
        HwClassDef {
            labels: vec![NodeLabelMatch {
                key: "rio.build/hw-class".into(),
                value: "metal-x86".into(),
            }],
            node_class: "rio-metal".into(),
            max_cores: Some(actor.sla_config.max_cores as u32),
            max_mem: Some(actor.sla_config.max_mem),
            provides_features: vec!["kvm".into()],
            ..Default::default()
        },
    );
    // Seed metal in the hw table so factor lookup succeeds.
    let m: std::collections::HashMap<_, _> = ["intel-6", "intel-7", "intel-8", "metal-x86"]
        .into_iter()
        .map(|h| (h.into(), 1.0))
        .collect();
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));

    actor.test_inject_ready_with_features("d-kvm", Some("test-pkg"), "x86_64-linux", &["kvm"]);
    actor.test_inject_ready("d-nokvm", Some("test-pkg"), "x86_64-linux", false);

    let snap = actor.compute_spawn_intents(&Default::default());
    let by_id = |id: &str| -> &rio_proto::types::SpawnIntent {
        snap.intents.iter().find(|i| i.intent_id == id).unwrap()
    };

    // kvm intent: hw_class_names == ["metal-x86"] ONLY.
    let kvm_intent = by_id("d-kvm");
    assert_eq!(
        kvm_intent.hw_class_names,
        vec!["metal-x86"],
        "kvm intent must route to metal-x86 ONLY (provides=[kvm]); got {:?}",
        kvm_intent.hw_class_names
    );
    assert!(
        !kvm_intent.node_affinity.is_empty(),
        "kvm intent gets full SLA-solve participation post-§13c"
    );

    // non-kvm intent: hw_class_names excludes metal (∅-guard).
    let nokvm_intent = by_id("d-nokvm");
    assert!(
        !nokvm_intent
            .hw_class_names
            .contains(&"metal-x86".to_string()),
        "non-kvm intent must exclude metal-x86 (∅-guard: required=[], \
         provides=[kvm] → incompatible); got {:?}",
        nokvm_intent.hw_class_names
    );
    assert!(
        !nokvm_intent.hw_class_names.is_empty(),
        "non-kvm intent still routes to intel-* classes"
    );
}
