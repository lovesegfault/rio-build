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

/// The counters `solve_intent_for` may emit. After §Third-strike and
/// arm-on-ack, `compute_spawn_intents` is **side-effect-free except
/// idempotent memo fill and debounced emits of these** — every other
/// counter write is a regression of merged_bug_001 / the validator's
/// r3 BLOCKED finding (per-poll over-emission).
///
/// The debounce gate is **per-counter**, not uniformly `was_miss`:
/// `_hw_cost_unknown` and `_infeasible{BestEffort.why}` are
/// `was_miss`-gated (memo inputs); `_hw_ladder_exhausted` and
/// `_infeasible{CapacityExhausted}` are ICE-edge-gated per R5B2
/// (read-time state, NOT in `inputs_gen`); `_infeasible` on the
/// hw-agnostic path is `fit_content_hash`-anchored per R5B3;
/// `_unroutable_features` is `unroutable_features_warned`-gated per
/// mb_031 (set on `DagActor`, keyed `(tenant, required_features)` —
/// fires before `was_miss` is even declared); `_forecast_dropped` is
/// `forecast_dropped_warned`-gated per r34 bug_018 (keyed
/// `(drv_hash, reason)` on `DagActor`). Each gate bounds the
/// counter to ≤1 per `model_key` per edge — the (1a) `≤ |Ready drvs|`
/// assertion holds for all of them.
const ONCE_PER_MISS: &[&str] = &[
    "rio_scheduler_sla_infeasible_total",
    "rio_scheduler_sla_hw_ladder_exhausted_total",
    "rio_scheduler_sla_hw_cost_unknown_total",
    "rio_scheduler_unroutable_features_total",
    "rio_scheduler_sla_forecast_dropped_total",
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    actor.cost_table.write().set_resolved_global((
        actor.sla_config.max_cores.unwrap() as u32,
        actor.sla_config.max_mem.unwrap(),
    ));
    actor.sla_ceilings = crate::sla::solve::Ceilings::from_resolved(
        &actor.sla_config,
        actor.cost_table.read().resolved_global(),
    );
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
    // mb_031: two `required_features` drvs with no hosting class (no
    // hwClass in `test_hw_sla_config` provides anything) so the (1a)
    // side-effect-free assertion exercises the unroutable emit branch.
    // Two with the SAME feature tuple → debounced bound is 1; per-poll
    // emission is `2×8=16 > 12=|Ready drvs|`, so the loose-form bound
    // catches a regression to per-poll without tightening the
    // assertion shape.
    for hash in ["d-featured-a", "d-featured-b"] {
        actor.test_inject_ready_with_features(
            hash,
            Some("test-pkg"),
            "x86_64-linux",
            &["zz-no-hosting-class"],
        );
    }
    // r34 bug_018: drive the forecast loop. The forecast block in
    // `compute_spawn_intents` is gated on `if max_lead > 0.0`;
    // test_hw_sla_config() has empty lead_time_seed so the block is
    // skipped. Seed one entry to enable the pass; value 100.0 < eta=300
    // so the pre-solve `eta >= intent_lead` gate fires lead_horizon.
    // q-stuck never reaches forecast.push so polls[0].len() stays at
    // the Ready count.
    actor.sla_config.lead_time_seed.insert(
        ("intel-6".into(), crate::sla::config::CapacityType::Od),
        100.0,
    );
    // Without a Queued drv with a Running dep, the `'q:` Queued loop
    // never iterates — the (1a) assertion is vacuous against any
    // forecast-loop emit. The debounce bounds it to ≤1 per (drv_hash,
    // reason); per-poll emission would read 8 (one per poll).
    actor.test_inject_at("dep-running", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("q-stuck", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("q-stuck", "dep-running");
    actor.test_set_running_eta("dep-running", 400.0, 100, 4); // eta=300

    // ── (1a) 8× poll, no state change → identical selectors ────────────
    // Side-effect-free except idempotent memo + debounced emits:
    // capture the full counter map before/after; ONLY the
    // `ONCE_PER_MISS` counters may have moved, and each by ≤ |Ready
    // drvs| (the per-key debounce bound — 10 drvs share 1 model_key,
    // so the actual bound is 1; ≤12 is the loose form that survives
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
    assert_eq!(polls[0].len(), 12, "all 12 Ready drvs intent-eligible");
    // r34 bug_018: per-poll forecast emit assertion. `q-stuck` is the
    // ONLY Queued+Running pair; the debounce bounds the counter to 1.
    // Per-poll emission reads 8 — the loose ≤12 bound above would
    // pass it (a §Stability-tests vacuity gap). Tighten exactly.
    assert_eq!(
        after
            .get("rio_scheduler_sla_forecast_dropped_total")
            .copied()
            .unwrap_or(0),
        1,
        "forecast_dropped_total MUST be debounced once-per-(drv,reason) — \
         per-poll emission reads 8× for one stuck Queued drv (r34 bug_018)"
    );
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    actor.cost_table.write().set_resolved_global((
        actor.sla_config.max_cores.unwrap() as u32,
        actor.sla_config.max_mem.unwrap(),
    ));
    actor.sla_ceilings = crate::sla::solve::Ceilings::from_resolved(
        &actor.sla_config,
        actor.cost_table.read().resolved_global(),
    );
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    // Three DISTINCT failures: each ack lands post-expiry
    // (merged_bug_005: in-window re-marks refresh at the same rung, so
    // the climb is forced through `force_expire` — a spurious
    // spawned-ack clear() would REMOVE the entry and reset the climb
    // to 0, which is exactly what this pins against).
    for i in 0..3 {
        if i > 0 {
            actor.ice.force_expire(&cell);
        }
        actor
            .handle_ack_spawned_intents(
                std::slice::from_ref(&intent),
                &["intel-6:spot".into()],
                &[],
                &[],
                &[],
                None,
                &[],
            )
            .expect("applied under leadership");
    }
    assert_eq!(
        actor.ice.step(&cell),
        Some(2),
        "spawned-ack must NOT clear; backoff doubles across consecutive \
         post-expiry failures"
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

    actor
        .handle_ack_spawned_intents(&[], &[], &["intel-6:spot".into()], &[], &[], None, &[])
        .expect("applied under leadership");
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let spot: crate::sla::config::Cell = ("mid-ebs-x86".into(), CapacityType::Spot);
    let od: crate::sla::config::Cell = ("mid-ebs-x86".into(), CapacityType::Od);
    assert!(actor.cost_table.read().menu(&spot).is_empty());

    // Controller's `Cell::to_string` form: `"h:spot"` / `"h:od"`
    // (sketch.rs:90 via `as_str()`).
    actor
        .handle_ack_spawned_intents(
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
            None,
            &[],
        )
        .expect("applied under leadership");

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
/// Also pins the wholesale-rebuild invariant (mb_012) on the LEGACY
/// field-5 arm (R9 read-side back-compat — pre-snapshot controllers):
/// a NON-empty `bound_intents` is the authoritative snapshot — entries
/// absent from it are dropped. An EMPTY `bound_intents` with NO
/// `binding_snapshot` is "no snapshot in this Ack" → no-op on the map.
/// (Presence semantics for snapshot-capable senders:
/// [`ack_binding_snapshot_presence_semantics`].)
#[tokio::test]
async fn ack_bound_intents_populates_authoritative_binding() {
    use rio_proto::types::BoundIntent;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    assert!(actor.authoritative_binding.is_empty());

    let bi = |id: &str, node: &str| BoundIntent {
        intent_id: id.into(),
        node_name: node.into(),
        deadline_secs: 0,
    };
    let abc = crate::state::DrvHash::from("abc123");
    let def = crate::state::DrvHash::from("def456");

    actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[
                bi("abc123", "ip-10-0-1-5.ec2.internal"),
                bi("def456", "ip-10-0-1-6.ec2.internal"),
            ],
            None,
            &[],
        )
        .expect("applied under leadership");

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
    // disappear from the controller's per-tick pod snapshot).
    actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[bi("abc123", "ip-10-0-1-5.ec2.internal")],
            None,
            &[],
        )
        .expect("applied under leadership");
    assert_eq!(actor.authoritative_binding.len(), 1);
    assert!(!actor.authoritative_binding.contains_key(&def));
    assert!(actor.authoritative_binding.contains_key(&abc));

    // Empty `bound_intents` = "this Ack carries no binding snapshot"
    // (per-pool reconciler at pool/jobs.rs sends `vec![]`; the
    // nodeclaim_pool reconciler owns the stream) → map unchanged.
    actor
        .handle_ack_spawned_intents(&[], &[], &[], &[], &[], None, &[])
        .expect("applied under leadership");
    assert_eq!(
        actor.authoritative_binding.len(),
        1,
        "empty bound_intents must be a no-op (per-pool ack), not a wipe"
    );
    assert!(actor.authoritative_binding.contains_key(&abc));
}

// r[verify sched.snapshot.binding-presence]
/// bug_285's presence semantics, exhaustively: `Some(non-empty)`
/// rebuilds; `Some(EMPTY)` CLEARS (the scale-to-zero tick — the
/// pre-fix behavior kept the stale map: the old pin literally asserted
/// "empty → no-op" for the only wire shape that existed); `None`
/// leaves the map untouched (per-pool Acks, pre-upgrade controllers).
#[tokio::test]
async fn ack_binding_snapshot_presence_semantics() {
    use rio_proto::types::BoundIntent;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let bi = |id: &str, node: &str| BoundIntent {
        intent_id: id.into(),
        node_name: node.into(),
        deadline_secs: 0,
    };
    let abc = crate::state::DrvHash::from("abc285");

    // Some(non-empty) rebuilds.
    actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            Some(&[bi("abc285", "node-1")]),
            &[],
        )
        .expect("applied under leadership");
    assert_eq!(actor.authoritative_binding.len(), 1);
    assert!(actor.authoritative_binding.contains_key(&abc));

    // None leaves the map untouched (per-pool Ack shape).
    actor
        .handle_ack_spawned_intents(&[], &[], &[], &[], &[], None, &[])
        .expect("applied under leadership");
    assert_eq!(
        actor.authoritative_binding.len(),
        1,
        "an Ack without a snapshot never clears captured bindings"
    );

    // Some(EMPTY) clears — scale-to-zero says so explicitly.
    actor
        .handle_ack_spawned_intents(&[], &[], &[], &[], &[], Some(&[]), &[])
        .expect("applied under leadership");
    assert!(
        actor.authoritative_binding.is_empty(),
        "present-and-empty is load-bearing: zero bound pods clears the map"
    );
}

/// merged_bug_046 red — REWRITES the retired cost-gate test. The OLD
/// test here (`ack_observed_instance_types_gated_on_cost_was_leader`)
/// certified "Err(CostGateClosed) ⇒ nothing landed": exactly the
/// whole-request refusal mechanics that created the evidence blackout
/// (one buffered observed type self-sustained a refusal of the
/// binding and ICE planes for up to 600s per failed reload) — the
/// availability claim the gate rode on was never witnessed. THIS test
/// certifies the apply-safety claim itself: an observation applied in
/// the PRE-RELOAD window (cost_was_leader=false) lands immediately —
/// the sibling mark on the SAME request lands too — and SURVIVES the
/// lease-acquire edge reload, because `carry_catalog` merges the
/// outgoing menus into the fresh PG load (union-only monotone store ⇒
/// lossless reload). Pre-fix red, verbatim:
///   left:  `Err(CostGateClosed)` ∧ menu empty ∧ step None
///   right: `Ok` ∧ menu len 1 ∧ step Some(0) ∧ entry survives reload
// r[verify sched.sla.cost-instance-type-feedback]
#[tokio::test]
async fn ack_observed_lands_pre_reload_and_survives_the_edge_reload() {
    use rio_proto::types::ObservedInstanceType;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let spot: crate::sla::config::Cell =
        ("mid-ebs-x86".into(), crate::sla::config::CapacityType::Spot);

    let observed = [ObservedInstanceType {
        cell: "mid-ebs-x86:spot".into(),
        instance_type: "c7i.8xlarge".into(),
        cores: 32,
        mem_bytes: 64 << 30,
    }];

    // Pre-reload window: the edge-reload latch is false (the
    // housekeeping prelude has not run for this tenure yet). One
    // observed type and one mark ride the SAME request — pre-fix the
    // observed plane closed the gate and the WHOLE request refused.
    actor
        .cost_was_leader
        .store(false, std::sync::atomic::Ordering::Relaxed);
    actor
        .handle_ack_spawned_intents(
            &[],
            &["mid-ebs-x86:spot".into()],
            &[],
            &observed,
            &[],
            None,
            &[],
        )
        .expect("pre-reload Ack applies every plane (the gate is gone)");
    assert_eq!(
        actor.cost_table.read().menu(&spot).len(),
        1,
        "observation lands immediately on the pre-reload table"
    );
    assert_eq!(
        actor.ice.step(&spot),
        Some(0),
        "the sibling mark is no longer refused for the observed plane's \
         apply-window"
    );

    // The lease-acquire edge reload against the SAME TestDb: the
    // observation was never persisted (persist is a housekeeping-tick
    // effect), so the fresh load does not contain it — the merge half
    // of `carry_catalog` must carry it forward.
    let sdb = crate::db::SchedulerDb::new(db.pool.clone());
    let (cluster, source) = {
        let g = actor.cost_table.read();
        (g.cluster().to_owned(), g.source())
    };
    let fresh = crate::sla::cost::CostTable::load(&sdb, &cluster, source)
        .await
        .expect("fresh load");
    actor.cost_table.write().carry_catalog(fresh);
    let ct = actor.cost_table.read();
    assert_eq!(
        ct.menu(&spot).len(),
        1,
        "a pre-reload observation must SURVIVE the edge reload (merge law)"
    );
    assert_eq!(ct.menu(&spot)[0].name, "c7i.8xlarge");
}

/// bug_094 red: an undecodable entry in ANY plane refuses the WHOLE
/// request before any mutation. Pre-fix all three string planes
/// folded unparseable entries into success (`if let Some(cell) =
/// parse_cell(s)` with no else arm; `filter_map(... parse_cell ?)`)
/// and the fn returned `Ok(())` against its own "Ok only when EVERY
/// plane landed" contract — on Ack-Ok the controller destroys its
/// consume-once buffer ("the ONLY clear"), so the dropped entry was
/// unrecoverable. `left: Ok(()) ∧ clear applied ∧ mark silently gone`
/// / `right: Err(PlaneEntryUndecodable{UnfulfillableCells, ..}) ∧ ice
/// state byte-identical (zero mutations)`.
// r[verify sched.sla.ack-validate-then-commit+1]
#[tokio::test]
async fn ack_undecodable_plane_entry_refuses_whole_request() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    // A masked cell whose valid clear rides the same request as the
    // undecodable mark entry.
    let cell: crate::sla::config::Cell = ("intel-6".into(), CapacityType::Spot);
    actor.ice.mark(&cell);
    assert_eq!(actor.ice.step(&cell), Some(0), "precondition: masked");

    // "mid-ebs-x86:bogus" is deliberately a shape NO production
    // constructor can emit — the controller's `Cell::Display` and the
    // shared `encode_cell_event` only emit alphabet capacities. The
    // test asserts the typed REFUSAL of the attack/skew shape, not a
    // valid-shape behavior (R13 lane (i); tag line-local per the
    // posted WO-S8-11 grammar).
    // r13-allow(refusal-probe): asserts the typed refusal; shape deliberately unproducible
    let bogus = "mid-ebs-x86:bogus".to_string();
    let r = actor.handle_ack_spawned_intents(
        &[],
        std::slice::from_ref(&bogus),
        &["intel-6:spot".into()],
        &[],
        &[],
        None,
        &[],
    );
    assert_eq!(
        r,
        Err(crate::actor::AckApplyError::PlaneEntryUndecodable {
            plane: crate::actor::AckPlane::UnfulfillableCells,
            entry: bogus,
        }),
        "undecodable plane entry must be a typed refusal naming plane + entry"
    );
    assert_eq!(
        actor.ice.step(&cell),
        Some(0),
        "zero mutations: the valid clear in the refused request must \
         NOT have applied (err implies no plane landed)"
    );
    assert!(actor.ice.is_masked(&cell), "ice state byte-identical");
}

/// merged_bug_134 red: a length-skewed spawn-intent echo is REFUSED,
/// never zip-truncated. The echo is the CONTROLLER'S (rolling skew /
/// one-array filter in scope), and `Iterator::zip` silently truncates
/// to the shorter array — a 2-cell arm truncated to 1 forges the
/// exactly-one-cell proof the §13a first-pull ICE clear gates on
/// (`let [cell] = cells.as_slice()`, actor/pull.rs), clearing the
/// ladder for a cell the pod may never have scheduled on. `left: Ok ∧
/// dispatched_cells armed with the forged single cell [c0] (the §13a
/// clear gate would now pass)` / `right: Err(ArmEchoSkewed{names:2,
/// terms:1}) ∧ dispatched_cells untouched`.
///
/// Witness provenance (Q1-1): the skewed fixture is a PRODUCTION
/// intent built through `cells_to_selector_terms` (the
/// paired-by-construction producer) with one `node_affinity` term
/// dropped — the controller-side one-array-filter shape; no
/// hand-rolled parallel arrays.
// r[verify sched.sla.ack-validate-then-commit+1]
#[tokio::test]
async fn ack_skewed_arm_echo_refused_not_truncated() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::SpawnIntent;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let cfg = test_hw_sla_config();
    let cells: Vec<crate::sla::config::Cell> = vec![
        ("intel-6".into(), CapacityType::Spot),
        ("intel-7".into(), CapacityType::Spot),
    ];
    let (terms, names) = crate::sla::solve::cells_to_selector_terms(&cells, &cfg.hw_classes);
    assert_eq!((names.len(), terms.len()), (2, 2), "paired by construction");
    let mut intent = SpawnIntent {
        intent_id: "d-skew".into(),
        hw_class_names: names,
        node_affinity: terms,
        ..Default::default()
    };
    intent.node_affinity.pop();

    let r = actor.handle_ack_spawned_intents(
        std::slice::from_ref(&intent),
        &[],
        &[],
        &[],
        &[],
        None,
        &[],
    );
    assert_eq!(
        r,
        Err(crate::actor::AckApplyError::ArmEchoSkewed {
            intent_id: "d-skew".into(),
            names: 2,
            terms: 1,
        }),
        "skewed echo must refuse, not truncate"
    );
    assert!(
        actor.dispatched_cells.get("d-skew").is_none(),
        "dispatched_cells untouched — no forged single-cell arm"
    );
}

/// merged_bug_039 red — the LABEL-COPY duplicate axis: an aligned term
/// carrying TWO capacity-type requirements (the label copy FIRST, the
/// authoritative one LAST — exactly what a colliding `hw_classes.labels`
/// entry makes `cells_to_selector_terms` emit: labels first, capacity
/// appended last) must REFUSE, not decode order-sensitively. Pre-fix,
/// verbatim: `left: Ok — armed (intel-6, Od) (the wrong cell — the
/// find() peek took the first match) / right:
/// Err(PlaneEntryUndecodable{SpawnedArming, ..})`.
///
/// Witness strength: the existing skew tests certify the length-skew
/// and missing-requirement axes ONLY; this red certifies the duplicate
/// axis the alphabet never enumerated. r13-allow(refusal-probe): the
/// duplicated term is the attack/skew shape under test — built by
/// extending a production `cells_to_selector_terms` echo with the
/// label copy the colliding config would inject.
// r[verify sched.sla.ack-validate-then-commit+1]
#[tokio::test]
async fn arm_decode_refuses_the_label_copy_of_the_capacity_key() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::{NodeSelectorRequirement, SpawnIntent};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let cfg = test_hw_sla_config();
    let cells: Vec<crate::sla::config::Cell> = vec![("intel-6".into(), CapacityType::Spot)];
    let (mut terms, names) = crate::sla::solve::cells_to_selector_terms(&cells, &cfg.hw_classes);
    // The colliding-config shape: the label copy lands FIRST (labels
    // are emitted before the authoritative capacity requirement).
    terms[0].match_expressions.insert(
        0,
        NodeSelectorRequirement {
            key: crate::sla::config::LABEL_CAPACITY_TYPE.into(),
            operator: "In".into(),
            values: vec!["on-demand".into()],
        },
    );
    let intent = SpawnIntent {
        intent_id: "d-labelcopy".into(),
        hw_class_names: names,
        node_affinity: terms,
        ..Default::default()
    };

    let r = actor.handle_ack_spawned_intents(
        std::slice::from_ref(&intent),
        &[],
        &[],
        &[],
        &[],
        None,
        &[],
    );
    assert!(
        matches!(
            &r,
            Err(crate::actor::AckApplyError::PlaneEntryUndecodable {
                plane: crate::actor::AckPlane::SpawnedArming,
                ..
            })
        ),
        "two capacity requirements in one term must refuse typed, not \
         decode the first match order-sensitively; got {r:?}"
    );
    assert!(
        actor.dispatched_cells.get("d-labelcopy").is_none(),
        "no arm from the refused echo (pre-fix: armed the LABEL copy's cell)"
    );
}

