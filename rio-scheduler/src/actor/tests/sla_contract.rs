//! CR-1 actor-boundary state-machine contract tests.
//!
//! Round 1 fixed primitives so they *can* satisfy a stated invariant
//! (ε_h seed = `hash(drv) ^ inputs_gen`; SolveCache keyed on
//! `model_key_hash`; `IceBackoff::clear` resets on success). Round 2
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

use super::*;
use std::collections::BTreeMap;

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

/// **Selector stability** (`r[sched.sla.hw-class.epsilon-explore+2]`):
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
// r[verify sched.sla.hw-class.epsilon-explore+2]
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
    let polls: Vec<_> = (0..8).map(|_| affinity_map(&actor)).collect();
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
        );
    }
    assert_eq!(
        actor.ice.step(&cell),
        Some(2),
        "spawned-ack must NOT clear; backoff doubles across consecutive marks"
    );
    // Arm-on-ack: the spawned echo populates `dispatched_cells` with
    // the cell recovered from `(hw_class_names[0], node_affinity[0])`
    // — `cells[0]` round-trips through the wire.
    assert_eq!(
        actor.dispatched_cells.get("d0").as_deref(),
        Some(&(intent.hw_class_names[0].clone(), CapacityType::Spot)),
        "spawned-ack arms dispatched_cells from the wire form"
    );

    actor.handle_ack_spawned_intents(&[], &[], &["intel-6:spot".into()]);
    assert_eq!(
        actor.ice.step(&cell),
        None,
        "registered_cells is the success edge → clears"
    );
}