/// merged_bug_039 table red — the OPERATOR axis: `NotIn[spot]` names
/// the COMPLEMENT of a cell; the pre-fix peek decoded it to its
/// inverse. Pre-fix: `Ok — armed (intel-6, Spot)`; post-fix: typed
/// refusal. r13-allow(refusal-probe): non-producer operator shape.
// r[verify sched.sla.ack-validate-then-commit+1]
#[tokio::test]
async fn arm_decode_refuses_notin_operator() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::SpawnIntent;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let cfg = test_hw_sla_config();
    let cells: Vec<crate::sla::config::Cell> = vec![("intel-6".into(), CapacityType::Spot)];
    let (mut terms, names) = crate::sla::solve::cells_to_selector_terms(&cells, &cfg.hw_classes);
    let cap_req = terms[0]
        .match_expressions
        .iter_mut()
        .find(|r| r.key == crate::sla::config::LABEL_CAPACITY_TYPE)
        .expect("producer emits the capacity requirement");
    cap_req.operator = "NotIn".into();
    let intent = SpawnIntent {
        intent_id: "d-notin".into(),
        hw_class_names: names,
        node_affinity: terms,
        ..Default::default()
    };

    let r = actor.handle_ack_spawned_intents(
        std::slice::from_ref(&intent),
        &[],
        &[],
        &[],
        &[],
        None,
        &[],
    );
    assert!(
        matches!(
            &r,
            Err(crate::actor::AckApplyError::PlaneEntryUndecodable {
                plane: crate::actor::AckPlane::SpawnedArming,
                ..
            })
        ),
        "NotIn must refuse (pre-fix: decoded to the set's own cell — its \
         inverse); got {r:?}"
    );
    assert!(actor.dispatched_cells.get("d-notin").is_none());
}

/// merged_bug_039 table red — the ARITY axis: `In[spot,on-demand]`
/// names TWO cells; the pre-fix peek decoded `values.first()` only.
/// Pre-fix: `Ok — armed (intel-6, Spot)` (silent truncation of the
/// cell set); post-fix: typed refusal. r13-allow(refusal-probe):
/// non-producer arity shape.
// r[verify sched.sla.ack-validate-then-commit+1]
#[tokio::test]
async fn arm_decode_refuses_multivalue_capacity() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::SpawnIntent;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let cfg = test_hw_sla_config();
    let cells: Vec<crate::sla::config::Cell> = vec![("intel-6".into(), CapacityType::Spot)];
    let (mut terms, names) = crate::sla::solve::cells_to_selector_terms(&cells, &cfg.hw_classes);
    terms[0]
        .match_expressions
        .iter_mut()
        .find(|r| r.key == crate::sla::config::LABEL_CAPACITY_TYPE)
        .expect("producer emits the capacity requirement")
        .values
        .push("on-demand".into());
    let intent = SpawnIntent {
        intent_id: "d-multival".into(),
        hw_class_names: names,
        node_affinity: terms,
        ..Default::default()
    };

    let r = actor.handle_ack_spawned_intents(
        std::slice::from_ref(&intent),
        &[],
        &[],
        &[],
        &[],
        None,
        &[],
    );
    assert!(
        matches!(
            &r,
            Err(crate::actor::AckApplyError::PlaneEntryUndecodable {
                plane: crate::actor::AckPlane::SpawnedArming,
                ..
            })
        ),
        "a multi-valued capacity requirement names {{spot, od}} — not one \
         cell; the peek silently truncated to values[0]; got {r:?}"
    );
    assert!(actor.dispatched_cells.get("d-multival").is_none());
}

/// merged_bug_039 round-trip green: the decode accepts the producer's
/// ACTUAL emission — `ArmDecode::decode(echo of
/// cells_to_selector_terms(cells)) == Armed(cells)` for 1- and 2-cell
/// sets, driven through the full apply path (R13: the producer fn IS
/// the production constructor for echo shapes; the out-of-plane
/// producer is imported READ-ONLY).
// r[verify sched.sla.ack-validate-then-commit+1]
#[tokio::test]
async fn producer_echo_roundtrips_through_arm_decode() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::SpawnIntent;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let cfg = test_hw_sla_config();

    for (tag, cells) in [
        (
            "rt-one",
            vec![("intel-6".into(), CapacityType::Spot)] as Vec<crate::sla::config::Cell>,
        ),
        (
            "rt-two",
            vec![
                ("intel-6".into(), CapacityType::Spot),
                ("intel-7".into(), CapacityType::Od),
            ],
        ),
    ] {
        let (terms, names) = crate::sla::solve::cells_to_selector_terms(&cells, &cfg.hw_classes);
        let intent = SpawnIntent {
            intent_id: tag.into(),
            hw_class_names: names,
            node_affinity: terms,
            ..Default::default()
        };
        actor
            .handle_ack_spawned_intents(
                std::slice::from_ref(&intent),
                &[],
                &[],
                &[],
                &[],
                None,
                &[],
            )
            .expect("the producer's own echo decodes");
        let armed = actor
            .dispatched_cells
            .get(tag)
            .unwrap_or_else(|| panic!("{tag}: armed"));
        assert_eq!(
            armed.as_slice(),
            cells.as_slice(),
            "{tag}: decode(echo(cells)) == cells"
        );
    }
}

/// merged_bug_134 companion green: the legacy one-array-empty echo
/// shape (`hw_class_names` empty, `node_affinity` non-empty) answers
/// Ok with NO arm — `ArmDecode::LegacyUnarmed` is a typed no-arm
/// totality lane, not a refusal (it cannot forge a cell set; pre-fix
/// it already zip-truncated to no-arm; its rolling-skew rationale is
/// MOOT per SIGNED Q6 --wipe rollout and the lane survives as decode
/// totality). Pins the refusal to the FORGING skew shapes only.
// r[verify sched.sla.ack-validate-then-commit+1]
#[tokio::test]
async fn ack_legacy_unarmed_echo_answers_ok_without_arm() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::SpawnIntent;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    let cfg = test_hw_sla_config();
    let cells: Vec<crate::sla::config::Cell> = vec![("intel-6".into(), CapacityType::Spot)];
    let (terms, _names) = crate::sla::solve::cells_to_selector_terms(&cells, &cfg.hw_classes);
    let intent = SpawnIntent {
        intent_id: "d-legacy".into(),
        hw_class_names: vec![],
        node_affinity: terms,
        ..Default::default()
    };

    actor
        .handle_ack_spawned_intents(std::slice::from_ref(&intent), &[], &[], &[], &[], None, &[])
        .expect("legacy shape is a typed no-arm lane, not a refusal");
    assert!(
        actor.dispatched_cells.get("d-legacy").is_none(),
        "no arm from the legacy lane"
    );
    // The spawn-ack witness still records (124(d) is arm-independent).
    assert!(
        actor
            .acked_spawned
            .contains_key(&crate::state::DrvHash::from("d-legacy")),
        "spawn-ack witness recorded for the legacy shape"
    );
}

/// merged_bug_008 plan-level red: epoch'd evidence through the FULL
/// apply path (wire string → plan decode → epoch gate → ladder). A
/// post-expiry redelivery of the SAME buffered event must not climb —
/// `left: step 0→1 (every redelivery after expiry was a phantom
/// consecutive failure)` / `right: step 0; only a strictly newer
/// epoch climbs`.
///
/// Witness provenance (Q1-1): suffixed strings are minted exclusively
/// via `rio_common::cell_wire::encode_cell_event` — the same fn the
/// controller's buffer mint calls; no hand-rolled "h:cap@e" literals.
// r[verify sched.sla.ack-validate-then-commit+1]
// r[verify ctrl.nodeclaim.evidence-ack-latch+3]
#[tokio::test]
async fn ack_redelivered_epoch_mark_does_not_climb_post_expiry() {
    use crate::sla::config::CapacityType;
    use rio_common::cell_wire::{EvidenceEpoch, WireCapacity, encode_cell_event};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let cell: crate::sla::config::Cell = ("intel-6".into(), CapacityType::Spot);

    let mark_e7 = encode_cell_event("intel-6", WireCapacity::Spot, Some(EvidenceEpoch(7)));
    actor
        .handle_ack_spawned_intents(
            &[],
            std::slice::from_ref(&mark_e7),
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("first delivery applies");
    assert_eq!(actor.ice.step(&cell), Some(0), "first failure at rung 0");

    // Mask expires; the controller's ~10s ack-retry loop redelivers
    // the SAME buffered event (client timeout after server apply).
    actor.ice.force_expire(&cell);
    actor
        .handle_ack_spawned_intents(
            &[],
            std::slice::from_ref(&mark_e7),
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("redelivery answers Ok — delivered evidence must clear the buffer");
    assert_eq!(
        actor.ice.step(&cell),
        Some(0),
        "post-expiry same-epoch redelivery is the SAME observation — no climb"
    );

    // A genuinely new failure (strictly newer epoch) climbs.
    let mark_e8 = encode_cell_event("intel-6", WireCapacity::Spot, Some(EvidenceEpoch(8)));
    actor
        .handle_ack_spawned_intents(
            &[],
            std::slice::from_ref(&mark_e8),
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("newer epoch applies");
    assert_eq!(
        actor.ice.step(&cell),
        Some(1),
        "strictly newer post-expiry failure climbs the ladder"
    );
}

/// merged_bug_003 scheduler-side companion (green pin — the
/// controller half is the red, lifecycle_tests.rs
/// `clear_then_mark_ships_both_planes_with_ordered_epochs`): a
/// request carrying one cell in BOTH planes — the `ClearThenMark`
/// wire shape, clear epoch < mark epoch — realizes
/// reset-then-step-0: the fixed clears-then-marks apply order resets
/// the ladder, then the strictly-newer mark masks at the BASE TTL
/// instead of climbing from the stale rung.
// r[verify sched.sla.ack-validate-then-commit+1]
// r[verify ctrl.nodeclaim.evidence-ack-latch+3]
#[tokio::test]
async fn ack_clear_then_mark_realizes_reset_then_step0() {
    use crate::sla::config::CapacityType;
    use rio_common::cell_wire::{EvidenceEpoch, WireCapacity, encode_cell_event};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let cell: crate::sla::config::Cell = ("intel-6".into(), CapacityType::Spot);
    let mark = |e: u64| encode_cell_event("intel-6", WireCapacity::Spot, Some(EvidenceEpoch(e)));
    let clear = |e: u64| encode_cell_event("intel-6", WireCapacity::Spot, Some(EvidenceEpoch(e)));

    // Climb to rung 1 via two genuine post-expiry failures.
    actor
        .handle_ack_spawned_intents(
            &[],
            std::slice::from_ref(&mark(1)),
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applies");
    actor.ice.force_expire(&cell);
    actor
        .handle_ack_spawned_intents(
            &[],
            std::slice::from_ref(&mark(2)),
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applies");
    assert_eq!(actor.ice.step(&cell), Some(1), "precondition: rung 1");
    actor.ice.force_expire(&cell);

    // The ClearThenMark shape: BOTH planes, clear epoch < mark epoch.
    actor
        .handle_ack_spawned_intents(
            &[],
            std::slice::from_ref(&mark(11)),
            std::slice::from_ref(&clear(10)),
            &[],
            &[],
            None,
            &[],
        )
        .expect("both planes apply");
    assert_eq!(
        actor.ice.step(&cell),
        Some(0),
        "reset-then-step-0 — never a climb from the stale rung"
    );
    assert!(actor.ice.is_masked(&cell), "masked at the base TTL");
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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

    actor
        .handle_ack_spawned_intents(std::slice::from_ref(&intent), &[], &[], &[], &[], None, &[])
        .expect("applied under leadership");

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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
        actor.ice.exhausted(
            ["intel-6", "intel-7", "intel-8"].map(String::from).iter(),
            |h| actor.sla_config.capacity_types_for(h).to_vec(),
        ),
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    actor.cost_table.write().set_resolved_global((
        actor.sla_config.max_cores.unwrap() as u32,
        actor.sla_config.max_mem.unwrap(),
    ));
    actor.sla_ceilings = crate::sla::solve::Ceilings::from_resolved(
        &actor.sla_config,
        actor.cost_table.read().resolved_global(),
    );

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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
        actor.cost_table.write().set_resolved_global((
            actor.sla_config.max_cores.unwrap() as u32,
            actor.sla_config.max_mem.unwrap(),
        ));
        actor.sla_ceilings = crate::sla::solve::Ceilings::from_resolved(
            &actor.sla_config,
            actor.cost_table.read().resolved_global(),
        );
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // Builders-only fixture: the `pool` precondition counts ALL
    // `cfg.hw_classes`, but a featureless drv never pins to fetcher
    // cells (∅-guard) — fetcher-* would inflate `want_pool` past what
    // the rotation can actually draw.
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
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
            max_cores: Some(actor.sla_config.max_cores.unwrap() as u32),
            max_mem: Some(actor.sla_config.max_mem.unwrap()),
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
/// semantic case observability.typ documents. Red on 6eab30da:
/// `infeasible_counts["interrupt_runaway"] == 0`, `core_ceiling == 1`.
#[tokio::test]
async fn contract_interrupt_runaway_reachable() {
    use crate::sla::cost::RatioEma;
    use crate::sla::metrics::infeasible_counts;
    use crate::sla::solve::InfeasibleReason;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;

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
        // r33 bug_007: key the seed on `test-hw` (the configured class)
        // so the per-intent `max_lead_for` admits the forecast intents.
        // A key ∉ `hw_classes` is a config-validation error
        // (`validate_both`) — pre-r33 the global `max(values())`
        // didn't care, but `class_routes` correctly returns `false`.
        sla.lead_time_seed
            .insert(("test-hw".into(), CapacityType::Spot), 200.0);
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // Builders-only fixture: the precondition asserts
    // `cfg.hw_classes.len()==3` and the metric count derives from
    // `|h_all|`; a featureless drv never routes to fetcher cells
    // (∅-guard), so fetcher-* would inflate the count without any
    // ClassCeiling cell appearing for them.
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
        let (cc, _) = actor.sla_config.class_ceilings(
            name,
            &Default::default(),
            actor.cost_table.read().resolved_global(),
        );
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
            max_cores: Some(actor.sla_config.max_cores.unwrap() as u32),
            max_mem: Some(actor.sla_config.max_mem.unwrap()),
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

/// §13d STRIKE-7 (r30 mb_012): cold-start kvm intent (`fit=None`, no
/// `--capacity` override, `required_features=["kvm"]`) must emit
/// `hw_class_names != []` so the controller mints a metal NodeClaim.
/// Pre-fix the bypass `None/None` arm returned `(Vec::new(), Vec::new())`
/// unconditionally → controller's `fallback_cell` picked a non-metal
/// reference cell → kvm pod CrashLoopBackOff on ENXIO `/dev/kvm`
/// (no metal node minted; pool-static nodeSelector deleted r33
/// bug_002) → no `build_sample` → `fit` stays `None` →
/// hard bootstrap deadlock now that §13c deleted the static metal
/// NodePool escape hatch.
// r[verify sched.sla.hwclass.provides]
#[tokio::test]
async fn bypass_none_arm_featured_intent_emits_cells() {
    use crate::sla::config::{ARCH_LABEL, HwClassDef, NodeLabelMatch};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.hw_classes.insert(
        "metal-x86".into(),
        HwClassDef {
            labels: vec![NodeLabelMatch {
                key: ARCH_LABEL.into(),
                value: "amd64".into(),
            }],
            node_class: "rio-metal".into(),
            max_cores: Some(actor.sla_config.max_cores.unwrap() as u32),
            max_mem: Some(actor.sla_config.max_mem.unwrap()),
            provides_features: vec!["kvm".into()],
            ..Default::default()
        },
    );
    let m: std::collections::HashMap<_, _> = ["intel-6", "intel-7", "intel-8", "metal-x86"]
        .into_iter()
        .map(|h| (h.into(), 1.0))
        .collect();
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));

    // Cold-start: pname "cold-kvm" has no seeded fit → `fit=None` →
    // bypass `None/None` arm.
    actor.test_inject_ready_with_features("d-cold-kvm", Some("cold-kvm"), "x86_64-linux", &["kvm"]);
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d-cold-kvm")
        .expect("cold-start kvm intent emitted");
    assert!(
        !intent.hw_class_names.is_empty(),
        "cold-start kvm intent must emit hw_class_names so the controller \
         mints a metal NodeClaim — got [] (bootstrap deadlock)"
    );
    assert!(
        intent.hw_class_names.iter().all(|h| h == "metal-x86"),
        "cold-start kvm intent must route to metal-x86 ONLY; got {:?}",
        intent.hw_class_names
    );
}

/// §13e (was §13d/r30 A9/mb_012): a fixed-output drv carrying
/// declared `required_features` (a degenerate but possible builder
/// declaration) routes to FETCHER cells, not the declared feature's
/// class. `effective_features` overrides the declaration: a FOD
/// declaring `[kvm]` would otherwise route to a kvm node with no
/// fetcher airgap (`r[builder.netpol.airgap]`). Pre-§13e the
/// `bypass_cells` FOD hoist returned `[]` and the static `rio-fetcher`
/// NodePool's pod nodeSelector caught it; that hoist is GONE.
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn bypass_none_arm_fod_with_features_routes_to_fetcher() {
    use crate::sla::config::{ARCH_LABEL, HwClassDef, NodeLabelMatch};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.hw_classes.insert(
        "metal-x86".into(),
        HwClassDef {
            labels: vec![NodeLabelMatch {
                key: ARCH_LABEL.into(),
                value: "amd64".into(),
            }],
            node_class: "rio-metal".into(),
            max_cores: Some(actor.sla_config.max_cores.unwrap() as u32),
            max_mem: Some(actor.sla_config.max_mem.unwrap()),
            provides_features: vec!["kvm".into()],
            ..Default::default()
        },
    );
    // FOD with `required_features=["kvm"]` — degenerate but a tenant
    // CAN declare it. Inject directly so `is_fixed_output=true` AND
    // `required_features` are both set.
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        pname: Some("cold-fod".into()),
        is_fixed_output: true,
        required_features: vec!["kvm".into()],
        ..crate::db::RecoveryDerivationRow::test_default("d-cold-fod", "x86_64-linux")
    });
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d-cold-fod")
        .expect("FOD intent emitted");
    assert!(
        intent
            .hw_class_names
            .iter()
            .all(|h| h.starts_with("fetcher-")),
        "FOD with declared [kvm] must route ONLY to fetcher cells \
         (effective_features overrides the misconfig); got {:?}",
        intent.hw_class_names
    );
    assert!(
        !intent.hw_class_names.is_empty(),
        "FOD must route to fetcher cells, not stay hw-agnostic; got []",
    );
    // Wire form carries the EFFECTIVE features so the controller's
    // spawn-decision query agrees on which Pool serves the intent.
    assert_eq!(
        intent.required_features,
        vec!["fetcher".to_string()],
        "wire SpawnIntent.required_features must be the derived set",
    );
}

/// **mb_031**: `rio_scheduler_unroutable_features_total` is debounced
/// once per `(tenant, required_features)` and carries NO `feature`
/// label. Both invariants closed in one test:
///
/// 1. **Bounded cardinality** — the `feature` label was tenant-
///    controlled (verbatim `requiredSystemFeatures`, unclamped); a
///    tenant submitting `["x-${uuid}"]` per drv mints unbounded
///    Prometheus series on shared monitoring (REVIEW.md §Threat-model
///    "unbounded-cardinality partition key"). Drop the label; keep
///    `tenant` (bounded by `Claims.sub`).
/// 2. **Once-per-edge debounce** — the doc-comment claims "Surface it
///    once per (tenant, feature)" but the emit was per-drv per-poll
///    (sat above `was_miss`, gated only on `h_all.is_empty()`). The
///    debounce mirrors `dispatch.rs`'s `unroutable_warned` set.
///
/// Pre-fix this fails on BOTH assertions: `feature` label present, and
/// counter = 2 after two polls.
// r[verify sched.sla.hwclass.provides]
#[tokio::test]
async fn unroutable_features_debounced_no_feature_label() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    seed_fit(&actor, "test-pkg");
    // No hwClass in `bare_actor_hw` provides "zz-unroutable" → `h_all`
    // is empty for this drv → unroutable emit fires.
    actor.test_inject_ready_with_features(
        "d-unroutable",
        Some("test-pkg"),
        "x86_64-linux",
        &["zz-unroutable"],
    );

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    {
        let _g = metrics::set_default_local_recorder(&rec);
        // Two polls, no state change. The unroutable feature-tuple is
        // identical both times → exactly one increment.
        actor.compute_spawn_intents(&Default::default());
        actor.compute_spawn_intents(&Default::default());
    }

    // No `feature` label: `counter_map_by(.., Some("feature"))` groups
    // by the label's value (or `""` when absent). Pre-fix the key is
    // `"zz-unroutable"`; post-fix it's `""` (label dropped).
    let by_feature = crate::sla::metrics::counter_map_by(
        &snap,
        "rio_scheduler_unroutable_features_total",
        Some("feature"),
    );
    assert!(
        !by_feature.contains_key("zz-unroutable"),
        "unroutable_features_total must NOT carry a `feature` label \
         (tenant-controlled unbounded cardinality on shared monitoring); \
         got keys {:?}",
        by_feature.keys().collect::<Vec<_>>()
    );
    let total: u64 = by_feature.values().sum();
    assert_eq!(
        total, 1,
        "unroutable_features_total must be debounced once per \
         (tenant, required_features) edge, not per-drv per-poll; \
         two polls → 1 increment, got {total}"
    );
}

/// §13e (was mb_023/r31 B0) — bypass-path `Some(cap)` arm: FOD with a
/// `--capacity` override routes to FETCHER cells (`effective_features
/// = [fetcher]` ⟹ `reference_hw_class_for_system` finds `fetcher-*`).
/// Pre-§13e the `bypass_cells` FOD hoist returned `[]` regardless of
/// arm because the static `rio-fetcher` NodePool's pod nodeSelector
/// caught FODs without per-intent affinity; both the hoist and that
/// NodePool are DELETED in §13e.
///
/// Also exercises the §13e debug_assert tripwire (was r31-A4): the
/// invariant inverted from `is_fixed_output ⟹ cells = []` to
/// `is_fixed_output ⟺ effective_features ∋ fetcher`.
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn contract_fod_capacity_override_routes_to_fetcher() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Fetcher drv (`is_fod=true`) whose pname matches a `--capacity`
    // override. The runbook documents `rio-cli sla override <pname>
    // --capacity=on-demand` for spot-interruption skew — applying it
    // to (or via wildcard over) an FOD pname is a documented operator
    // action, not an edge case.
    actor.test_inject_ready("d-fod-cap", Some("test-pkg"), "x86_64-linux", true);
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "test-pkg".into(),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        }]);

    let state = actor.dag.node("d-fod-cap").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);

    assert!(
        intent
            .hw_class_names
            .iter()
            .all(|h| h.starts_with("fetcher-")),
        "FOD with `--capacity` override must route ONLY to fetcher \
         cells (§13e); got hw_class_names={:?} node_affinity={:?}",
        intent.hw_class_names,
        intent.node_affinity,
    );
    assert!(
        !intent.hw_class_names.is_empty(),
        "FOD with `--capacity` override must route to fetcher cells, \
         not stay hw-agnostic; got []",
    );
}

/// r40 bug_025: `solve_intent_for`'s `None` arm must clamp `cores` to
/// `sla_ceilings.max_cores` BEFORE `bypass_cells` so the producer-side
/// `reference_hw_class_for_system` size filter and the post-finalize
/// `retain_hosting_cells` chokepoint agree on `(cores, mem)`. With
/// pre-clamp `cores > ceiling`, the size filter rejects every class →
/// `bypass_cells` returns `[]` → `retain_hosting_cells`
/// (filters-then-expands over RETAINED classes — merged_bug_004; it
/// never RECOVERS a producer-rejected cell, and an empty input has no
/// retained class to expand) keeps `[]` → `node_affinity = []`. For a
/// featured intent the pod silently lands without the feature affinity
/// and crashloops.
///
/// A `bypass_cells`-level test does NOT pin this: `bypass_cells(192) →
/// non-empty` and `bypass_cells(256) → empty` are both true pre-fix —
/// the producer's contract is unchanged. The fix is in the *call site*
/// (the pre-clamp before `bypass_cells`), so only a `solve_intent_for`
/// test goes red on revert.
///
/// Modeled on `contract_fod_capacity_override_routes_to_fetcher` —
/// same FOD/featured shape, same `solve_intent_for` entry, but the
/// override forces `cores` over `sla_ceilings.max_cores` (256 vs the
/// `test_hw_sla_config` ceiling of 64) instead of pinning `--capacity`.
///
/// RED at 938f2a957: `intent.hw_class_names == []`.
/// GREEN after the pre-clamp: `intent.hw_class_names == ["fetcher-x86"]`.
#[tokio::test]
async fn contract_overcap_cores_override_still_routes_featured_intent() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Featured intent: FOD ⟹ `effective_features = [fetcher]` (§13e),
    // routes to `fetcher-*` only — same construction as the
    // `--capacity` test above.
    actor.test_inject_ready("d-overcap", Some("test-pkg"), "x86_64-linux", true);
    let ceiling = actor.sla_ceilings.max_cores as u32;
    let overcap = (ceiling as f64) * 4.0;
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "test-pkg".into(),
            cores: Some(overcap),
            ..Default::default()
        }]);

    let state = actor.dag.node("d-overcap").unwrap();
    let (hw, cost, ig) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, ig);

    assert!(
        !intent.hw_class_names.is_empty(),
        "featured intent with over-cap `--cores={overcap}` (ceiling \
         {ceiling}) must keep node_affinity — `bypass_cells` must see \
         the post-clamp cores so `reference_hw_class_for_system` finds \
         a hosting class; got hw_class_names={:?} node_affinity={:?}",
        intent.hw_class_names,
        intent.node_affinity,
    );
    assert!(
        intent
            .hw_class_names
            .iter()
            .all(|h| h.starts_with("fetcher-")),
        "FOD with over-cap `--cores` override must route ONLY to \
         fetcher cells (§13e); got hw_class_names={:?}",
        intent.hw_class_names,
    );
    assert!(
        intent.cores <= ceiling,
        "over-cap `--cores={overcap}` must be clamped to ceiling \
         {ceiling}; got intent.cores={}",
        intent.cores,
    );
}

/// §13e (was mb_023/r31 B0) — `bypass_cells` derives `effective_
/// features` ABOVE the `match cap`, so BOTH arms (and any future arm)
/// see `[fetcher]` for FODs. Asserts on `bypass_cells` directly so a
/// future arm bypassing the chokepoint is caught at the producer, not
/// just by the post-finalize chokepoint or the §13e tripwire.
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn bypass_cells_fod_routes_to_fetcher_regardless_of_cap() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.test_inject_ready("d-fod", Some("test-pkg"), "x86_64-linux", true);

    let state = actor.dag.node("d-fod").unwrap();
    let (_, cost, _) = actor.solve_inputs();
    for cap in [None, Some(CapacityType::Od), Some(CapacityType::Spot)] {
        let cells = actor.bypass_cells(state, cap, 4, 4 << 30, &cost, "tenant-a");
        assert!(
            !cells.is_empty(),
            "FOD must produce fetcher bypass cells (§13e) for cap={cap:?}; got []",
        );
        assert!(
            cells.iter().all(|(h, _)| h.starts_with("fetcher-")),
            "FOD bypass cells must be fetcher-* ONLY (§13e) for cap={cap:?}; got {cells:?}",
        );
    }
}

/// **mb_003 (r31 B0)** — bypass-path `Some(cap)` arm gates `cap ∈
/// capacity_types_for(h)`, mirroring the `None` arm. Pre-fix: an
/// override pinning a cap the reference class doesn't host (e.g.
/// `--capacity=spot` on an od-only reference class) emits
/// `[(h, Spot)]` which `retain_hosting_cells` strips (`cap_ok=false`)
/// → chokepoint `warn!("producer-path … regressed?")` per drv per
/// poll for the override TTL — defeating the documented "strip = a
/// producer regression signal, not log spam" contract at config.rs.
///
/// Asserts on `bypass_cells` directly (pre-chokepoint) — a
/// `solve_intent_for` test would see `intent.hw_class_names == []`
/// either way (the chokepoint already strips it), so it cannot be
/// red-first for the producer fix (r31 A1, §Kani-extract-predicate).
#[tokio::test]
async fn bypass_cells_unhosted_cap_pin_drops_at_producer() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Make the reference class od-only so `Spot ∉ capacity_types_for(h)`.
    actor.sla_config.reference_hw_class = "intel-6".into();
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-6")
        .unwrap()
        .capacity_types = vec![CapacityType::Od];
    actor.test_inject_ready("d-cap", Some("test-pkg"), "x86_64-linux", false);

    let state = actor.dag.node("d-cap").unwrap();
    let (_, cost, _) = actor.solve_inputs();

    // Hosted cap (Od) passes the producer gate.
    let hosted = actor.bypass_cells(state, Some(CapacityType::Od), 4, 4 << 30, &cost, "tenant-a");
    assert_eq!(
        hosted,
        vec![("intel-6".to_owned(), CapacityType::Od)],
        "hosted `--capacity` pin MUST emit the reference cell"
    );
    // A cap the reference class refuses but a SIBLING hosts routes to
    // the sibling at the pinned capacity (merged_bug_067 — pre-fix
    // this dropped empty and the controller fallback inverted the
    // pin). The mb_003 contract is PRESERVED: no cell is emitted that
    // `retain_hosting_cells` would strip.
    let sibling = actor.bypass_cells(
        state,
        Some(CapacityType::Spot),
        4,
        4 << 30,
        &cost,
        "tenant-a",
    );
    assert_eq!(
        sibling,
        vec![("intel-7".to_owned(), CapacityType::Spot)],
        "a pin the reference class refuses routes to the first sorted \
         pin-honoring sibling"
    );
    // NO class hosts the pin → the true producer drop (empty cells,
    // NOT a chokepoint strip; the classifier mints PinGated).
    for h in ["intel-7", "intel-8", "fetcher-x86", "fetcher-arm"] {
        actor
            .sla_config
            .hw_classes
            .get_mut(h)
            .unwrap()
            .capacity_types = vec![CapacityType::Od];
    }
    let state = actor.dag.node("d-cap").unwrap();
    let dropped = actor.bypass_cells(
        state,
        Some(CapacityType::Spot),
        4,
        4 << 30,
        &cost,
        "tenant-a",
    );
    assert!(
        dropped.is_empty(),
        "`--capacity=spot` hosted by NO class MUST drop at the \
         producer (mb_003), not at retain_hosting_cells — got \
         {dropped:?}",
    );
}

/// §13e: a FOD intent must route to fetcher cells, NOT be hw-agnostic.
/// Pre-§13e: `bypass_cells` returned `[]` for FODs (no per-intent
/// affinity → the now-DELETED static `rio-fetcher` NodePool's pod
/// nodeSelector caught them). Post-§13e: FOD's `effective_features =
/// [fetcher]` → `class_routes` admits `fetcher-{x86,arm}` →
/// `retain_hosting_cells` keeps them → `cells_to_selector_terms`
/// writes the per-intent `nodeAffinity{rio.build/fetcher}`.
///
/// `pname=None` → no fit → exercises `bypass_cells` (cold-start path).
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn fod_intent_routes_to_fetcher_cell() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.test_inject_ready("fod-1", None, "x86_64-linux", true);
    let intents = actor.compute_spawn_intents(&Default::default()).intents;
    let fod = intents
        .iter()
        .find(|i| i.intent_id == "fod-1")
        .expect("FOD intent must be emitted");
    assert!(
        fod.hw_class_names.iter().any(|h| h.starts_with("fetcher-")),
        "FOD must route to a fetcher cell; got hw_class_names={:?}",
        fod.hw_class_names
    );
    assert!(
        fod.hw_class_names.iter().all(|h| h.starts_with("fetcher-")),
        "FOD must route ONLY to fetcher cells; got hw_class_names={:?}",
        fod.hw_class_names
    );
}

/// §13e: a FOD intent with a seeded cost-table fit must still route to
/// fetcher cells via `solve_full` (not just the `bypass_cells`
/// cold-start path). Pre-§13e the `!is_fixed_output` gate kept FODs
/// out of `solve_full` entirely.
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn fod_intent_with_fit_routes_to_fetcher_cell() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // `bare_actor_hw` seeds a "test-pkg" Amdahl fit; the FOD shares it
    // (lookup is by `ModelKey {pname, system, tenant}`, not `is_fod`).
    actor.test_inject_ready("fod-2", Some("test-pkg"), "x86_64-linux", true);
    let intents = actor.compute_spawn_intents(&Default::default()).intents;
    let fod = intents
        .iter()
        .find(|i| i.intent_id == "fod-2")
        .expect("FOD intent must be emitted");
    assert!(
        fod.hw_class_names.iter().any(|h| h.starts_with("fetcher-")),
        "FOD with fit must route to a fetcher cell; got hw_class_names={:?}",
        fod.hw_class_names
    );
    assert!(
        fod.hw_class_names.iter().all(|h| h.starts_with("fetcher-")),
        "FOD with fit must route ONLY to fetcher cells; got hw_class_names={:?}",
        fod.hw_class_names
    );
}

/// §13e inverse: a non-FOD builder intent must NOT route to fetcher
/// cells. The bidirectional ∅-guard makes this structural:
/// `[] ⊆ [fetcher]` fails `required.is_empty() == provides.is_empty()`.
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn builder_intent_does_not_route_to_fetcher_cell() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.test_inject_ready("build-1", Some("test-pkg"), "x86_64-linux", false);
    let intents = actor.compute_spawn_intents(&Default::default()).intents;
    let bld = intents
        .iter()
        .find(|i| i.intent_id == "build-1")
        .expect("builder intent must be emitted");
    assert!(
        !bld.hw_class_names.iter().any(|h| h.starts_with("fetcher-")),
        "builder must NOT route to a fetcher cell; got hw_class_names={:?}",
        bld.hw_class_names
    );
}

/// **r35 B0 (merged_bug_004 reverse direction)** — a non-FOD declaring
/// `requiredSystemFeatures: ["fetcher"]` has `fetcher` STRIPPED at the
/// `EffectiveFeatures::derive` chokepoint. Pre-fix the non-FOD arm of
/// `effective_features` passes the declared set through verbatim, so
/// the wire `SpawnIntent.required_features` is `["fetcher"]` — the
/// controller's `pool_covers` Fetcher tuple `["fetcher"]` accepts it,
/// FFD places it, and a fetcher node is minted that never builds it
/// (the dispatch-time `hard_filter` rejects FOD↔kind mismatch). The
/// strip closes the "release silently mints idle fetcher node" path:
/// `effective_features = []` ⟹ ∅-guard rejects the Fetcher Pool ⟹
/// routes to a Builder Pool like any non-featured drv.
///
/// Constructed via `insert_recovered_node` (NOT direct field
/// assignment) per §Spike-assertion-must-execute — proves production
/// code passes through the chokepoint.
///
/// **Pre-fix: RED** — `required_features = ["fetcher"]`, not `[]`.
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn non_fod_with_declared_fetcher_strips_routing_tag() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    seed_fit(&actor, "test-pkg");
    // Non-FOD with `requiredSystemFeatures: ["fetcher"]` — a tenant
    // CAN declare it (the gateway forwards `requiredSystemFeatures`
    // verbatim). `fetcher` is a rio-internal routing tag.
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        pname: Some("test-pkg".into()),
        is_fixed_output: false,
        required_features: vec!["fetcher".into()],
        ..crate::db::RecoveryDerivationRow::test_default("d-non-fod-fetcher", "x86_64-linux")
    });
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d-non-fod-fetcher")
        .expect("non-FOD intent emitted");
    assert_eq!(
        intent.required_features,
        Vec::<String>::new(),
        "non-FOD declaring `fetcher` must have it STRIPPED at the \
         chokepoint (`fetcher` is a rio-internal routing tag, not a \
         tenant-declarable system feature); got {:?}",
        intent.required_features,
    );
    // Routing follow-through: with `effective_features = []` the
    // intent must NOT reach fetcher cells — the ∅-guard rejects them.
    assert!(
        !intent
            .hw_class_names
            .iter()
            .any(|h| h.starts_with("fetcher-")),
        "non-FOD with stripped `fetcher` must NOT route to fetcher cells; \
         got hw_class_names={:?}",
        intent.hw_class_names,
    );
}

/// **mb_003 / r31 A3** — the producer-side `cap ∉ capacity_types_for(h)`
/// drop is NOT silent: it fires a debounced `warn!` keyed
/// `(tenant, pname, cap)` so the operator's ignored `--capacity` pin is
/// observable. Without the warn, the override looks applied but every
/// pod for the pname stalls at `fallback_cell` with no signal
/// (§Diagnostic-blind-spots). Asserts the debounce keys the LRU once
/// per `(tenant, pname, cap)` regardless of poll count.
#[tokio::test]
async fn bypass_cells_unhosted_cap_pin_warns_once() {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.reference_hw_class = "intel-6".into();
    // merged_bug_067: the warn lane keys on "NO class hosts the pin"
    // (a pin-honoring SIBLING now routes instead of warning) — make
    // every class od-only so the spot pin is truly unhosted.
    for h in [
        "intel-6",
        "intel-7",
        "intel-8",
        "fetcher-x86",
        "fetcher-arm",
    ] {
        actor
            .sla_config
            .hw_classes
            .get_mut(h)
            .unwrap()
            .capacity_types = vec![CapacityType::Od];
    }
    actor.test_inject_ready("d-cap-w", Some("test-pkg"), "x86_64-linux", false);

    let state = actor.dag.node("d-cap-w").unwrap();
    let (_, cost, _) = actor.solve_inputs();
    assert_eq!(actor.cap_mismatch_warned.lock().len(), 0);
    for _ in 0..8 {
        let _ = actor.bypass_cells(
            state,
            Some(CapacityType::Spot),
            4,
            4 << 30,
            &cost,
            "tenant-a",
        );
    }
    assert_eq!(
        actor.cap_mismatch_warned.lock().len(),
        1,
        "cap-mismatch debounce MUST key once per (tenant, pname, cap) — \
         8 polls × 1 key = 1 LRU entry (the warn fires on the first edge \
         only)"
    );
}

/// **mb_001 (r31 B0)** — `unroutable_features_warned` is bounded by
/// `UNROUTABLE_FEATURES_WARNED_CAP` (LRU), not just per-entry
/// byte-clamped. r30B2 (mb_031) clamped each entry to 64×32 ASCII
/// (~2 KiB) and added a doc-comment naming `["x-${uuid}"]` as the
/// closed threat — but `take(32)` on a 38-char UUID still leaves
/// ~2^120 distinct values, and the `HashSet` had no entry-count bound.
/// Pre-fix: `len() == 2000` (and `.put()` wouldn't compile on
/// `HashSet`). Post-fix: `len() == cap`.
#[tokio::test]
async fn unroutable_features_warned_is_bounded() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let actor = bare_actor_hw(db.pool.clone());
    let cap = crate::actor::UNROUTABLE_FEATURES_WARNED_CAP;
    for i in 0..(2 * cap) {
        actor
            .unroutable_features_warned
            .lock()
            .put(("tenant-a".to_owned(), vec![format!("f{i}")]), ());
    }
    let len = actor.unroutable_features_warned.lock().len();
    assert!(
        len <= cap,
        "unroutable_features_warned MUST be bounded by the LRU cap \
         (mb_001): inserted {} distinct keys, expected ≤{cap}, got {len}",
        2 * cap,
    );
    // The LRU should be at exactly `cap` after `2 × cap` inserts of
    // distinct keys — eviction happened.
    assert_eq!(len, cap, "LRU should be at cap after over-insertion");
}

// ──────────────────────────────────────────────────────────────────
// r33 B1 — bug_007 per-intent forecast horizon + bug_013 solve_inputs
// hoist
// ──────────────────────────────────────────────────────────────────

/// Forecast actor with TWO hwClasses and a per-class `lead_time_seed`:
/// `metal-x86` (kvm-providing, `od` lead=600s) and `mid-ebs-x86`
/// (featureless, `spot` lead=18s). Reproduces the r33 bug_007
/// shape — adding the metal seed raised the GLOBAL `max_lead` 30×.
fn bare_actor_per_intent_lead(pool: sqlx::PgPool, max_forecast_cores: u32) -> DagActor {
    use crate::sla::config::{ARCH_LABEL, CapacityType, HwClassDef, NodeLabelMatch};
    let mut sla = test_sla_config();
    sla.hw_classes.clear();
    sla.hw_classes.insert(
        "metal-x86".into(),
        HwClassDef {
            labels: vec![NodeLabelMatch {
                key: ARCH_LABEL.into(),
                value: "amd64".into(),
            }],
            max_cores: Some(64),
            max_mem: Some(256 << 30),
            provides_features: vec!["kvm".into()],
            ..Default::default()
        },
    );
    sla.hw_classes.insert(
        "mid-ebs-x86".into(),
        HwClassDef {
            labels: vec![NodeLabelMatch {
                key: ARCH_LABEL.into(),
                value: "amd64".into(),
            }],
            max_cores: Some(64),
            max_mem: Some(256 << 30),
            ..Default::default()
        },
    );
    sla.lead_time_seed
        .insert(("metal-x86".into(), CapacityType::Od), 600.0);
    sla.lead_time_seed
        .insert(("mid-ebs-x86".into(), CapacityType::Spot), 18.0);
    sla.max_forecast_cores_per_tenant = max_forecast_cores;
    sla.reference_hw_class = "mid-ebs-x86".into();
    bare_actor_cfg(
        pool,
        DagActorConfig {
            sla,
            ..Default::default()
        },
    )
}

/// **r33 bug_007 — pre-solve gate.** The forecast horizon is
/// per-intent: `max(lead_time_seed[(h,cap)])` over hwClasses
/// `class_routes` admits for `(system, features)` — NOT the global
/// `max(values())`. r31 added `metal-{arm,x86}:od leadTimeSeed=600`,
/// raising the global max 30×. A featureless drv with `eta∈(18, 600)`
/// is admitted under the global max, runs `solve_intent_for`, debits
/// the tenant budget — then the controller's per-cell `a_open` filter
/// (`eta < lead_time(c)` ≈ 18s for non-metal cells) drops it with no
/// fallback. With the per-intent horizon, the scheduler drops it
/// pre-solve and emits `forecast_dropped_total{reason=lead_horizon}`.
///
/// Pre-fix: RED — drv `q-shallow` is admitted, no metric.
// r[verify sched.sla.forecast.one-layer+2]
#[tokio::test]
async fn forecast_lead_horizon_per_intent() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_per_intent_lead(db.pool.clone(), 2_000);

    // dep(Running, eta≈300) → q-shallow(Queued, featureless).
    actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("q-shallow", "x86_64-linux", DerivationStatus::Queued);
    actor.test_inject_edge("q-shallow", "dep");
    actor.test_set_running_eta("dep", 400.0, 100, 4); // eta = 400-100 = 300

    let rec = DebuggingRecorder::new();
    let snapr = rec.snapshotter();
    let snap = {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.compute_spawn_intents(&Default::default())
    };

    assert!(
        !snap.intents.iter().any(|i| i.intent_id == "q-shallow"),
        "featureless drv with eta=300 MUST NOT be admitted: its routable \
         classes' lead is max(mid-ebs-x86:spot=18)=18 < 300; the metal \
         class's seed=600 is for kvm intents only. Got intents: {:?}",
        snap.intents
            .iter()
            .map(|i| &i.intent_id)
            .collect::<Vec<_>>(),
    );
    let dropped = crate::sla::metrics::counter_map_by(
        &snapr,
        "rio_scheduler_sla_forecast_dropped_total",
        Some("reason"),
    );
    assert_eq!(
        dropped.get("lead_horizon"),
        Some(&1),
        "forecast_dropped_total{{reason=lead_horizon}} MUST be emitted at \
         the pre-solve gate for the dropped intent; got {dropped:?}"
    );
}

/// **r33 bug_007 — kvm intent KEEPS the metal lead.** The per-intent
/// horizon over `class_routes`-admissible classes still yields the
/// metal seed (600s) for kvm intents. `eta=300 < 600` → admitted.
/// Pre-fix: GREEN (the global max already admitted it). This is the
/// inverse — the per-intent gate must not over-fire.
// r[verify sched.sla.forecast.one-layer+2]
#[tokio::test]
async fn forecast_kvm_intent_uses_metal_lead() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_per_intent_lead(db.pool.clone(), 2_000);

    // dep(Running, eta≈300) → q-kvm(Queued, requires kvm). r35: route
    // through `set_required_features` so `effective_features`
    // re-derives — `compute_spawn_intents` reads the derived set.
    actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Running);
    actor.test_inject_at("q-kvm", "x86_64-linux", DerivationStatus::Queued);
    actor
        .dag
        .node_mut("q-kvm")
        .unwrap()
        .set_required_features(vec!["kvm".into()]);
    actor.test_inject_edge("q-kvm", "dep");
    actor.test_set_running_eta("dep", 400.0, 100, 4);

    let snap = actor.compute_spawn_intents(&Default::default());
    assert!(
        snap.intents.iter().any(|i| i.intent_id == "q-kvm"),
        "kvm drv with eta=300 MUST be admitted: its routable class \
         (metal-x86:od) has lead=600 > 300. Got intents: {:?}",
        snap.intents
            .iter()
            .map(|i| &i.intent_id)
            .collect::<Vec<_>>(),
    );
}

/// **r33 bug_007 — budget drop is observable.** The per-tenant
/// forecast budget `continue` was a silent drop — the operator never
/// sees that the forecast pass burned a budget slot on an intent that
/// then got bumped by a higher-priority sibling. Emit
/// `forecast_dropped_total{reason=tenant_budget}` so the rate is
/// visible.
///
/// Pre-fix: RED — no metric on the budget `continue`.
// r[verify sched.sla.forecast.tenant-ceiling]
#[tokio::test]
async fn forecast_budget_drop_metric() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // probe.cpu=4 cores per intent. Cap=4 → admits ONE forecast intent.
    let mut actor = bare_actor_per_intent_lead(db.pool.clone(), 4);

    // dep(Running, eta≈300) → {qa, qb} (Queued, kvm — high lead = 600).
    // r35: route through `set_required_features` so `effective_features`
    // re-derives — `compute_spawn_intents` reads the derived set.
    actor.test_inject_at("dep", "x86_64-linux", DerivationStatus::Running);
    for q in ["qa", "qb"] {
        actor.test_inject_at(q, "x86_64-linux", DerivationStatus::Queued);
        actor
            .dag
            .node_mut(q)
            .unwrap()
            .set_required_features(vec!["kvm".into()]);
        actor.test_inject_edge(q, "dep");
    }
    actor.test_set_running_eta("dep", 400.0, 100, 4);

    let rec = DebuggingRecorder::new();
    let snapr = rec.snapshotter();
    let snap = {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.compute_spawn_intents(&Default::default())
    };

    let forecast: Vec<_> = snap
        .intents
        .iter()
        .filter(|i| i.ready == Some(false))
        .collect();
    assert_eq!(
        forecast.len(),
        1,
        "budget=4, intents are 4 cores each → exactly one forecast intent \
         admitted; got {} ({:?})",
        forecast.len(),
        forecast.iter().map(|i| &i.intent_id).collect::<Vec<_>>(),
    );
    let dropped = crate::sla::metrics::counter_map_by(
        &snapr,
        "rio_scheduler_sla_forecast_dropped_total",
        Some("reason"),
    );
    assert_eq!(
        dropped.get("tenant_budget"),
        Some(&1),
        "forecast_dropped_total{{reason=tenant_budget}} MUST be emitted at \
         the budget continue; got {dropped:?}"
    );
}

/// **First pull clears the single-cell ICE mask** (mechanism #22's
/// clear half, pull-mode trigger): a pull-mode pod's first successful
/// pull is the success edge for its intent — the cell the spawn ack
/// armed is cleared exactly as the stream path's registration edge
/// clears it — while a `NotYetReady` answer (the pod has not taken
/// work) clears nothing and leaves the armed entry in place. The
/// stream-mode registration edge itself is untouched (its existing
/// contract tests above keep covering it).
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn contract_first_pull_clears_ice_not_yet_ready_does_not() {
    use crate::actor::pull::PullOutcome;
    use crate::sla::config::CapacityType;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    // Real merges (durable rows) so the fenced pull mint can commit:
    // one Ready drv, plus a parent whose dep is unbuilt (the
    // NotYetReady waiter).
    let merge = |nodes: Vec<rio_proto::types::DerivationNode>,
                 edges: Vec<rio_proto::types::DerivationEdge>| MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: None,
        priority_class: PriorityClass::Scheduled,
        nodes,
        edges,
        options: BuildOptions::default(),
        keep_going: false,
        traceparent: String::new(),
        jti: None,
        jwt_token: None,
    };
    actor
        .handle_merge_dag(merge(vec![make_node("ice-pull-a")], vec![]))
        .await
        .expect("merge ready drv");
    actor
        .handle_merge_dag(merge(
            vec![make_node("ice-pull-dep"), make_node("ice-pull-b")],
            vec![make_test_edge("ice-pull-b", "ice-pull-dep")],
        ))
        .await
        .expect("merge waiter dag");

    // Controller flow for the Ready drv: poll → ack the emitted intent
    // (arms dispatched_cells from the wire form) and report its cell
    // unfulfillable (marks ICE).
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "ice-pull-a")
        .expect("ready drv emitted")
        .clone();
    assert_eq!(
        intent.hw_class_names.len(),
        1,
        "precondition: single-cell A' (the |A'|=1 clear discipline applies)"
    );
    let cap = intent.node_affinity[0]
        .match_expressions
        .iter()
        .find(|r| r.key == "karpenter.sh/capacity-type")
        .and_then(|r| r.values.first())
        .cloned()
        .expect("affinity term carries the capacity type");
    let cell: crate::sla::config::Cell = (
        intent.hw_class_names[0].clone(),
        CapacityType::parse(&cap).expect("known capacity type"),
    );
    let cell_str = format!("{}:{cap}", intent.hw_class_names[0]);
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&intent),
            std::slice::from_ref(&cell_str),
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied under leadership");
    // Arm + mark the waiter's intent the same way (hand-built echo with
    // the same cell — the ack handler arms from the wire form alone).
    let waiter_intent = rio_proto::types::SpawnIntent {
        intent_id: "ice-pull-b".into(),
        hw_class_names: intent.hw_class_names.clone(),
        node_affinity: intent.node_affinity.clone(),
        ..Default::default()
    };
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&waiter_intent),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied under leadership");
    assert!(actor.ice.is_masked(&cell), "precondition: cell ICE-masked");
    assert!(actor.dispatched_cells.contains_key("ice-pull-a"));
    assert!(actor.dispatched_cells.contains_key("ice-pull-b"));

    // A NotYetReady answer (deps unbuilt) clears nothing: the pod has
    // not taken work.
    let (tx, rx) = tokio::sync::oneshot::channel();
    actor
        .handle_pull_assignment(
            "ice-pull-b".into(),
            Some("ice-pull-b".into()),
            rio_evidence_kernel::pull::PullKind::Build,
            None,
            None,
            None,
            false,
            Some("tokhash-pod-a".into()),
            tx,
        )
        .await;
    assert!(
        matches!(
            rx.await.expect("reply"),
            Ok(PullOutcome::NotYetReady { .. })
        ),
        "precondition: the waiter answers NotYetReady"
    );
    assert!(
        actor.ice.is_masked(&cell),
        "NotYetReady is not a success edge — the mask stays"
    );
    assert!(
        actor.dispatched_cells.contains_key("ice-pull-b"),
        "the armed entry stays until the pod actually takes work"
    );

    // The first successful pull of the Ready drv IS the success edge.
    let (tx, rx) = tokio::sync::oneshot::channel();
    actor
        .handle_pull_assignment(
            "ice-pull-a".into(),
            Some("ice-pull-a".into()),
            rio_evidence_kernel::pull::PullKind::Build,
            None,
            None,
            None,
            false,
            Some("tokhash-pod-a".into()),
            tx,
        )
        .await;
    assert!(
        matches!(rx.await.expect("reply"), Ok(PullOutcome::Deliver(_))),
        "precondition: the pull delivered"
    );
    assert!(
        !actor.ice.is_masked(&cell),
        "the first successful pull clears the single-cell ICE mask"
    );
    assert!(
        !actor.dispatched_cells.contains_key("ice-pull-a"),
        "the armed entry is consumed by the clear, exactly like the registration edge"
    );
}

// r[verify obs.metric.scheduler-leader-gate+5]
/// bug_310's structural pin: `handle_leader_lost` writes the
/// cost-table edge-reload latch false through the LEADER_EDGES table.
/// Pre-table, NO lose-side writer existed — `cost_was_leader` stayed
/// true across an A→B→A lease flap inside one 600s housekeeping tick,
/// so the prelude's `!was_leader` reload check
/// (r[sched.sla.cost-leader-edge-reload+1], pinned by the prelude tests
/// in sla/cost.rs) was skipped and the tick body persisted the deposed
/// tenure's prices. Composed invariant: this lose-edge store + the
/// prelude test = "the first leader tick after ANY acquire edge
/// reloads before persist", wake-timing independent (a Notify-based
/// lose signal would coalesce with the re-acquire nudge).
#[tokio::test]
async fn leader_lost_writes_cost_latch_false() {
    use std::sync::atomic::Ordering;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.cost_was_leader.store(true, Ordering::Relaxed);

    actor.handle_leader_lost();

    assert!(
        !actor.cost_was_leader.load(Ordering::Relaxed),
        "handle_leader_lost must store cost_was_leader=false (the LEADER_EDGES \
         lose cell) — without it a lose→re-acquire flap inside one housekeeping \
         tick skips the cost-table edge reload and persists stale prices"
    );
}

/// The paired-hook table is total: every edge has BOTH cells written
// r[verify sched.lease.rebound+4]
/// merged_bug_212: a REBOUND transition (holder change observed late on
/// a still-leading round) must run the Compound lose cells — the cost
/// latch in particular — before its re-acquire effects. Pre-fix the
/// rebound delivered plain `LeaderAcquired` (the lease loop fired
/// `on_acquire`), which runs only the acquire cells: `cost_was_leader`
/// stayed true, so the first housekeeping tick after the foreign term
/// skipped the edge reload and persisted prices over the foreign
/// tenure's evolved EMA — bug_310's defect class escaping through the
/// edge table's missing rebound axis.
#[tokio::test]
async fn rebound_runs_cost_latch_lose_cell() {
    use std::sync::atomic::Ordering;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // The latch state of a leading tenure whose housekeeping already
    // reloaded (the steady state a rebound interrupts).
    actor.cost_was_leader.store(true, Ordering::Relaxed);

    actor.handle_leader_rebound().await;

    assert!(
        !actor.cost_was_leader.load(Ordering::Relaxed),
        "a rebound transition must run the cost-latch lose cell \
         (cost_was_leader=false) before its re-acquire effects — \
         otherwise the post-foreign-term housekeeping tick skips the \
         edge reload and persists prices from the pre-term table"
    );
    // And the acquire half still ran: the cost-latch acquire cell's
    // housekeeping nudge is observable as a stored notify permit.
    assert!(
        futures_util::FutureExt::now_or_never(actor.cost_reload_notify.notified()).is_some(),
        "the rebound's acquire half must nudge the housekeeping reload"
    );
}

/// (no-ops are explicit fn pointers, so this is a compile-time
/// property), and the acquire cell of the cost latch actually nudges
/// the housekeeping notify (permit-based — observable as an immediate
/// `notified()` completion).
#[tokio::test]
async fn leader_edges_acquire_cells_fire() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let actor = bare_actor_hw(db.pool.clone());

    for edge in crate::observability::LEADER_EDGES {
        (edge.on_acquire)(&actor);
    }
    // The cost-latch acquire cell stored a notify permit.
    assert!(
        futures_util::FutureExt::now_or_never(actor.cost_reload_notify.notified()).is_some(),
        "cost-table LEADER_EDGES acquire cell must notify_one() the \
         housekeeping edge-reload"
    );
}

// r[verify sched.sla.hw-class.ice-mask]
/// bug_067 red: a leadership cycle re-opens the per-cell evidence-
/// epoch watermark so a LOWER-epoch successor lineage's genuine
/// evidence applies — and the retention is per-field (the ladder
/// survives the lose edge, the watermark does not).
///
/// Defect: the prose priced the handoff on a `last_applied` wipe that
/// did not exist as an API — `EpochMint` is `Default prev=0` seeded
/// `max(now, prev+1)`, so a clock-behind successor controller's
/// genuine marks hit the `epoch_gate` NoOp arm until clock catch-up; a
/// no-op'd genuine mark leaves a sick cell UNMASKED (absence of a mask
/// has no TTL to heal). Witness strength: certifies "a leadership
/// cycle re-opens the watermark so a lower-epoch successor lineage
/// applies" — the adjudication's load-bearing leg itself (the
/// `epoch_gate_table` keeps certifying the single-lineage gate
/// algebra, which is untouched). Pre-fix, verbatim: the post-expiry
/// successor mark NoOp'd — step stayed Some(0) and the cell read
/// UNMASKED while sick.
///
/// World built through production constructors only: marks ride the
/// production `handle_ack_spawned_intents` wire grammar
/// (`"h:cap@epoch"`); the lose edge is the production
/// `handle_leader_lost()` (the same entry the bug_310 latch pins
/// drive); `force_expire` is the sanctioned cfg(test) expiry shim the
/// refresh-not-step suite already uses.
#[tokio::test]
async fn leadership_cycle_resets_the_epoch_watermark() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let cell: crate::sla::config::Cell =
        ("mid-ebs-x86".into(), crate::sla::config::CapacityType::Spot);

    // Old controller lineage: genuine mark at epoch 1000 — masked,
    // watermark ratchets to 1000.
    actor
        .handle_ack_spawned_intents(
            &[],
            &["mid-ebs-x86:spot@1000".into()],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("mark applied under leadership");
    assert_eq!(
        actor.ice.step(&cell),
        Some(0),
        "precondition: masked at step 0"
    );

    // The scheduler loses the lease (production lose edge — runs every
    // LEADER_EDGES lose cell, the new ice-epoch-watermark row
    // included).
    actor.handle_leader_lost();

    // Per-field retention, pinned by assertion not prose: the LADDER
    // entry survived the lose edge…
    assert_eq!(
        actor.ice.step(&cell),
        Some(0),
        "the TTL'd ladder is RETAINED across the leadership edge"
    );
    // …and the cell's mask is then expired (the sick cell's window
    // lapsed; only a GENUINE new mark can re-mask it).
    actor.ice.force_expire(&cell);
    assert!(!actor.ice.is_masked(&cell), "precondition: window lapsed");

    // Successor controller lineage with a BEHIND clock mints epoch
    // 500. Pre-fix: NoOp (500 <= 1000) — the genuine consecutive
    // failure is black-holed, the sick cell stays unmasked and the
    // step never climbs. Post-fix: the watermark was reset at the
    // lose edge, so the mark APPLIES — post-expiry consecutive
    // failure climbs the ladder and re-masks.
    actor
        .handle_ack_spawned_intents(
            &[],
            &["mid-ebs-x86:spot@500".into()],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("successor-lineage mark applied");
    assert_eq!(
        actor.ice.step(&cell),
        Some(1),
        "the clock-behind successor lineage's genuine mark must APPLY \
         (post-expiry consecutive failure climbs) — not no-op against \
         the previous lineage's watermark"
    );
    assert!(
        actor.ice.is_masked(&cell),
        "the sick cell is masked again by the genuine successor evidence"
    );
}

// r[verify ctrl.nodeclaim.ice-mark-clear+4]
/// W7-D leg B (R4-B — a DISCLOSED green-side consumption pin, never
/// claimed as a red: this WO contains zero rio-scheduler production
/// edits, so a scheduler-side test cannot regress pre-fix). Certifies:
/// *an acked vanish mark — minted by the production
/// `rio_common::cell_wire::encode_cell_event` codec, the EXACT call
/// the controller's evidence builder makes, with epoch >
/// `last_applied[cell]` so the apply is not an epoch-gate no-op —
/// masks `(h, spot)` in IceBackoff, and the next solve's
/// `A \ ice_masked` chooses an OnDemand cell* (structural: the solved
/// cells' capacity types, not log text). Composes with leg A
/// (`never_registered_vanish_ships_the_mark_on_the_wire`,
/// rio-controller lifecycle_tests) over the shared wire codec — the
/// W7-D pinned-composition convention (rio-scheduler has no dependency
/// on rio-controller, so no single fn can drive both halves).
#[tokio::test]
async fn acked_vanish_mark_masks_spot_and_solve_buys_od() {
    use crate::sla::config::CapacityType;
    use rio_common::cell_wire::{EvidenceEpoch, WireCapacity, encode_cell_event};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // τ widened so the od cells are ADMITTED into A beside spot (seed
    // od/spot price ratio ≈ 1/0.35 ≈ 2.86 — the default deadband
    // excludes od while spot exists; the read-time `A \ ice_masked`
    // subtraction is what this pin certifies, and it can only choose
    // od if od is in A). The capacity-type rungs themselves default
    // [spot, od] per class.
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.sla_config.hw_cost_tolerance = 2.0;
    actor.test_inject_ready("d0", Some("test-pkg"), "x86_64-linux", false);

    // Pre-mask solve: the admissible set prefers spot (cost order).
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d0")
        .expect("d0 emitted")
        .clone();
    let caps_of = |i: &rio_proto::types::SpawnIntent| -> Vec<String> {
        i.node_affinity
            .iter()
            .filter_map(|t| {
                t.match_expressions
                    .iter()
                    .find(|r| r.key == "karpenter.sh/capacity-type")
                    .and_then(|r| r.values.first().cloned())
            })
            .collect()
    };
    assert!(
        caps_of(&intent).iter().any(|c| c == "spot"),
        "precondition: pre-mask solve offers spot: {:?}",
        caps_of(&intent)
    );

    // Leg-B payload: every builder class's spot cell marked via the
    // production codec (the controller-side vanish marks for a
    // spot-wide launch-failure event), epoch 1 > last_applied (empty).
    let marks: Vec<String> = ["intel-6", "intel-7", "intel-8"]
        .iter()
        .map(|h| encode_cell_event(h, WireCapacity::Spot, Some(EvidenceEpoch(1))))
        .collect();
    actor
        .handle_ack_spawned_intents(&[], &marks, &[], &[], &[], None, &[])
        .expect("applied under leadership");
    for h in ["intel-6", "intel-7", "intel-8"] {
        let cell: crate::sla::config::Cell = (h.into(), CapacityType::Spot);
        assert!(
            actor.ice.step(&cell).is_some(),
            "IceBackoff masks ({h}, spot) from the acked vanish mark"
        );
    }

    // The failover: the next solve's A \ ice_masked chooses OnDemand.
    let snap2 = actor.compute_spawn_intents(&Default::default());
    let intent2 = snap2
        .intents
        .iter()
        .find(|i| i.intent_id == "d0")
        .expect("d0 re-emitted")
        .clone();
    let caps = caps_of(&intent2);
    assert!(
        !caps.is_empty() && caps.iter().all(|c| c == "on-demand"),
        "post-mask solve buys od — structural on the solved cells' \
         capacity types: {caps:?}"
    );
}

// r[verify ctrl.nodeclaim.capacity-ladder]
/// W7-E (R5 — the WO-S7-5 advance red): certifies *rung-1 cells
/// ICE'd through the WO-S7-4 evidence path (production `cell_wire`
/// marks via `handle_ack_spawned_intents`, epoch > `last_applied`)
/// ⇒ the next emission carries a STRICTLY DIFFERENT unmasked rung —
/// every unmasked cell's class+capacity ∉ rung-1's coordinates* — the
/// chosen→assigned half of W7-D's failover chain (the assigned half
/// is the controller's `assign_to_cells` walk, cover.rs W7-F battery).
///
/// The fixture stays at the DEFAULT cost tolerance: the cap-only seed
/// ratio (od/spot ≈ 2.86) keeps od cells OUTSIDE the admissible set,
/// so pre-fix a spot-wide ICE leaves the emission with ZERO unmasked
/// cells (`A \ masked = ∅` → the full-A fallback re-emits the masked
/// spot cells) — the live_050 starved shape. The ladder's hosting
/// closure is deadband-INDEPENDENT membership: the rung's od cell is
/// in `hw_class_names` from the FIRST emission (membership, not a
/// mask-triggered special case) without widening τ, and once the spot
/// plane is iced it is the walk's advance target.
#[tokio::test]
async fn rung_one_ice_advances_to_a_different_rung() {
    use crate::sla::config::{CapacityLadder, LadderRung};
    use rio_common::cell_wire::{EvidenceEpoch, WireCapacity, encode_cell_event};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    // The generation-rung sibling: a full class row (ceilings, labels,
    // capacity types all its own) + the parent's ladder naming it.
    let rung_def = {
        let mut d = actor.sla_config.hw_classes["intel-8"].clone();
        d.labels[0].value = "intel-8r7".into();
        d
    };
    actor
        .sla_config
        .hw_classes
        .insert("intel-8r7".into(), rung_def);
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-8")
        .unwrap()
        .ladder = Some(CapacityLadder {
        rungs: vec![LadderRung {
            class: "intel-8r7".into(),
        }],
    });
    actor.test_inject_ready("d0", Some("test-pkg"), "x86_64-linux", false);

    let cells_of = |i: &rio_proto::types::SpawnIntent| -> Vec<(String, String)> {
        i.hw_class_names
            .iter()
            .cloned()
            .zip(i.node_affinity.iter().map(|t| {
                t.match_expressions
                    .iter()
                    .find(|r| r.key == "karpenter.sh/capacity-type")
                    .and_then(|r| r.values.first().cloned())
                    .expect("every emitted cell carries the capacity term")
            }))
            .collect()
    };
    // Pre-mask: the solve's admissible set is spot-plane only (od is
    // ≈2.86× the cap-only spot seed — outside the default deadband);
    // the closure ADDS the declared rung's od cell as membership.
    // rung-1 (the realized first rungs, capacity-major) = the spot
    // cells.
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d0")
        .expect("d0 emitted")
        .clone();
    let rung_one: Vec<(String, String)> = cells_of(&intent)
        .into_iter()
        .filter(|(_, cap)| cap == "spot")
        .collect();
    assert!(
        !rung_one.is_empty(),
        "precondition: the solve admitted spot cells: {:?}",
        cells_of(&intent)
    );
    assert!(
        cells_of(&intent).contains(&("intel-8r7".into(), "on-demand".into())),
        "the closure carries the rung od cell from the FIRST emission \
         (membership, not a mask reaction): {:?}",
        cells_of(&intent)
    );

    // Rung-1 ICE'd: every spot cell marked through the production
    // codec (the WO-S7-4 evidence path — the leg-B convention of
    // W7-D).
    let marks: Vec<String> = ["intel-6", "intel-7", "intel-8", "intel-8r7"]
        .iter()
        .map(|h| encode_cell_event(h, WireCapacity::Spot, Some(EvidenceEpoch(1))))
        .collect();
    actor
        .handle_ack_spawned_intents(&[], &marks, &[], &[], &[], None, &[])
        .expect("applied under leadership");

    // The advance: the next emission's UNMASKED set is non-empty and
    // every unmasked cell differs from every rung-1 coordinate —
    // specifically the declared rung's od cell, which the deadband
    // refused and the closure admitted. Pre-fix left (reverse-strawman
    // transcript in the commit body): unmasked == [] — the emission
    // re-offers only the masked spot cells and the intent starves.
    let snap2 = actor.compute_spawn_intents(&Default::default());
    let intent2 = snap2
        .intents
        .iter()
        .find(|i| i.intent_id == "d0")
        .expect("d0 re-emitted")
        .clone();
    let masked: std::collections::HashSet<(String, String)> = snap2
        .ice_masked_cells
        .iter()
        .map(|s| {
            let (h, cap) = crate::sla::config::parse_cell(s).expect("snapshot cell decodes");
            (h, cap.label().to_string())
        })
        .collect();
    let unmasked: Vec<(String, String)> = cells_of(&intent2)
        .into_iter()
        .filter(|c| !masked.contains(c))
        .collect();
    assert!(
        !unmasked.is_empty(),
        "the walk has an unmasked rung to advance to (pre-fix: empty)"
    );
    assert!(
        unmasked.iter().all(|c| !rung_one.contains(c)),
        "every unmasked cell is a STRICTLY DIFFERENT rung (class+capacity \
         ∉ rung-1 {rung_one:?}): {unmasked:?}"
    );
    assert!(
        unmasked.contains(&("intel-8r7".into(), "on-demand".into())),
        "the declared rung's od cell is in the closure: {unmasked:?}"
    );
}

// r[verify scheduler.sla.ceiling.catalog-derived+4]
/// R16 — the ARM-INDEPENDENT composed red (the CE-5 closure; the
/// phantom-ceiling → ICE → walk composition across WO-S7-6 and
/// WO-S7-5, witnessed via the slot's pinned-composition convention):
/// ceilings GROUNDED through the production `derive_ceilings` over a
/// catalog that CONTAINS the phantom row (the committed exclusion
/// active — the WO-S7-6 fixture lane), the WO-S7-4 evidence path ICEs
/// rung-1, and the ladder walk ADVANCES to a launchable rung — never
/// sized to the phantom, never starved while a buyable rung exists.
/// Leg coordinates (W7-D's form): leg A = the grounding
/// (`derive_ceilings` → `set_catalog_ceilings`, this fn); the codec =
/// `cell_wire::encode_cell_event` marks; leg B = the emission's
/// unmasked rung (the W7-E shape); the assigned half = the cover.rs
/// walk battery.
#[tokio::test]
async fn phantom_ceiling_rung_advances_not_starves() {
    use crate::sla::config::{CapacityLadder, LadderRung};
    use rio_common::cell_wire::{EvidenceEpoch, WireCapacity, encode_cell_event};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    // Ladder: intel-8 -> intel-8r7 (the WO-S7-5 machinery).
    let rung_def = {
        let mut d = actor.sla_config.hw_classes["intel-8"].clone();
        d.labels[0].value = "intel-8r7".into();
        d
    };
    actor
        .sla_config
        .hw_classes
        .insert("intel-8r7".into(), rung_def);
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-8")
        .unwrap()
        .ladder = Some(CapacityLadder {
        rungs: vec![LadderRung {
            class: "intel-8r7".into(),
        }],
    });
    // The grounding (WO-S7-6): catalog ceilings derived from a catalog
    // CONTAINING the phantom, with the committed exclusion active —
    // every class lands at the launchable 191, never 383.
    let cat_entry = |name: &str, cores: u32, mem_gib: u64| {
        let (family, size) = name.split_once('.').unwrap();
        let category: String = family
            .chars()
            .take_while(|c| c.is_ascii_alphabetic())
            .collect();
        let generation: String = family
            .chars()
            .skip_while(|c| c.is_ascii_alphabetic())
            .take_while(|c| c.is_ascii_digit())
            .collect();
        let mut labels = std::collections::BTreeMap::new();
        labels.insert("karpenter.k8s.aws/instance-category", category);
        labels.insert("karpenter.k8s.aws/instance-generation", generation);
        labels.insert("karpenter.k8s.aws/instance-size", size.to_owned());
        labels.insert("kubernetes.io/arch", "amd64".to_owned());
        labels.insert("karpenter.k8s.aws/instance-local-nvme", "0".to_owned());
        crate::sla::catalog::CatalogEntry {
            name: name.into(),
            cores,
            mem_bytes: mem_gib << 30,
            labels,
        }
    };
    let catalog_rows = vec![
        cat_entry("r8i.96xlarge", 384, 3072),
        cat_entry("c8a.48xlarge", 192, 384),
    ];
    let unlaunchable: Vec<String> = ["96xlarge", "metal-96xl"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let ceilings = crate::sla::catalog::derive_ceilings(
        &catalog_rows,
        &actor.sla_config.hw_classes,
        &[],
        &unlaunchable,
    );
    for (h, &(c, _)) in &ceilings {
        assert_eq!(c, 191, "{h}: grounded at the launchable ceiling, not 383");
    }
    actor.cost_table.write().set_catalog_ceilings(ceilings);
    actor.test_inject_ready("d0", Some("test-pkg"), "x86_64-linux", false);

    // First emission under the grounded ceilings.
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d0")
        .expect("d0 emitted")
        .clone();
    assert!(
        intent.cores <= 191,
        "no demand sized to the phantom: cores={} <= 191",
        intent.cores
    );

    // The WO-S7-4 evidence path ICEs the spot plane (rung-1).
    let marks: Vec<String> = ["intel-6", "intel-7", "intel-8", "intel-8r7"]
        .iter()
        .map(|h| encode_cell_event(h, WireCapacity::Spot, Some(EvidenceEpoch(1))))
        .collect();
    actor
        .handle_ack_spawned_intents(&[], &marks, &[], &[], &[], None, &[])
        .expect("applied under leadership");

    // The walk advances: a launchable unmasked rung exists (the
    // ladder's od cell) — placement-able, never starved while a
    // buyable rung exists.
    let snap2 = actor.compute_spawn_intents(&Default::default());
    let intent2 = snap2
        .intents
        .iter()
        .find(|i| i.intent_id == "d0")
        .expect("d0 re-emitted")
        .clone();
    let masked: std::collections::HashSet<(String, String)> = snap2
        .ice_masked_cells
        .iter()
        .map(|s| {
            let (h, cap) = crate::sla::config::parse_cell(s).expect("snapshot cell decodes");
            (h, cap.label().to_string())
        })
        .collect();
    let unmasked: Vec<(String, String)> = intent2
        .hw_class_names
        .iter()
        .cloned()
        .zip(intent2.node_affinity.iter().map(|t| {
            t.match_expressions
                .iter()
                .find(|r| r.key == "karpenter.sh/capacity-type")
                .and_then(|r| r.values.first().cloned())
                .expect("capacity term present")
        }))
        .filter(|c| !masked.contains(c))
        .collect();
    assert!(
        unmasked.contains(&("intel-8r7".into(), "on-demand".into())),
        "the phantom-grounded universe still walks: unmasked rung \
         present after rung-1 ICE: {unmasked:?}"
    );
    assert!(
        intent2.cores <= 191,
        "the re-emission stays grounded: cores={}",
        intent2.cores
    );
}

// ===========================================================================
// WO-S7-7 — stale-solve revalidation: the CellEmission alphabet, the
// floor clamp law, and the no-hosting-class verdict consumer
// (live_050(e) + live_051(b)/(d)/(c)).
// ===========================================================================

/// Shrink every builder class's catalog ceiling to `(cores, mem)` —
/// the WO-S7-6 catalog fixture lane in its minimal form (the ceiling
/// vector swap between two emission passes).
fn shrink_catalog(actor: &DagActor, cores: u32, mem: u64) {
    let mut c = std::collections::HashMap::new();
    for h in actor.sla_config.hw_classes.keys() {
        c.insert(h.clone(), (cores, mem));
    }
    actor.cost_table.write().set_catalog_ceilings(c);
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **R17 + W7-Q + W7-S** — *a demand envelope solved under ceiling C,
/// with ceilings then shrunk to C′ < demand, is RE-SOLVED on the next
/// emission pass*: the emitted intent carries cells hostable under C′
/// (structural: non-empty cells, each named class's live ceiling ≥ the
/// re-solved dims). Promptness is operation-count (W7-S): the FIRST
/// `compute_spawn_intents` after the shrink re-solves — no deadline
/// pacing, zero wall-clock. Production emission pass end-to-end; the
/// ceiling swap rides the catalog fixture lane.
///
/// Pre-fix red (left, reverse-strawman transcript in the commit body):
/// `hw_class_names == []` — the fitted drv's BestEffort solve fell to
/// the hw-agnostic fallback and `bypass_cells` emitted empty SILENTLY;
/// the controller would churn it as `no_hosting_class` forever (the
/// measured post-rev-3 live loop).
#[tokio::test]
async fn stale_envelope_revalidates_on_ceiling_shrink() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-stale", Some("test-pkg"), "x86_64-linux", false);

    // Pass 1: solved under the BIG ceilings — hw-routed cells.
    let snap = actor.compute_spawn_intents(&Default::default());
    let i1 = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d-stale")
        .expect("emitted");
    assert!(
        !i1.hw_class_names.is_empty(),
        "precondition: solved hw-routed under C"
    );

    // The shrink: every class's catalog ceiling drops below the fitted
    // demand (mem p90 6 GiB ≫ 1 GiB).
    shrink_catalog(&actor, 2, 1 << 30);

    // Pass 2 — the FIRST pass after the shrink (W7-S): re-solved, not
    // empty. Every named class hosts the re-solved dims under C′.
    let snap2 = actor.compute_spawn_intents(&Default::default());
    let i2 = snap2
        .intents
        .iter()
        .find(|i| i.intent_id == "d-stale")
        .expect("re-emitted");
    assert!(
        !i2.hw_class_names.is_empty(),
        "the stale envelope RE-SOLVES at the next emission — never empty cells \
         for non-agnostic demand (pre-fix: [] silent)"
    );
    let catalog = actor.cost_table.read().clone();
    for h in &i2.hw_class_names {
        let (cc, cm) = actor.sla_config.class_ceilings(
            h,
            catalog.catalog_ceilings(),
            catalog.resolved_global(),
        );
        assert!(
            i2.cores <= cc && i2.mem_bytes <= cm,
            "re-solved dims ({}, {}) hostable by {h} under C' ({cc}, {cm})",
            i2.cores,
            i2.mem_bytes
        );
    }
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **R18 (green-side pin, disclosed)** — *the genuinely hw-agnostic
/// emission stays quiet*: a featureless drv with NO infeasibility
/// evidence emits empty `hw_class_names` exactly as before the typing
/// (the §13e cold-start arm's regression pin — quiet BY TYPE via
/// `CellEmission::HwAgnostic`, not by shared emptiness). Green
/// pre-fix AND post-fix by design (W7-R's quiet-edge half).
#[tokio::test]
async fn hw_agnostic_emission_stays_quiet() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    // No pname → no fit → probe path → zero infeasibility evidence.
    actor.test_inject_ready("d-quiet", None, "x86_64-linux", false);
    let snap = actor.compute_spawn_intents(&Default::default());
    let i = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d-quiet")
        .expect("emitted");
    assert!(
        i.hw_class_names.is_empty() && i.node_affinity.is_empty(),
        "genuinely agnostic demand keeps the quiet fallback-cell path: {:?}",
        i.hw_class_names
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **R19 + R23 + W7-V (unhostable half) + W7-R (kill-isolation)** —
/// *feature-constrained demand no class can host even re-solved is
/// `Unhostable` + LOUD, never empty-silent*: a FOD whose clamped floor
/// exceeds every fetcher class's live ceiling emits typed-empty WITH
/// the disclosure (the `exit="unhostable"` counter + warn naming
/// demand and best class). The task-named red (live_051(b)): pre-fix
/// left = `hw_class_names == []` with ZERO disclosure (counter absent)
/// — indistinguishable from hw-agnostic; the controller would churn
/// `no_hosting_class`.
///
/// The floor value models the persisted-row population (a prior run's
/// OOM ladder under bigger ceilings) — r13-allow(frozen-legacy): the
/// adversarial INPUT is legacy persisted state by definition; the
/// bump-law provenance of such values is pinned by the floor.rs unit
/// battery.
#[tokio::test]
async fn infeasible_everywhere_drv_emits_unhostable_not_empty_cells() {
    let metrics = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = metrics.snapshotter();
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // FOD → effective features [fetcher] → fetcher-* candidates only.
    actor.test_inject_ready("d-fod", Some("fod-pkg"), "x86_64-linux", true);
    // Fetcher catalog ceilings shrink below the (clamped) floor.
    shrink_catalog(&actor, 2, 1 << 30);
    actor
        .dag
        .node_mut("d-fod")
        .unwrap()
        .sched
        .resource_floor
        .mem_bytes = 8 << 30; // legacy row: above every live class ceiling
    let snap = metrics::with_local_recorder(&metrics, || {
        actor.compute_spawn_intents(&Default::default())
    });
    let i = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d-fod")
        .expect("emitted");
    assert!(
        i.hw_class_names.is_empty(),
        "unhostable demand emits typed-EMPTY (no phantom cell): {:?}",
        i.hw_class_names
    );
    // The disclosure: exit="unhostable" counted exactly once
    // (debounced per (tenant, pname, kind) edge). ONE snapshot
    // (hazard ppppp).
    let snap_m = snapshotter.snapshot().into_vec();
    let unhostable: u64 = snap_m
        .iter()
        .filter_map(|(k, _, _, v)| {
            let (kind, key) = k.clone().into_parts();
            (kind == metrics_util::MetricKind::Counter
                && key.name() == "rio_scheduler_sla_hw_ladder_exhausted_total"
                && key
                    .labels()
                    .any(|l| l.key() == "exit" && l.value() == "unhostable"))
            .then_some(match v {
                metrics_util::debugging::DebugValue::Counter(c) => *c,
                _ => 0,
            })
        })
        .sum();
    assert_eq!(
        unhostable, 1,
        "the unhostable emission is DISCLOSED (typed + loud) — pre-fix: zero"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **The live_051(b) clamp green twin (W7-V, clamp half)** —
/// *demand infeasible at every class with NO feature constraint
/// clamps-with-disclosure into the largest mintable class*: a fitted
/// featureless drv whose solve is BestEffort under shrunk per-class
/// ceilings emits NAMED cells at the clamped size (hostable by
/// composition — the global is honest post-live_051(a)) plus the
/// `exit="stale_resolved"` disclosure. Kill-isolation vs `HwAgnostic`:
/// the quiet arm (no evidence) emits no cells and no disclosure —
/// `hw_agnostic_emission_stays_quiet` is the paired pin.
#[tokio::test]
async fn oversize_unconstrained_demand_clamps_with_disclosure() {
    let metrics = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = metrics.snapshotter();
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-clamp", Some("test-pkg"), "x86_64-linux", false);
    // Every class's catalog mem ceiling drops below the fitted demand
    // (p90 6 GiB → 2 GiB ceilings): infeasible at every class, no
    // feature constraint, floor zero.
    shrink_catalog(&actor, 32, 2 << 30);
    let snap = metrics::with_local_recorder(&metrics, || {
        actor.compute_spawn_intents(&Default::default())
    });
    let i = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d-clamp")
        .expect("emitted");
    assert!(
        !i.hw_class_names.is_empty(),
        "clamp-with-disclosure emits NAMED cells (never empty for evidenced demand)"
    );
    assert!(
        i.mem_bytes <= 2 << 30,
        "demand clamped into the live class ceiling: mem={}",
        i.mem_bytes
    );
    let snap_m = snapshotter.snapshot().into_vec();
    let resolved: u64 = snap_m
        .iter()
        .filter_map(|(k, _, _, v)| {
            let (kind, key) = k.clone().into_parts();
            (kind == metrics_util::MetricKind::Counter
                && key.name() == "rio_scheduler_sla_hw_ladder_exhausted_total"
                && key
                    .labels()
                    .any(|l| l.key() == "exit" && l.value() == "stale_resolved"))
            .then_some(match v {
                metrics_util::debugging::DebugValue::Counter(c) => *c,
                _ => 0,
            })
        })
        .sum();
    assert_eq!(resolved, 1, "the clamp is DISCLOSED exactly once per edge");
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **R24 + W7-W** — *a floor persisted above the live global is
/// consumed CLAMPED on the first post-boot read, and the hydrate seam
/// grounds it on entry*: actor-1 ("the old boot", big ceilings)
/// persists an 8 GiB floor row; actor-2 ("the new boot", 2 GiB live
/// global via the test config) re-merges the same drv — the I-208
/// hydrate clamps the row at entry (in-mem floor ≤ live global) and
/// the production emission stays hostable (cells derivable; the
/// bypass mem can no longer exceed the live global — the (b) channel
/// is dead). Kill-isolation: a floor BELOW the live global hydrates
/// byte-identical (`merge_hydrates_resource_floor_from_db` is the
/// paired pin at the clamped boundary).
///
/// Pre-fix red (left, reverse-strawman transcript in the commit
/// body): hydrated floor == 8 GiB (the 383-era value re-imported
/// across the boot); the bypass max re-raises mem past the live
/// global and the emission falls into the silent empty-cells channel.
///
/// The seeded row is the persisted-legacy population —
/// r13-allow(frozen-legacy), disclosed: rows written by a prior boot
/// under bigger ceilings ARE legacy data; the GREATEST-ratchet writer
/// preserves them by design (the law grounds them at consumption).
#[tokio::test]
async fn floor_above_global_reclamps_at_boot() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    // "Old boot": create the row via a production merge, then the
    // prior run's OOM ladder leaves floor_mem=8GiB persisted (legacy
    // row form, frozen-legacy lane).
    let mut actor1 = bare_actor_hw_builders_only(db.pool.clone());
    let node = make_node("d-floor");
    let req = |nodes, build_id| crate::actor::command::MergeDagRequest {
        build_id,
        tenant_id: None,
        priority_class: crate::state::PriorityClass::Scheduled,
        nodes,
        edges: vec![],
        options: crate::state::BuildOptions::default(),
        keep_going: false,
        traceparent: String::new(),
        jti: None,
        jwt_token: None,
    };
    actor1
        .handle_merge_dag(req(vec![node.clone()], Uuid::new_v4()))
        .await
        .expect("old-boot merge");
    sqlx::query("UPDATE derivations SET floor_mem_bytes = $2 WHERE drv_hash = $1")
        .bind(&node.drv_hash)
        .bind(8i64 << 30)
        .execute(&db.pool)
        .await?;

    // "New boot": fresh actor, SMALL live global (test_default 2 GiB).
    let mut actor2 = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: crate::sla::config::SlaConfig::test_default(),
            ..Default::default()
        },
    );
    let live_max_mem = actor2.sla_ceilings.max_mem;
    assert!(
        live_max_mem < 8 << 30,
        "precondition: the live global shrank"
    );
    actor2
        .handle_merge_dag(req(vec![node], Uuid::new_v4()))
        .await
        .expect("new-boot merge");
    let hydrated = actor2
        .dag
        .node("d-floor")
        .expect("merged")
        .sched
        .resource_floor
        .mem_bytes;
    assert_eq!(
        hydrated,
        live_max_mem - rio_common::footprint::WORKER_MEM_OVERHEAD_BYTES,
        "the hydrate seam grounds the stale row at the LIVE global's \
         SOLVE-domain cap (global − pad, merged_bug_016 — a raw-global \
         floor renders an unhostable container; pre-fix: 8 GiB \
         re-imported raw)"
    );
    Ok(())
}

/// One `NO_HOSTING_CLASS` wire verdict.
fn no_host_verdict(id: &str, detail: &str) -> rio_proto::types::IntentVerdict {
    rio_proto::types::IntentVerdict {
        intent_id: id.into(),
        reason: rio_proto::types::IntentVerdictReason::NoHostingClass as i32,
        detail: detail.into(),
    }
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **R25 + W7-X** — *N consecutive no-hosting-class verdicts drive the
/// drv out of Ready via poison with the controller's detail*
/// (operation-count: exactly `NO_HOST_VERDICTS_TO_POISON` applied
/// acks; zero wall-clock). Pre-fix red (left): the ack carried no
/// verdict plane at all — nothing counted, the drv re-emitted Ready
/// forever, and the measured live loop ended only by operator
/// cancellation.
///
/// Kill-isolation pins (each its own conjunct):
/// - N−1 verdicts then a SPAWNED echo for the drv → the track resets
///   (the heal path) — N−1 further verdicts still do not poison;
/// - a hosting-class config change (the census reset key) restarts
///   the count at 1; detail jitter does NOT (merged_bug_043(1));
/// - duplicate entries for one drv within ONE ack count once (the
///   in-request dedup half of the redelivery law; the cross-request
///   half is the PRODUCER's no-buffer law — `CoverResult::rejected`
///   is re-minted per pass, never redelivered, and `admin_call` is
///   single-shot — cited, controller-side).
#[tokio::test]
async fn n_no_host_verdicts_poison_the_drv_with_the_verdict_message() {
    use crate::actor::snapshot::NO_HOST_VERDICTS_TO_POISON as N;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-loop", Some("test-pkg"), "x86_64-linux", false);
    let detail = "no [sla.hw_classes] entry hosts system=x86_64-linux cores=383 \
                  mem_bytes=412316860416 required_features=[]; configured classes: \
                  [intel-6, intel-7, intel-8]";

    // N−1 applied acks: counted, never poisoned.
    for k in 1..N {
        let p = actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-loop", detail)],
            )
            .expect("applied under leadership");
        assert!(p.is_empty(), "no poison at {k} < {N}");
    }
    assert_eq!(
        actor.dag.node("d-loop").unwrap().status(),
        DerivationStatus::Ready,
        "still Ready inside the budget"
    );

    // The Nth: the budget crosses — the typed poison row carries the
    // controller's detail; applying it drives Ready → Poisoned.
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-loop", detail)],
        )
        .expect("applied under leadership");
    assert_eq!(p.len(), 1, "budget crossing at exactly N = {N}");
    assert_eq!(
        p[0].detail, detail,
        "the operator message IS the verdict detail"
    );
    actor.apply_no_host_poisons(p).await;
    assert_eq!(
        actor.dag.node("d-loop").unwrap().status(),
        DerivationStatus::Poisoned,
        "the drv leaves Ready via the EXISTING poison machinery — \
         emission stops (Ready-only spawn-intent filter)"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// W7-X kill-isolation: the three reset/dedup conjuncts of the
/// verdict-budget law, each driven through the production ack-apply
/// plane (see `n_no_host_verdicts_poison_the_drv_with_the_verdict_
/// message` for the poison half).
#[tokio::test]
async fn verdict_budget_resets_on_spawn_and_census_change_and_dedups_in_request() {
    use crate::actor::snapshot::NO_HOST_VERDICTS_TO_POISON as N;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-heal", Some("test-pkg"), "x86_64-linux", false);

    // (a) spawn reset: N−1 verdicts, then a spawned echo, then N−1
    // more — never poisons (the count restarted).
    for _ in 1..N {
        actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-heal", "A")],
            )
            .expect("applied");
    }
    let spawned = rio_proto::types::SpawnIntent {
        intent_id: "d-heal".into(),
        ..Default::default()
    };
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&spawned),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied");
    for k in 1..N {
        let p = actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-heal", "A")],
            )
            .expect("applied");
        assert!(
            p.is_empty(),
            "spawn reset the track: no poison at {k} of the second run"
        );
    }

    // (b) detail JITTER does NOT restart (merged_bug_043(1)): the
    // count after the second A-run sits at N−1; one byte-different
    // B-verdict with an UNCHANGED hosting config is consecutive
    // evidence — the budget crosses HERE (pre-fix the byte-diff
    // restarted at 1 and the budget was structurally defeated for
    // the refit/price-churn population).
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-heal", "B")],
        )
        .expect("applied");
    assert_eq!(
        p.len(),
        1,
        "detail jitter is display-only — the budget crosses at N"
    );

    // (b2) a hosting-class CONFIG change restarts at 1 (the law's
    // heal-window axis, now keyed on the scheduler's own config
    // census): drive a fresh track to N−1, mutate the config, then
    // one more verdict — the count restarted, no poison.
    for k in 1..N {
        let p = actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-heal", "C")],
            )
            .expect("applied");
        assert!(p.is_empty(), "C-run inside the budget at {k}");
    }
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-6")
        .unwrap()
        .max_mem = Some(64 << 30);
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-heal", "C")],
        )
        .expect("applied");
    assert!(
        p.is_empty(),
        "a hosting-class config change re-opens the heal window \
         (census restart at 1)"
    );
    // Re-build the track to N−1 under the NEW census for the dedup
    // conjunct below (the restart left it at 1).
    for k in 2..N {
        let p = actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-heal", "C")],
            )
            .expect("applied");
        assert!(p.is_empty(), "post-census C-run inside the budget at {k}");
    }

    // (c) in-request dedup: ONE ack carrying the drv twice counts
    // once — the Nth B entry rides this ack (count N−1 → N), and the
    // duplicate does NOT overshoot (poison fires exactly here, len 1).
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[
                no_host_verdict("d-heal", "B"),
                no_host_verdict("d-heal", "B"),
            ],
        )
        .expect("applied");
    assert_eq!(p.len(), 1, "duplicate entries within one ack count once");
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// Counter-lifecycle law table (R15 product census over the step
/// alphabet, merged_bug_043 form): `step_no_host_counter` walked over
/// (prev-track × census × pass) cells with a HAND-WRITTEN oracle
/// (never the impl's own expression). The detail is DISPLAY-ONLY —
/// rows pin that a byte-different detail with identical census/pass
/// adjacency CONTINUES the count (the jitter law), that a census
/// change restarts (the config-reload heal window), and that a pass
/// gap restarts (the typed no-verdict non-event — a frozen track can
/// never claim false consecutiveness). The apply-plane composition
/// rows (fresh-spawn-edge reset, in-request dedup, Ready-only poison)
/// are pinned by the production-plane tests above.
#[test]
fn no_host_counter_step_law_table() {
    use crate::actor::snapshot::{NoHostTrack, step_no_host_counter as step};
    let track = |count: u32, census: u64, detail: &str, pass: u64| NoHostTrack {
        count,
        census,
        detail: detail.to_owned(),
        pass,
    };
    // (prev, census, detail, pass) → (count, detail) — hand oracle.
    #[allow(clippy::type_complexity)] // law-table rows read better flat
    let rows: &[(Option<NoHostTrack>, u64, &str, u64, (u32, &str))] = &[
        // first evidence
        (None, 9, "A", 1, (1, "A")),
        // consecutive identical (adjacent pass, same census)
        (Some(track(1, 9, "A", 1)), 9, "A", 2, (2, "A")),
        (Some(track(7, 9, "A", 4)), 9, "A", 5, (8, "A")),
        // DETAIL JITTER does NOT restart (merged_bug_043(1)): the
        // detail is display-only — count continues, detail updates.
        (Some(track(7, 9, "A", 4)), 9, "B", 5, (8, "B")),
        // census change restarts (the config-reload heal window).
        (Some(track(7, 9, "A", 4)), 10, "A", 5, (1, "A")),
        // PASS GAP restarts (merged_bug_043(3)): the streak broke on
        // an evidence-carrying pass that skipped this drv.
        (Some(track(29, 9, "A", 4)), 9, "A", 6, (1, "A")),
        // same-pass re-step is idempotent (defensive; unreachable
        // through the in-request dedup).
        (Some(track(7, 9, "A", 4)), 9, "A", 4, (7, "A")),
        // saturates, no wrap
        (Some(track(u32::MAX, 9, "A", 4)), 9, "A", 5, (u32::MAX, "A")),
    ];
    for (prev, census, detail, pass, want) in rows {
        let got = step(prev.as_ref(), *census, detail, *pass);
        assert_eq!(
            (got.count, got.detail.as_str()),
            (want.0, want.1),
            "row ({prev:?}, census={census}, {detail}, pass={pass})"
        );
        assert_eq!(
            (got.census, got.pass),
            (*census, *pass),
            "the track stamps the step's census and pass"
        );
    }
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// An out-of-alphabet verdict reason refuses the WHOLE request
/// (validate-then-commit: no plane applied) — the closed-alphabet
/// posture of the wire fold (rustc-exhaustive at validate; this pins
/// the runtime half for unknown discriminants and UNSPECIFIED).
#[tokio::test]
async fn out_of_alphabet_verdict_reason_refuses_the_request() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-bad", Some("test-pkg"), "x86_64-linux", false);
    for reason in [0i32, 999] {
        let bad = rio_proto::types::IntentVerdict {
            intent_id: "d-bad".into(),
            reason,
            detail: "x".into(),
        };
        let r = actor.handle_ack_spawned_intents(
            &[],
            &["intel-6:spot".into()],
            &[],
            &[],
            &[],
            None,
            std::slice::from_ref(&bad),
        );
        assert!(r.is_err(), "reason={reason} refused");
        assert!(
            !actor
                .ice
                .is_masked(&("intel-6".into(), crate::sla::config::CapacityType::Spot)),
            "an erring ack applied NOTHING (the mark plane did not land)"
        );
    }
}

/// One `OVER_CAP` wire verdict (the advisory letter).
fn over_cap_verdict(id: &str) -> rio_proto::types::IntentVerdict {
    rio_proto::types::IntentVerdict {
        intent_id: id.into(),
        reason: rio_proto::types::IntentVerdictReason::OverCap as i32,
        detail: "pod footprint (64, 1Ti, 2Ti) exceeds cell intel-6:spot per-class cap".into(),
    }
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-AX (scheduler observe face) + W9-AX′ (the negative face — the
/// mis-poison hazard's own population):** the over-cap letter is
/// ADVISORY — acknowledged WITHOUT poison. Driven at the
/// `NO_HOST_VERDICTS_TO_POISON` threshold population: ≥ N consecutive
/// over-cap dispositions for ONE Ready drv leave its no-host track
/// UN-STEPPED and the drv UN-POISONED (over-cap is transient/
/// self-healing ≤300s skew; the poison budget is 30 × ~10s ≈ the SAME
/// window — conflation would poison exactly the population that heals
/// itself). The un-stepped clause is probed BOTH directly (the track
/// map carries no entry) and structurally (a follow-on N−1
/// no-hosting-class run still does not poison — if over-cap had fed
/// the shared budget, 30 + N−1 ≥ N would have crossed it).
#[tokio::test]
async fn over_cap_verdicts_acknowledge_without_poison() {
    use crate::actor::snapshot::NO_HOST_VERDICTS_TO_POISON as N;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-overcap", Some("test-pkg"), "x86_64-linux", false);

    // W9-AX observe face: the ack DECODES and APPLIES (reason distinct
    // from NO_HOSTING_CLASS — not refused, not conflated); W9-AX′: at
    // and beyond the poison threshold, zero poisons and the track
    // never steps.
    for k in 1..=(N + 5) {
        let p = actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[over_cap_verdict("d-overcap")],
            )
            .expect("an over-cap verdict is a VALID ack plane entry (observed, not refused)");
        assert!(
            p.is_empty(),
            "no poison at {k} consecutive over-cap dispositions (advisory lane)"
        );
        assert!(
            !actor
                .supply_reval
                .no_host_verdicts
                .contains_key(&crate::state::DrvHash::from("d-overcap")),
            "the no-host poison track is UN-STEPPED at {k} over-cap dispositions"
        );
    }
    assert_eq!(
        actor.dag.node("d-overcap").unwrap().status(),
        DerivationStatus::Ready,
        "the drv stays Ready — re-mint/wait is the advisory consumer answer"
    );

    // Structural un-steppedness: a follow-on N−1 no-hosting-class run
    // does NOT poison (the over-cap dispositions contributed nothing
    // to the terminal budget).
    for _ in 1..N {
        let p = actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict(
                    "d-overcap",
                    "no [sla.hw_classes] entry hosts it",
                )],
            )
            .expect("applied");
        assert!(p.is_empty(), "fresh budget: over-cap never fed it");
    }
    assert_eq!(
        actor.dag.node("d-overcap").unwrap().status(),
        DerivationStatus::Ready,
        "still Ready at N-1 no-host verdicts after the over-cap run"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **The emission-arm product census (R15)** — cells from the
/// `CellEmission` alphabet over (feat ∅/non-∅ × {hostable,
/// stale-hostable, infeasible-everywhere, unhostable} × pin rows),
/// driven through the production classifier with per-row expected
/// VARIANTS (rustc keeps the alphabet closed at the fold; this test
/// keeps every arm REACHED — premise-reachability per row). The
/// live_050(e) journal row (stale-hostable) and the live_051(b)
/// verdict row (infeasible-everywhere) are DISTINCT input cells that
/// share the `StaleSolve` family — pinned separately below.
#[tokio::test]
async fn cell_emission_arm_product_census() {
    use crate::actor::snapshot::CellEmission as E;
    use crate::sla::config::CapacityType;
    use crate::sla::solve::InfeasibleReason;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.test_inject_ready("d-plain", Some("test-pkg"), "x86_64-linux", false);
    actor.test_inject_ready("d-fod", Some("fod-pkg"), "x86_64-linux", true);
    actor.test_inject_ready_with_features("d-exotic", None, "x86_64-linux", &["no-such"]);
    actor.test_inject_ready("d-unmappable", None, "unmappable-system", false);
    // Floored node: legacy 8 GiB row (frozen-legacy lane — see R23).
    actor.test_inject_ready("d-floored", Some("fod2"), "x86_64-linux", true);
    actor
        .dag
        .node_mut("d-floored")
        .unwrap()
        .sched
        .resource_floor
        .mem_bytes = 8 << 30;

    let cost_big = actor.cost_table.read().clone();
    // Shrunk-catalog cost table (the fixture lane) for the stale rows.
    shrink_catalog(&actor, 2, 1 << 30);
    let cost_small = actor.cost_table.read().clone();

    let node = |h: &str| actor.dag.node(h).unwrap();
    let feat_fetcher = vec![rio_common::k8s::FETCHER_FEATURE.to_string()];
    let no_feat: Vec<String> = vec![];

    // Row 1 (∅ feat, hostable demand, no evidence) → HwAgnostic.
    let e = actor.classify_cell_emission(
        node("d-plain"),
        None,
        4,
        1 << 30,
        &cost_big,
        "t",
        &no_feat,
        None,
        false,
    );
    assert!(matches!(e, E::HwAgnostic), "row 1: {e:?}");
    // Row 1b (∅ feat, arch-unmappable) → HwAgnostic (the r35 B1 guard).
    let e = actor.classify_cell_emission(
        node("d-unmappable"),
        None,
        4,
        1 << 30,
        &cost_big,
        "t",
        &no_feat,
        Some(InfeasibleReason::MemCeiling),
        false,
    );
    assert!(matches!(e, E::HwAgnostic), "row 1b: {e:?}");
    // Row 2 (∅ feat, stale-hostable — the live_050(e) journal cell):
    // demand above every shrunk class, candidates exist → StaleSolve.
    let e = actor.classify_cell_emission(
        node("d-plain"),
        None,
        48,
        6 << 30,
        &cost_small,
        "t",
        &no_feat,
        Some(InfeasibleReason::MemCeiling),
        false,
    );
    let E::StaleSolve {
        resolved, cells, ..
    } = &e
    else {
        panic!("row 2 (journal cell): {e:?}")
    };
    assert!(resolved.0 <= 2 && resolved.1 <= 1 << 30 && !cells.is_empty());
    // Row 3 (∅ feat, infeasible-everywhere — the live_051(b) verdict
    // cell): same variant family, distinct input (solve-time
    // infeasibility at the BIG table whose classes the demand still
    // exceeds via the config ceiling axis is not constructible here;
    // the in-tree (b) feed is BestEffort → the shrunk-ceiling shape) —
    // pinned at the production pass by
    // `oversize_unconstrained_demand_clamps_with_disclosure`.
    let e = actor.classify_cell_emission(
        node("d-plain"),
        None,
        383,
        412 << 30,
        &cost_small,
        "t",
        &no_feat,
        Some(InfeasibleReason::MemCeiling),
        false,
    );
    assert!(
        matches!(e, E::StaleSolve { .. }),
        "row 3 (verdict cell): {e:?}"
    );
    // Row 4 (∅ feat, forced oversize) → Unhostable (a pin is never
    // clamped — the bug_019 law).
    let e = actor.classify_cell_emission(
        node("d-plain"),
        None,
        383,
        412 << 30,
        &cost_small,
        "t",
        &no_feat,
        Some(InfeasibleReason::MemCeiling),
        true,
    );
    assert!(
        matches!(
            e,
            E::Unhostable {
                best_class: Some(_),
                ..
            }
        ),
        "row 4: {e:?}"
    );
    // Row 5 (non-∅ feat, hostable) → Cells (the §13d cold-start arm).
    let e = actor.classify_cell_emission(
        node("d-fod"),
        None,
        2,
        1 << 29,
        &cost_big,
        "t",
        &feat_fetcher,
        None,
        false,
    );
    assert!(
        matches!(e, E::Cells(ref c) if !c.is_empty()),
        "row 5: {e:?}"
    );
    // Row 6 (non-∅ feat, oversize with candidates) → StaleSolve into
    // the feature class.
    let e = actor.classify_cell_emission(
        node("d-fod"),
        None,
        48,
        6 << 30,
        &cost_small,
        "t",
        &feat_fetcher,
        Some(InfeasibleReason::MemCeiling),
        false,
    );
    let E::StaleSolve { class, cells, .. } = &e else {
        panic!("row 6: {e:?}")
    };
    assert!(
        class.starts_with("fetcher-"),
        "feature demand stays in its class: {class}"
    );
    assert!(cells.iter().all(|(h, _)| h.starts_with("fetcher-")));
    // Row 7 (non-∅ feat, floor above the best feature ceiling) →
    // Unhostable WITH the best class named (the WHY fields).
    let e = actor.classify_cell_emission(
        node("d-floored"),
        None,
        2,
        1 << 29,
        &cost_small,
        "t",
        &feat_fetcher,
        None,
        false,
    );
    let E::Unhostable {
        demand: _,
        best_class: Some((bh, _)),
    } = &e
    else {
        panic!("row 7: {e:?}")
    };
    assert!(bh.starts_with("fetcher-"));
    // Row 8 (unroutable feature — zero candidates) → Unhostable{None}.
    let e = actor.classify_cell_emission(
        node("d-exotic"),
        None,
        2,
        1 << 29,
        &cost_big,
        "t",
        &["no-such".to_string()],
        None,
        false,
    );
    assert!(
        matches!(
            e,
            E::Unhostable {
                best_class: None,
                ..
            }
        ),
        "row 8: {e:?}"
    );
    // Row 9 (pin hosted + fits) → Cells via the bypass Some-arm.
    let e = actor.classify_cell_emission(
        node("d-plain"),
        Some(CapacityType::Spot),
        4,
        1 << 30,
        &cost_big,
        "t",
        &no_feat,
        None,
        false,
    );
    assert!(matches!(e, E::Cells(ref c) if c.len() == 1), "row 9: {e:?}");
    // Row 10 (pin + oversize, not forced) → StaleSolve HONORING the
    // pin (one cell at the pinned capacity).
    let e = actor.classify_cell_emission(
        node("d-plain"),
        Some(CapacityType::Spot),
        48,
        6 << 30,
        &cost_small,
        "t",
        &no_feat,
        Some(InfeasibleReason::MemCeiling),
        false,
    );
    let E::StaleSolve { cells, .. } = &e else {
        panic!("row 10: {e:?}")
    };
    assert_eq!(cells.len(), 1);
    assert!(
        matches!(cells[0].1, CapacityType::Spot),
        "the pin survives the re-solve"
    );
    // Row 11 (∅ feat × hostable × TIME evidence) — the product cell
    // the pre-round-9 row-set omitted (merged_bug_057's census LINT
    // GAP): time-only evidence (SerialFloor) leaves fitting
    // featureless demand on the designed agnostic lane; pre-fix the
    // flattened bool pinned it to the mem-largest class with a false
    // stale disclosure.
    let e = actor.classify_cell_emission(
        node("d-plain"),
        None,
        4,
        1 << 30,
        &cost_big,
        "t",
        &no_feat,
        Some(InfeasibleReason::SerialFloor),
        false,
    );
    assert!(matches!(e, E::HwAgnostic), "row 11 (time evidence): {e:?}");
    // Row 12 (∅ feat × hostable × SIZE evidence) — the sibling cell:
    // size evidence excludes the agnostic lane, but demand FITTING
    // the best hosting class has nothing stale to disclose (the
    // StaleSolve premise is resolved != solved) — it routes as plain
    // Cells at that class, no WARN, no exit increment.
    let e = actor.classify_cell_emission(
        node("d-plain"),
        None,
        4,
        1 << 30,
        &cost_big,
        "t",
        &no_feat,
        Some(InfeasibleReason::MemCeiling),
        false,
    );
    assert!(
        matches!(e, E::Cells(ref c) if !c.is_empty()),
        "row 12 (size evidence, fits): {e:?}"
    );
    // Row 13 — **W10-AA (bug_128, the prescription-adopted exemplar)**:
    // ∅ feat × hostable demand × DISK evidence (DiskCeiling).
    // Disk has NO per-class ceiling (config.rs: global-only via
    // SlaCeilings BY DESIGN) and the request is already clamped at
    // the solve chokepoint, so re-routing featureless disk-capped
    // demand into the (mem,cores)-largest class adds ZERO hostability
    // while silently concentrating every disk_p90>maxDisk build onto
    // the most expensive class — the merged_bug_057 concentration
    // failure re-opened on the disk axis, durably, every tick. The
    // agnostic lane MUST stay open: the partition predicate derives
    // from "does any per-class ceiling exist on this axis", never
    // from variant naming. Pre-fix red: DiskCeiling sat in the size
    // arm by 'Ceiling' name-analogy → the gate denied the agnostic
    // lane → the stale walk found the demand fits → plain Cells
    // pinned to the largest class with no StaleSolve disclosure
    // (both faces asserted).
    let e = actor.classify_cell_emission(
        node("d-plain"),
        None,
        4,
        1 << 30,
        &cost_big,
        "t",
        &no_feat,
        Some(InfeasibleReason::DiskCeiling),
        false,
    );
    assert!(
        !matches!(e, E::Cells(_)),
        "row 13 pre-fix shape: featureless disk-capped demand was \
         PINNED to the largest class as plain Cells (silent \
         concentration, zero disclosure): {e:?}"
    );
    assert!(
        matches!(e, E::HwAgnostic),
        "row 13 (disk evidence): the agnostic lane stays open — the \
         chokepoint clamp is the disk law; got {e:?}"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **[GEN-SET] call-site censuses (R15)** — committed scanner output
/// over the EMBEDDED sources (include_str! — the nix-gate-safe form;
/// bare runtime walks fail under the sandbox, the bughunt-6
/// observation). Three needles:
///
/// 1. `reference_hw_class_for_system(` call sites — the emission
///    seam's resolver: every production caller is one of the typed
///    lanes (bypass Some-arm, bypass cold-start arm, the classifier's
///    pin-mismatch probe) or the config-side definition;
/// 2. `SolveFullResult::BestEffort` — the memo-None feed into the
///    emission fold (live_051(b)): constructor + the two consumer
///    folds;
/// 3. `resource_floor` reads in the actor solve plane — every
///    read-consume site takes the CLAMPED projection (the (d) law);
///    the deadline-axis read is cap-const-bounded (Ceilings has no
///    time dimension); hydrate/bump/debug rows classified.
///
/// An unlisted hit FAILS the census naming the file (closure
/// tomorrow, not completeness today).
#[test]
fn stale_solve_revalidation_call_site_censuses() {
    let snapshot_src = include_str!("../snapshot.rs");
    let solve_src = include_str!("../../sla/solve.rs");
    let config_src = include_str!("../../sla/config.rs");
    let floor_src = include_str!("../floor.rs");
    let merge_src = include_str!("../merge.rs");

    let prod = |src: &str| -> String {
        src.split("#[cfg(test)]\nmod tests")
            .next()
            .unwrap_or(src)
            .to_string()
    };
    let count = |src: &str, needle: &str| prod(src).matches(needle).count();

    // (1) the resolver: 4 call sites in snapshot.rs (bypass Some-arm,
    // bypass cold-start arm, classifier pin probe) + the fn def +
    // 1 doc/comment mention in config.rs; solve.rs has none.
    assert_eq!(
        count(snapshot_src, ".reference_hw_class_for_system("),
        5,
        "snapshot.rs resolver call sites: bypass Some-arm (pin-aware \
         since merged_bug_067), the bypass cap-is-binding probe (the \
         re-keyed r31 A3 warn lane), bypass cold-start arm, classifier \
         cap-blind PinGated probe, the PinGated premise debug_assert — \
         a NEW call site joins this census with its CellEmission lane \
         named"
    );
    assert_eq!(
        count(solve_src, ".reference_hw_class_for_system("),
        0,
        "solve.rs never CALLS the resolver (one doc-comment mention at the \
         §Canonicalize note is prose, not a call site — the emission \
         fold owns resolution)"
    );
    assert!(
        prod(config_src).contains("pub fn reference_hw_class_for_system("),
        "the single definition lives in sla/config.rs"
    );

    // (2) the memo-None feed (live_051(b)): the enum def + `Feasible`
    // sibling live in solve.rs; snapshot.rs consumes it in EXACTLY
    // one fold (the memo arm) — the silent second consumer would be a
    // new feed into the emission chokepoint.
    assert_eq!(
        count(snapshot_src, "SolveFullResult::BestEffort"),
        1,
        "the ONE memo-None feed (snapshot.rs solve fold) — a second \
         consumer must route through the CellEmission fold"
    );

    // (3) floor reads in the solve/emission plane: every mem/disk
    // consumption is the clamped projection.
    assert_eq!(
        count(snapshot_src, "ClampedFloor::of("),
        5,
        "snapshot.rs clamped-projection sites: solve-arm pre-clamp, \
         post-solve chokepoint, bypass seam (the load-bearing one), \
         classifier floor guard, memo-arm survival check \
         (merged_bug_002 — predicts the chokepoint's overlay so the \
         memo emission re-classifies when the floor kills its cells)"
    );
    // Raw floor reads surviving in snapshot.rs: the chokepoint's
    // deadline read (`floor.deadline_secs` — cap-const axis) + the
    // binding that feeds it + the three projection constructor args.
    assert_eq!(
        count(snapshot_src, "state.sched.resource_floor"),
        5,
        "raw floor mentions in snapshot.rs = 4 projection-constructor \
         args (incl. the memo-arm survival check's) + the chokepoint \
         binding (whose mem/disk reads go through `fclamped`, deadline \
         through the cap const)"
    );
    assert_eq!(
        count(merge_src, "clamp_floor_to_live"),
        1,
        "the hydrate seam applies the in-place clamp law exactly once"
    );
    assert!(
        prod(floor_src).contains("pub(super) fn clamp_floor_to_live"),
        "the law lives in actor/floor.rs"
    );

    // (4) W9-R (merged_bug_002, the 2026-06 revision): the
    // producer-side totality claim made machine-shaped. The STRIKE-7
    // chokepoint doc asserts "a new producer-hole would require a
    // SpawnIntent construction site that bypasses `solve_intent_for`
    // entirely" — this needle IS that census: the sole production
    // construction site is the shared `to_proto` constructor inside
    // `compute_spawn_intents` (whose `(cores, mem, cells)` inputs come
    // from `solve_intent_for`'s classified fold). Count = 2 because
    // the closure's return-type annotation and the struct literal
    // share the needle. A bypassing constructor anywhere in the
    // emission plane fails this census naming the file.
    let dispatch_src = include_str!("../dispatch.rs");
    let mod_src = include_str!("../mod.rs");
    let command_src = include_str!("../command.rs");
    let admin_mod_src = include_str!("../../admin/mod.rs");
    let admin_si_src = include_str!("../../admin/spawn_intents.rs");
    assert_eq!(
        count(snapshot_src, "SpawnIntent {"),
        2,
        "snapshot.rs SpawnIntent construction = the shared `to_proto` \
         constructor only (type annotation + struct literal); a new \
         construction site joins the classified fold or fails here"
    );
    for (name, other) in [
        ("dispatch.rs", dispatch_src),
        ("mod.rs", mod_src),
        ("command.rs", command_src),
        ("admin/mod.rs", admin_mod_src),
        ("admin/spawn_intents.rs", admin_si_src),
    ] {
        assert_eq!(
            count(other, "SpawnIntent {"),
            0,
            "{name}: zero SpawnIntent construction sites — the serving \
             plane re-serves snapshot intents, it never mints them"
        );
    }

    // (5) W9-AA (merged_bug_037): the wire-empty emitter census — the
    // scoped totality claim ("within the scheduler the alphabet is
    // total; `Unhostable` serializes typed-empty BY DESIGN") is bound
    // to a machine count of the fold's empty-cells returns: exactly
    // THREE letters emit wire-empty (`HwAgnostic` — the designed
    // quiet edge; `PinGated` — the pin never silently rewritten;
    // `Unhostable` — the no_hosting_class feeder). A fourth empty
    // return is a new silent-wire population that must either join a
    // typed letter or update the scoped claim (doc + spec clause +
    // this census together).
    assert_eq!(
        count(snapshot_src, "(cores, mem, Vec::new())"),
        3,
        "fold_cell_emission wire-empty returns: HwAgnostic, PinGated, \
         Unhostable — the scoped-totality claim's census \
         (merged_bug_037)"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-P (merged_bug_002)** — *a memoized solve + post-memo floor
/// bump into the (class-ceiling, global] band yields a CLASSIFIED
/// emission, never silent `hw_class_names=[]`* — driven through the
/// MEMO path, the population the round-8 battery never drove (the
/// law quantifies over EMISSIONS, not over no-memo emissions).
///
/// The floor is NOT in `inputs_gen` BY DESIGN (a floor bump must not
/// thrash the memo), so the second solve memo-HITS; pre-fix the
/// shared chokepoint then raised mem to the clamped floor, stripped
/// every per-class cell at `retain_hosting_cells` (size axis), and
/// emitted empty cells with only the misattributed
/// producer-regression warn. Post-fix the memo arm re-classifies
/// after the overlay: floor above EVERY class ceiling => Unhostable
/// (typed-empty BY DESIGN + the loud disclosure + the exit counter).
#[tokio::test]
async fn memo_arm_floor_bump_is_classified_not_silent_empty() {
    use crate::sla::metrics::counter_map_by;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Class ceilings BELOW the global so the (class-ceiling, global]
    // band exists: intel-* at 128 GiB, global stays 256 GiB.
    for h in ["intel-6", "intel-7", "intel-8"] {
        actor.sla_config.hw_classes.get_mut(h).unwrap().max_mem = Some(128 << 30);
    }
    actor.test_inject_ready("d-memo-floor", Some("test-pkg"), "x86_64-linux", false);

    // First solve: the memo path (harness seeds the test-pkg fit and
    // the hw table) — hw-routed, non-empty cells.
    let (hw, cost, g) = actor.solve_inputs();
    let state = actor.dag.node("d-memo-floor").unwrap();
    let i0 = actor.solve_intent_for(state, &hw, &cost, g);
    assert!(
        !i0.hw_class_names.is_empty(),
        "precondition: the first solve memoizes and routes: {i0:?}"
    );

    // Post-memo floor bump into the band: 160 GiB sits above every
    // class ceiling (128 GiB) and below the global (256 GiB).
    actor
        .dag
        .node_mut("d-memo-floor")
        .unwrap()
        .sched
        .resource_floor
        .mem_bytes = 160 << 30;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let state = actor.dag.node("d-memo-floor").unwrap();
    let i1 = {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.solve_intent_for(state, &hw, &cost, g)
    };
    // ONE snapshot read (it drains).
    let exits = counter_map_by(
        &snap,
        "rio_scheduler_sla_hw_ladder_exhausted_total",
        Some("exit"),
    );
    assert!(
        i1.hw_class_names.is_empty(),
        "no class hosts a 160 GiB floor — Unhostable serializes \
         typed-empty by design: {i1:?}"
    );
    assert_eq!(
        exits.get("unhostable").copied().unwrap_or(0),
        1,
        "the memo-arm emission must be CLASSIFIED after the floor \
         overlay (the unhostable disclosure) — silent hw_class_names=[] \
         with only the chokepoint strip warn was merged_bug_002; \
         exits: {exits:?}"
    );
    assert!(
        i1.mem_bytes >= 160 << 30,
        "the clamped floor survives emission: {}",
        i1.mem_bytes
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-Q (merged_bug_057)** — *time-only BestEffort (SerialFloor)
/// featureless demand fitting every class does NOT mint StaleSolve
/// and KEEPS the designed agnostic lane* (the false-pin inverse).
///
/// Pre-fix `classify_cell_emission` gated the HwAgnostic lane on a
/// flattened `solve_infeasible: bool` — true for EVERY BestEffort
/// including time-only SerialFloor whose demand fits every class —
/// and the StaleSolve mint had no demand-vs-ceiling premise, so
/// size-hosting featureless demand was pinned to the mem-largest
/// class with `resolved==solved`, a false "no longer hostable" WARN,
/// and an `exit=stale_resolved` increment: demand concentrated on
/// the most expensive class exactly under capacity/interrupt
/// pressure. The typed reason (`Option<InfeasibleReason>`) lets the
/// agnostic gate key on SIZE-infeasibility.
#[tokio::test]
async fn time_only_best_effort_keeps_the_agnostic_lane() {
    use crate::sla::metrics::counter_map_by;
    use crate::sla::types::{DurationFit, RefSeconds};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // SerialFloor fit: S=3000s — the serial floor breaches the only
    // tier bound (p90=1200s) at EVERY hw class (the largest factor,
    // intel-8 at 2.0, still leaves S_eff=1500 > 1200), so the
    // hw-aware solve is BestEffort{SerialFloor} everywhere, while the
    // demand envelope (6 GiB mem fit, 10 GiB disk) fits EVERY class.
    let mut f = make_fit("test-pkg");
    f.fit = DurationFit::Amdahl {
        s: RefSeconds(3000.0),
        p: RefSeconds(30.0),
    };
    actor.sla_estimator.seed(f);
    actor.test_inject_ready("d-serial", Some("test-pkg"), "x86_64-linux", false);

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let (hw, cost, g) = actor.solve_inputs();
    let state = actor.dag.node("d-serial").unwrap();
    let i = {
        let _g = metrics::set_default_local_recorder(&rec);
        actor.solve_intent_for(state, &hw, &cost, g)
    };
    // ONE snapshot read (it drains).
    let exits = counter_map_by(
        &snap,
        "rio_scheduler_sla_hw_ladder_exhausted_total",
        Some("exit"),
    );
    assert!(
        i.hw_class_names.is_empty(),
        "time-only infeasibility keeps the agnostic lane (empty \
         affinity; the controller's fallback arch-matches) — a pin to \
         the mem-largest class is merged_bug_057: {i:?}"
    );
    assert_eq!(
        exits.get("stale_resolved").copied().unwrap_or(0),
        0,
        "nothing is stale: no false 'no longer hostable' disclosure \
         for fitting demand; exits: {exits:?}"
    );
    assert!(
        exits.is_empty(),
        "no ladder-exhausted exit fires for time-only fitting demand; \
         exits: {exits:?}"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-S (merged_bug_004)** — *an od-pinned intent over a
/// `[spot, on-demand]` rung config NEVER lands on a spot rung* —
/// end-to-end through the memo pin filter, `retain_hosting_cells`'
/// ladder expansion, and `cells_to_selector_terms`.
///
/// Pre-fix `retain_hosting_cells` appended rung cells at EVERY
/// configured capacity type without receiving the operator pin — a
/// pin honored by every producer arm (memo filter, bypass Some-arm,
/// StaleSolve pinned arm) was silently widened at the post-finalize
/// seam, and with the controller's spot-first `cell_rank` an
/// od-pinned build was preferentially provisioned onto a spot rung
/// node with zero disclosure (the pin-times-ladder interaction the
/// pre-fix signature made structurally untestable).
#[tokio::test]
async fn od_pinned_intent_never_lands_on_a_spot_rung() {
    use crate::sla::config::{CapacityLadder, LadderRung};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // intel-6 gains a declared rung sibling (intel-7) hosting BOTH
    // capacity types (the default `[Spot, Od]`).
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-6")
        .unwrap()
        .ladder = Some(CapacityLadder {
        rungs: vec![LadderRung {
            class: "intel-7".into(),
        }],
    });
    actor.test_inject_ready("d-od-pin", Some("test-pkg"), "x86_64-linux", false);
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "test-pkg".into(),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        }]);

    let state = actor.dag.node("d-od-pin").unwrap();
    let (hw, cost, g) = actor.solve_inputs();
    let intent = actor.solve_intent_for(state, &hw, &cost, g);

    assert!(
        !intent.hw_class_names.is_empty(),
        "the od pin routes (precondition): {intent:?}"
    );
    // STRUCTURAL: no emitted affinity term may carry a spot
    // capacity-type requirement under an od pin — neither the parent
    // cell nor any ladder rung.
    let spot_terms: Vec<_> = intent
        .node_affinity
        .iter()
        .filter(|t| {
            t.match_expressions.iter().any(|r| {
                r.key == "karpenter.sh/capacity-type" && r.values.iter().any(|v| v == "spot")
            })
        })
        .collect();
    assert!(
        spot_terms.is_empty(),
        "an od-pinned intent must NEVER emit a spot cell (the ladder \
         expansion widened the pin pre-fix — merged_bug_004): \
         {spot_terms:?}"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-T (merged_bug_067)** — *pinned demand with a pin-honoring
/// sibling routes to it; pinned demand with NO pin-honoring class
/// mints `PinGated`* (the inversion's inverse + the letter's premise).
///
/// Pre-fix the `PinGated` guard adjudicated the `--capacity` pin via
/// the capacity-BLIND `reference_hw_class_for_system` (arch/features/
/// size only) and pre-empted the capacity-aware candidate walk
/// whenever ANY size-hosting class existed — empty emission then
/// routed the controller's `fallback_cell` first-cap with no capacity
/// term, silently INVERTING the pin while a pin-honoring sibling
/// route existed.
#[tokio::test]
async fn pinned_demand_prefers_the_pin_honoring_sibling() {
    use crate::actor::snapshot::CellEmission as E;
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Reference class od-only; siblings (intel-7/8) keep [Spot, Od].
    actor.sla_config.reference_hw_class = "intel-6".into();
    actor
        .sla_config
        .hw_classes
        .get_mut("intel-6")
        .unwrap()
        .capacity_types = vec![CapacityType::Od];
    actor.test_inject_ready("d-pin-sib", Some("test-pkg"), "x86_64-linux", false);
    let (_, cost, _) = actor.solve_inputs();

    // Face 1: a spot pin the reference class refuses but a SIBLING
    // hosts must route to the sibling at the pinned capacity — never
    // mint PinGated (whose empty emission inverts the pin downstream).
    let e = actor.classify_cell_emission(
        actor.dag.node("d-pin-sib").unwrap(),
        Some(CapacityType::Spot),
        4,
        4 << 30,
        &cost,
        "t",
        &[],
        None,
        false,
    );
    let E::Cells(cells) = &e else {
        panic!(
            "a pin-honoring sibling exists (intel-7/8 host spot) — the \
             pin routes to it, never PinGated/empty (merged_bug_067): {e:?}"
        )
    };
    assert!(
        !cells.is_empty() && cells.iter().all(|(_, c)| *c == CapacityType::Spot),
        "every routed cell honors the pin: {cells:?}"
    );

    // Face 2 (the letter's premise): NO class hosts the pin — every
    // class od-only, spot pinned — while size-hosting classes exist:
    // PinGated is the honest letter (empty emission, the pin is the
    // binding axis).
    for h in ["intel-6", "intel-7", "intel-8"] {
        actor
            .sla_config
            .hw_classes
            .get_mut(h)
            .unwrap()
            .capacity_types = vec![CapacityType::Od];
    }
    let e = actor.classify_cell_emission(
        actor.dag.node("d-pin-sib").unwrap(),
        Some(CapacityType::Spot),
        4,
        4 << 30,
        &cost,
        "t",
        &[],
        None,
        false,
    );
    assert!(
        matches!(e, E::PinGated),
        "no pin-honoring class anywhere + size-hosting classes exist \
         => PinGated (the premise): {e:?}"
    );

    // W9-U: oversized pinned demand and fitting pinned demand get the
    // SAME pin-respect (population: both). Fitting → face 1 above
    // (sibling Cells, all at the pin). Oversized → the StaleSolve walk
    // honors the pin (one cell at the pinned capacity) — restore
    // dual-cap classes and drive the oversize face.
    for h in ["intel-6", "intel-7", "intel-8"] {
        actor
            .sla_config
            .hw_classes
            .get_mut(h)
            .unwrap()
            .capacity_types = vec![CapacityType::Spot, CapacityType::Od];
    }
    let e = actor.classify_cell_emission(
        actor.dag.node("d-pin-sib").unwrap(),
        Some(CapacityType::Spot),
        383,
        412 << 30,
        &cost,
        "t",
        &[],
        Some(crate::sla::solve::InfeasibleReason::MemCeiling),
        false,
    );
    let E::StaleSolve { cells, .. } = &e else {
        panic!("oversized pinned demand re-solves (StaleSolve): {e:?}")
    };
    assert!(
        cells.len() == 1 && matches!(cells[0].1, CapacityType::Spot),
        "the oversize face honors the pin exactly like the fitting \
         face (W9-U): {cells:?}"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-V (merged_bug_043(1))** — *a genuinely-unhostable drv under
/// continuous detail-jitter churn POISONS at the budget* (the
/// eternal-Ready inverse; the jitter population driven).
///
/// The controller's verdict detail embeds per-solve `cores`/
/// `mem_bytes` — routine refit/price churn faster than the ~5min
/// budget window re-minted a byte-different detail every pass, and
/// the pre-fix reset key (raw detail equality) restarted the count at
/// 1 forever: the live_051(c) budget was structurally defeated for
/// exactly the churn population it shipped to kill. The reset key is
/// now the scheduler's hosting-class config census (the axis the law
/// cares about: a config reload re-opens the heal window) — demand
/// jitter does not reset.
#[tokio::test]
async fn jitter_churn_does_not_defeat_the_verdict_budget() {
    use crate::actor::snapshot::NO_HOST_VERDICTS_TO_POISON as N;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-jitter", Some("test-pkg"), "x86_64-linux", false);
    let jitter_detail = |k: u32| {
        format!(
            "no [sla.hw_classes] entry hosts system=x86_64-linux cores={} \
             mem_bytes={} required_features=[]; configured classes: \
             [intel-6, intel-7, intel-8]",
            4 + k,
            (6u64 << 30) + u64::from(k)
        )
    };

    // N−1 applied acks, each with a byte-DIFFERENT detail (the
    // refit/price jitter shape): counted, never poisoned.
    for k in 1..N {
        let p = actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-jitter", &jitter_detail(k))],
            )
            .expect("applied under leadership");
        assert!(p.is_empty(), "no poison at {k} < {N}");
    }
    // The Nth jittered verdict crosses the budget — the config census
    // never changed, so the evidence is consecutive.
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-jitter", &jitter_detail(N))],
        )
        .expect("applied under leadership");
    assert_eq!(
        p.len(),
        1,
        "demand jitter must NOT defeat the verdict budget \
         (merged_bug_043: the reset key is the config census, never \
         the formatted detail) — budget crossing at exactly N = {N}"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-Y (bug_119)** — *heal-then-relapse for the same
/// `(tenant, pname)` discloses BOTH episodes* (warn + increment) —
/// the per-episode quantifier driven across the latch boundary.
///
/// Pre-fix the `disclose_once` LRU was INSERT-ONLY: no heal-edge pop
/// existed (re-arm only via leader transition or 1024-entry
/// eviction), so the spec law's per-episode "clamp disclosed"
/// obligation silently became per-TENURE — every recurrence within a
/// tenure was invisible to `increase()`-based monitoring while the
/// sibling `all_masked` exit on the SAME metric re-armed correctly
/// via the `ice_exhausted` rising edge. The latch now lives where the
/// heal edge is visible: a healthy classified emission
/// (`Cells`/`HwAgnostic`/memo-survives) pops the latch.
#[tokio::test]
async fn heal_then_relapse_discloses_both_episodes() {
    use crate::sla::metrics::counter_map_by;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let mut actor = bare_actor_hw(db.pool.clone());
    for h in ["intel-6", "intel-7", "intel-8"] {
        actor.sla_config.hw_classes.get_mut(h).unwrap().max_mem = Some(128 << 30);
    }
    actor.test_inject_ready("d-relapse", Some("test-pkg"), "x86_64-linux", false);
    let (hw, cost, g) = actor.solve_inputs();

    // Episode 1: memoize, then bump the floor past every class
    // ceiling — Unhostable, disclosed.
    let i0 = actor.solve_intent_for(actor.dag.node("d-relapse").unwrap(), &hw, &cost, g);
    assert!(!i0.hw_class_names.is_empty(), "precondition: routed");
    actor
        .dag
        .node_mut("d-relapse")
        .unwrap()
        .sched
        .resource_floor
        .mem_bytes = 160 << 30;
    {
        let _g = metrics::set_default_local_recorder(&rec);
        let _ = actor.solve_intent_for(actor.dag.node("d-relapse").unwrap(), &hw, &cost, g);
    }

    // HEAL: the class ceilings re-grow past the floor — the emission
    // routes again (the heal edge, visible at the classifier).
    for h in ["intel-6", "intel-7", "intel-8"] {
        actor.sla_config.hw_classes.get_mut(h).unwrap().max_mem = Some(256 << 30);
    }
    {
        let _g = metrics::set_default_local_recorder(&rec);
        let healed = actor.solve_intent_for(actor.dag.node("d-relapse").unwrap(), &hw, &cost, g);
        assert!(
            !healed.hw_class_names.is_empty(),
            "the heal routes: {healed:?}"
        );
    }

    // RELAPSE: ceilings shrink again — the SAME (tenant, pname) must
    // disclose the second episode.
    for h in ["intel-6", "intel-7", "intel-8"] {
        actor.sla_config.hw_classes.get_mut(h).unwrap().max_mem = Some(128 << 30);
    }
    {
        let _g = metrics::set_default_local_recorder(&rec);
        let _ = actor.solve_intent_for(actor.dag.node("d-relapse").unwrap(), &hw, &cost, g);
    }

    // ONE snapshot read (it drains) — counters accumulate across the
    // three recorded passes.
    let exits = counter_map_by(
        &snap,
        "rio_scheduler_sla_hw_ladder_exhausted_total",
        Some("exit"),
    );
    assert_eq!(
        exits.get("unhostable").copied().unwrap_or(0),
        2,
        "heal-then-relapse discloses BOTH episodes (bug_119: the \
         per-episode law, not per-tenure) — exits: {exits:?}"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-W (merged_bug_043(2))** — *a Pending re-ack (Job-EXISTS echo)
/// does NOT reset the counter; the fresh spawn edge does* (both
/// edges).
///
/// The pool reconciler re-acks already-Pending Jobs in `spawned`
/// every tick; pre-fix the unconditional spawned-ack reset treated
/// that echo as a heal, so a genuinely-unhostable drv with a
/// Pending-forever Job kept its track at zero and looped Ready
/// eternally. The heal witness is now the FRESH edge: an ack whose
/// drv has no live `acked_spawned` entry (the entry is refreshed per
/// ack and pruned at 2× the defer window, so a still-Pending Job's
/// re-acks are echoes by construction).
#[tokio::test]
async fn pending_reack_echo_does_not_reset_the_verdict_budget() {
    use crate::actor::snapshot::NO_HOST_VERDICTS_TO_POISON as N;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-echo", Some("test-pkg"), "x86_64-linux", false);
    let spawned = rio_proto::types::SpawnIntent {
        intent_id: "d-echo".into(),
        ..Default::default()
    };

    // Fresh edge: the FIRST spawned ack resets (the heal) — drive the
    // track to N−1, spawn-ack, then N−1 more verdicts stay unpoisoned.
    for _ in 1..N {
        actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-echo", "A")],
            )
            .expect("applied");
    }
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&spawned),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied");
    for k in 1..N {
        let p = actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-echo", "A")],
            )
            .expect("applied");
        assert!(p.is_empty(), "fresh spawn edge healed the track ({k})");
    }
    // Echo edge: the drv's acked_spawned entry is LIVE (just
    // refreshed by the verdict-run? no — by the spawn ack above and
    // every subsequent spawned re-ack below). A re-ack does NOT
    // reset: the track sits at N−1; one more verdict crosses.
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&spawned),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied (the echo)");
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-echo", "A")],
        )
        .expect("applied");
    assert_eq!(
        p.len(),
        1,
        "a Job-EXISTS echo is not a hosting witness — the track \
         survived the re-ack and the budget crossed (merged_bug_043(2))"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W9-X (merged_bug_043(3))** — *verdicts-cease-while-Ready decays
/// the track structurally: a later identical verdict cannot claim
/// false consecutiveness* (the frozen-29+1 shape pinned).
///
/// `UnplaceableAllMasked` ships no verdict (cover.rs's
/// `verdict_reason` law), so pre-fix a track frozen at 29 survived
/// any masked period and poisoned on ONE fresh verdict with a false
/// "30 consecutive" message. The pass stamp types the non-event:
/// verdict-carrying acks advance the pass ordinal, and a track whose
/// stamp is not adjacent restarts at 1.
#[tokio::test]
async fn frozen_track_cannot_claim_false_consecutiveness() {
    use crate::actor::snapshot::NO_HOST_VERDICTS_TO_POISON as N;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-frozen", Some("test-pkg"), "x86_64-linux", false);
    actor.test_inject_ready("d-other", Some("test-pkg"), "x86_64-linux", false);

    // Drive d-frozen to N−1 (29 with the shipped budget).
    for _ in 1..N {
        actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-frozen", "A")],
            )
            .expect("applied");
    }
    // The gap: verdict-carrying passes that SKIP d-frozen (its
    // outcome went UnplaceableAllMasked — no verdict minted) while
    // d-other keeps the evidence flowing.
    for _ in 0..3 {
        actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-other", "A")],
            )
            .expect("applied");
    }
    // Verdicts resume for d-frozen with an IDENTICAL detail: the
    // streak broke — restart at 1, never a false "30 consecutive".
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-frozen", "A")],
        )
        .expect("applied");
    assert!(
        p.is_empty(),
        "a frozen track restarts after a pass gap (merged_bug_043(3)) \
         — got a poison claiming false consecutiveness: {p:?}"
    );
}

// ---------------------------------------------------------------------------
// merged_bug_125: typed lifecycle edges for the ack-apply planes (W10-U/V/W)
// ---------------------------------------------------------------------------

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W10-U (merged_bug_125 edge 1)** — the spawned-ack heal consumes
/// an AGE witness, never bare map-presence (R26: presence in the
/// lazily-pruned `acked_spawned` map is an INCOMPLETE view of ack
/// history — the retain is gated behind spawned-carrying acks and
/// ordered after the insert, so a fleet-quiet gap leaves STALE
/// entries that make a genuinely new spawn read as an echo).
///
/// Drive: spawn-ack (entry minted) → N−1 no-host verdicts (none of
/// these acks carry `spawned`, so the gated retain never runs) → the
/// fleet-quiet gap (the entry is backdated past the staleness
/// horizon — state-modeled elapsed time, the `backdate` precedent;
/// during a real gap the retain cannot fire by construction) → the
/// controller spawns a FRESH Job for the drv (the old one died in
/// the gap) and acks it → one more verdict.
///
/// Pre-fix red: the stale entry is PRESENT, the fresh spawn reads as
/// an echo, the track survives at N−1, and the next verdict crosses
/// the budget — a FALSE NoHostPoison for a drv whose hosting just
/// got re-witnessed. Post-fix: the heal keys on the entry's AGE (the
/// same staleness horizon the retain prunes at): stale ⇒ the gap was
/// genuine ⇒ heal; the track restarts and N−1 further verdicts stay
/// unpoisoned.
#[tokio::test]
async fn quiet_gap_spawn_heals_by_age_not_map_presence() {
    use crate::actor::snapshot::NO_HOST_VERDICTS_TO_POISON as N;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-gap", Some("test-pkg"), "x86_64-linux", false);
    let spawned = rio_proto::types::SpawnIntent {
        intent_id: "d-gap".into(),
        ..Default::default()
    };

    // The first spawn ack mints the entry.
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&spawned),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied");
    // N−1 verdicts (no spawned plane → the gated retain never runs).
    for _ in 1..N {
        actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-gap", "no class hosts")],
            )
            .expect("applied");
    }
    // The fleet-quiet gap: the entry outlives the staleness horizon
    // in place (no spawned-carrying ack arrives to prune it).
    let stale =
        crate::db::attempts::epoch_now() - 2.0 * crate::actor::pull::ACKED_SPAWNED_DEFER_SECS - 1.0;
    actor
        .acked_spawned
        .insert(crate::state::DrvHash::from("d-gap"), stale);

    // The GENUINE fresh spawn after the gap.
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&spawned),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied");
    // One more verdict: a healed track sits at 1, far from the budget.
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-gap", "no class hosts")],
        )
        .expect("applied");
    assert!(
        p.is_empty(),
        "left (pre-fix): the stale acked_spawned entry made the genuine \
         post-gap spawn read as a Job-EXISTS echo — no heal, the N−1 track \
         survived, and this verdict crossed the budget: a FALSE \
         NoHostPoison ({p:?}) / right: the heal consumes the AGE witness \
         (entry older than the staleness horizon ⇒ the gap was genuine ⇒ \
         reset)"
    );
    // And the live-echo half still holds (merged_bug_043(2)): a FRESH
    // entry's re-ack is an echo — drive back to N−1 and confirm the
    // echo does not heal.
    for _ in 2..N {
        actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-gap", "no class hosts")],
            )
            .expect("applied");
    }
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&spawned),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied (live echo)");
    let p = actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-gap", "no class hosts")],
        )
        .expect("applied");
    assert_eq!(
        p.len(),
        1,
        "a LIVE entry's re-ack stays an echo (no heal) — the budget crosses"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W10-V (merged_bug_125 edge 2)** — `ArmDecode::Empty` is a typed
/// NEGATIVE edge: a spawned echo declaring "no cells" (both arrays
/// empty — the hw-agnostic shape) DISARMS a stale `dispatched_cells`
/// entry left by an earlier Armed echo. Pre-fix the Empty arm was a
/// no-op, so a re-dispatched intent's first pull `ice.clear()`ed
/// exactly the cell that had been failing (the stale set). The
/// no-information sibling stays inert: `LegacyUnarmed` (exactly one
/// array empty — the pre-field-14 echo) neither arms nor disarms.
#[tokio::test]
async fn empty_arm_decode_disarms_stale_dispatched_cells() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-arm", Some("test-pkg"), "x86_64-linux", false);

    // The producer-shaped Armed echo: take the REAL emitted intent.
    let snap = actor.compute_spawn_intents(&Default::default());
    let intent = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "d-arm")
        .expect("emitted")
        .clone();
    assert!(
        !intent.node_affinity.is_empty(),
        "precondition: hw-routed intent (Armed echo shape)"
    );
    actor
        .handle_ack_spawned_intents(std::slice::from_ref(&intent), &[], &[], &[], &[], None, &[])
        .expect("applied");
    assert!(
        actor.dispatched_cells.get("d-arm").is_some(),
        "Armed echo arms dispatched_cells"
    );

    // The drv re-solves hw-agnostic; the next echo is Empty (both
    // arrays empty).
    let empty_echo = rio_proto::types::SpawnIntent {
        intent_id: "d-arm".into(),
        ..Default::default()
    };
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&empty_echo),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied");
    assert!(
        actor.dispatched_cells.get("d-arm").is_none(),
        "left (pre-fix): the Empty decode was a silent no-op — the STALE \
         armed cells survived, and the re-dispatched intent's first pull \
         would ice.clear() exactly the failing cell / right: Empty is a \
         typed negative edge — the entry disarms"
    );

    // LegacyUnarmed (one side empty) is NO-INFORMATION: re-arm, then a
    // legacy echo must neither arm nor disarm.
    actor
        .handle_ack_spawned_intents(std::slice::from_ref(&intent), &[], &[], &[], &[], None, &[])
        .expect("applied");
    let legacy_echo = rio_proto::types::SpawnIntent {
        intent_id: "d-arm".into(),
        hw_class_names: intent.hw_class_names.clone(),
        node_affinity: vec![],
        ..Default::default()
    };
    actor
        .handle_ack_spawned_intents(
            std::slice::from_ref(&legacy_echo),
            &[],
            &[],
            &[],
            &[],
            None,
            &[],
        )
        .expect("applied");
    assert!(
        actor.dispatched_cells.get("d-arm").is_some(),
        "LegacyUnarmed carries no cell information — it must not disarm"
    );
}

// r[verify scheduler.sla.ceiling.stale-solve-revalidation+2]
/// **W10-W (merged_bug_125 edge 3)** — the verdict-pass ordinal
/// witnesses the FULL decoded rejected plane, not the NoHost subset.
/// Pre-fix only NoHost-carrying acks advanced it, so an OverCap-ONLY
/// applied ack (a real cover pass in which this drv received no
/// verdict) left the ordinal frozen — a masked drv's track read
/// pass-ADJACENT when NoHost verdicts resumed, claiming false
/// consecutiveness (the frozen-29 shape one plane over; the residual
/// comment's "other drvs' verdicts DO advance it" was false for the
/// OverCap-only pass).
#[tokio::test]
async fn over_cap_only_pass_advances_the_verdict_ordinal() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d-pass", Some("test-pkg"), "x86_64-linux", false);
    actor.test_inject_ready("d-other", Some("test-pkg"), "x86_64-linux", false);

    // Two adjacent NoHost passes: track at 2.
    for _ in 0..2 {
        actor
            .handle_ack_spawned_intents(
                &[],
                &[],
                &[],
                &[],
                &[],
                None,
                &[no_host_verdict("d-pass", "no class hosts")],
            )
            .expect("applied");
    }
    assert_eq!(
        actor
            .supply_reval
            .no_host_verdicts
            .get(&crate::state::DrvHash::from("d-pass"))
            .map(|t| t.count),
        Some(2),
        "precondition: two adjacent passes counted"
    );

    // A cover pass in which d-pass got NO verdict but ANOTHER drv
    // drew an OverCap — the rejected plane is non-empty, the pass
    // happened.
    actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[over_cap_verdict("d-other")],
        )
        .expect("applied");

    // NoHost resumes for d-pass.
    actor
        .handle_ack_spawned_intents(
            &[],
            &[],
            &[],
            &[],
            &[],
            None,
            &[no_host_verdict("d-pass", "no class hosts")],
        )
        .expect("applied");
    assert_eq!(
        actor
            .supply_reval
            .no_host_verdicts
            .get(&crate::state::DrvHash::from("d-pass"))
            .map(|t| t.count),
        Some(1),
        "left (pre-fix): the OverCap-only pass did not advance the \
         ordinal, so the resumed NoHost read pass-adjacent and the track \
         continued 2 → 3 (false consecutiveness across a real pass gap) / \
         right: ANY decoded verdict plane advances the ordinal; the gap \
         restarts the track at 1"
    );
}

// r[verify sched.sla.pin-wire]
/// **W10-Z (bug_121, the R25 injectivity census)** — *every
/// [`CellEmission`]-shaped population × its wire image
/// `(cell set, capacity_pin)` × the consumer disposition its premise
/// derives: no two letters share BOTH a wire image and a consumer
/// disposition.* The pre-fix red on the image axis: PinGated's wire
/// image was byte-identical to HwAgnostic's (empty cells, no pin
/// field existed) while their dispositions diverge (typed pend vs the
/// designed first-cap fallback walk) — the silent pin→spot decay.
/// Post-fix the `capacity_pin` field splits those images. The two
/// populations that STILL share an image — PinGated and a pinned
/// Unhostable, both `(∅, Some(pin))` — are split by the consumer's
/// OWN re-derivation (the controller's pin-stripped fallback
/// attribution, pinned at `w10y_…` / `pinned_but_genuinely_…` in
/// rio-controller), and their scheduler-side mint premises are the
/// two branches of exactly that predicate, asserted here over the
/// cap-blind reference resolve: PinGated mints ONLY when a
/// size-hosting class exists ignoring the pin; the pinned walk falls
/// to Unhostable ONLY when none does. Cells rows are image-disjoint
/// by non-emptiness. Populations driven through the production
/// `solve_intent_for` (the one mint site), never hand-built.
#[tokio::test]
async fn w10z_cell_emission_wire_image_injectivity() {
    use crate::sla::config::{ARCH_LABEL, CapacityType, NodeLabelMatch};

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Builder classes get explicit amd64 arch labels (the bug_008
    // form) so the riscv population below is structurally unhostable,
    // and SPOT-ONLY capacity types so an od pin has no hosting class
    // while the size axis stays hostable (the PinGated premise).
    for d in actor.sla_config.hw_classes.values_mut() {
        d.labels.push(NodeLabelMatch {
            key: ARCH_LABEL.into(),
            value: "amd64".into(),
        });
        d.capacity_types = vec![CapacityType::Spot];
    }
    actor.sla_tiers = actor.sla_config.solve_tiers();

    // Population 1 — HwAgnostic: unfitted featureless x86 demand, no
    // override. The designed quiet edge.
    actor.test_inject_ready("d-agnostic", Some("never-fit"), "x86_64-linux", false);
    // Population 2 — PinGated: unfitted featureless x86 demand with a
    // `--capacity=on-demand` pin no (spot-only) class hosts, while
    // size-hosting classes exist ignoring the pin.
    actor.test_inject_ready("d-pin", Some("pin-pkg"), "x86_64-linux", false);
    // Population 3 — pinned Unhostable: cores+capacity override on a
    // system no class hosts (arch axis binds, not the pin).
    actor.test_inject_ready("d-rv-pin", Some("rv-pkg"), "riscv64-linux", false);
    // Population 4 — Cells: fitted, routed (image-disjoint by
    // non-emptiness; pin axis exercised on the empty images above).
    actor.test_inject_ready("d-cells", Some("test-pkg"), "x86_64-linux", false);
    actor.sla_estimator.seed_overrides(vec![
        crate::db::SlaOverrideRow {
            pname: "pin-pkg".into(),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        },
        crate::db::SlaOverrideRow {
            pname: "rv-pkg".into(),
            cores: Some(16.0),
            capacity_type: Some("on-demand".into()),
            ..Default::default()
        },
    ]);

    let (hw, cost, ig) = actor.solve_inputs();
    let image = |drv: &str| {
        let state = actor.dag.node(drv).unwrap();
        let i = actor.solve_intent_for(state, &hw, &cost, ig);
        // The wire image: what `to_proto` ships — cell emptiness +
        // the pin token through the shared alphabet.
        let pin_wire = i.capacity_pin.map(|c| {
            rio_common::cell_wire::WireCapacity::from(c)
                .wire_str()
                .to_owned()
        });
        (i.node_affinity.is_empty(), pin_wire, i)
    };

    let (agn_empty, agn_pin, agn) = image("d-agnostic");
    let (pin_empty, pin_pin, pin) = image("d-pin");
    let (rv_empty, rv_pin, rv) = image("d-rv-pin");
    let (cells_empty, cells_pin, _) = image("d-cells");

    // Row images.
    assert!(
        agn_empty && agn_pin.is_none(),
        "HwAgnostic image = (∅, None)"
    );
    assert!(
        pin_empty && pin_pin.as_deref() == Some("od"),
        "PinGated image = (∅, Some(od)) — the pin SURVIVES the wire; \
         got (empty={pin_empty}, pin={pin_pin:?})"
    );
    assert!(
        rv_empty && rv_pin.as_deref() == Some("od"),
        "pinned Unhostable image = (∅, Some(od)); got (empty={rv_empty}, pin={rv_pin:?})"
    );
    assert!(!cells_empty, "Cells image is non-empty (image-disjoint)");
    assert!(cells_pin.is_none(), "unpinned routed demand carries no pin");

    // THE bug_121 axis: PinGated and HwAgnostic MUST NOT share a wire
    // image (pre-fix they did — both (∅, nothing)).
    assert_ne!(
        (agn_empty, &agn_pin),
        (pin_empty, &pin_pin),
        "two letters sharing a wire image re-creates the silent-empty \
         population the alphabet was minted to kill (R25)"
    );

    // The shared-(∅, Some) pair splits on the consumer's own
    // re-derivation; the mint premises are its two branches. Probe the
    // cap-blind reference resolve (what the controller's pin-stripped
    // fallback re-derives) at each row's own demand:
    let feat: Vec<String> = vec![];
    let premise = |state_sys: &str, i: &crate::state::SolvedIntent| {
        actor
            .sla_config
            .reference_hw_class_for_system(
                state_sys,
                i.cores,
                i.mem_bytes,
                &feat,
                cost.catalog_ceilings(),
                cost.resolved_global(),
                None,
            )
            .is_some()
    };
    assert!(
        premise("x86_64-linux", &pin),
        "PinGated premise: a size-hosting class EXISTS ignoring the pin \
         — the consumer's pin-stripped re-eval succeeds ⇒ typed pend"
    );
    assert!(
        !premise("riscv64-linux", &rv),
        "pinned-Unhostable premise: NO class hosts even ignoring the \
         pin — the consumer's pin-stripped re-eval fails ⇒ the \
         poison-feeding NoHostingClass, exactly like an unpinned config \
         gap (the pin is not the differentiator)"
    );
    // Belt: the agnostic row's demand is hostable sans pin too — its
    // disposition (designed first-cap walk) differs from PinGated's
    // pend purely BY the image split asserted above.
    assert!(premise("x86_64-linux", &agn));
}
