//! Completion handling: retry/poison thresholds, dep-chain release, duplicate idempotence.
// r[verify sched.completion.idempotent]
// r[verify sched.state.transitions]
// r[verify sched.state.terminal-idempotent]

use super::*;
use tracing_test::traced_test;

/// Self-check on [`setup_ca_fixture`]: the fixture returns with the
/// actor waiting for a `ProcessCompletion`, NOT past the CA-compare
/// callsite. If a future refactor made the fixture eagerly drive the
/// actor through completion (auto-dispatch → auto-BuildResult →
/// CA-compare fires before the test body seeds PG), the tests that
/// seed realisations AFTER the fixture would silently become vacuous.
///
/// Proof: the `rio_scheduler_ca_hash_compares_total` counter stays at
/// 0 after the fixture returns (no CA-compare ran), then increments
/// after `complete_ca` (the compare fires NOW, not earlier). Seeds a
/// prior realisation post-fixture and observes the match path take
/// effect — direct proof that post-fixture seeding still reaches the
/// first compare.
#[tokio::test]
async fn setup_ca_fixture_does_not_race_past_ca_compare() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let f = setup_ca_fixture("ca-race-guard").await?;

    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";

    // Precondition: NO CA-compare fired during setup. If the fixture
    // had raced past (e.g., synthesized a ProcessCompletion), this
    // counter would already be >0.
    assert_eq!(
        recorder.get(miss_key),
        0,
        "CA-compare fired during setup_ca_fixture — fixture raced past \
         the callsite; post-fixture PG seeding is now vacuous"
    );
    assert_eq!(
        recorder.get(match_key),
        0,
        "CA-compare fired during setup_ca_fixture"
    );

    // Seed a PRIOR realisation AFTER setup. Different modular_hash
    // (simulating a prior build), same output_path as what we'll
    // report. If the fixture raced past, this seed would be too late.
    let out_path = test_store_path("ca-race-out");
    let prior_hash: [u8; 32] = [0xAA; 32];
    seed_realisation(&f.pool, &prior_hash, "out", &out_path, &[0x42; 32]).await?;

    // Drive to completion — the CA-compare fires NOW.
    complete_ca(
        &f.actor,
        &f.executor_id,
        &f.drv_path,
        &[("out", &out_path, vec![0x42; 32])],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-race-guard")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);

    // The realisation seeded AFTER setup DID take effect: prior
    // realisation found → match counter increments. If the compare
    // had already fired pre-seed, we'd see miss=1 match=0.
    assert_eq!(
        recorder.get(match_key),
        1,
        "prior realisation seeded AFTER fixture did not take effect — \
         fixture raced past CA-compare"
    );
    assert!(
        info.ca_output_unchanged,
        "prior realisation found → matched=true → ca_output_unchanged=true"
    );

    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// CA early-cutoff compare: on successful completion of an `is_ca`
/// derivation, `handle_success_completion` checks each output_path
/// against the realisations table for a PRIOR build (different
/// modular_hash, same path). All-match → `ca_output_unchanged=true`.
/// The metric is labeled `{outcome=match|miss}`.
///
/// Three scenarios in one test (shared PG + actor setup, which is
/// the expensive part):
///   1. Output path has a prior realisation → `true`, counter{match}
///      += 1.
///   2. Output path has no prior → `false`, counter{miss} += 1.
///   3. Non-CA derivation → hook skipped entirely, counter untouched,
///      flag stays `false`. Proves the `is_ca` guard.
///
/// AND-fold correctness (multi-output with [match, miss] → `false`)
/// is covered by sending two BuiltOutputs in scenario 2.
#[tokio::test]
async fn ca_completion_hash_compare_sets_unchanged_and_counts() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let f = setup_ca_fixture("ca-match").await?;

    // Seed a PRIOR realisation: simulates a previous build (different
    // modular_hash) having registered the SAME output_path. The
    // CA-compare's `query_prior_realisation(path, exclude=our_hash)`
    // finds this row → match.
    let out_path = test_store_path("ca-match-out");
    let prior_modular: [u8; 32] = [0x77; 32];
    let out_hash: [u8; 32] = [0x42; 32];
    seed_realisation(&f.pool, &prior_modular, "out", &out_path, &out_hash).await?;

    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";
    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";

    // ─── Scenario 1: CA + prior realisation → unchanged=true ───────
    let pre = f
        .actor
        .debug_query_derivation("ca-match")
        .await?
        .expect("exists");
    assert!(pre.is_ca, "precondition: merged with is_ca=true");
    assert!(!pre.ca_output_unchanged, "default false before completion");

    let match_before = recorder.get(match_key);
    complete_ca(
        &f.actor,
        &f.executor_id,
        &f.drv_path,
        &[("out", &out_path, out_hash.to_vec())],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-match")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        info.ca_output_unchanged,
        "CA + prior realisation found → ca_output_unchanged=true"
    );
    assert_eq!(
        recorder.get(match_key) - match_before,
        1,
        "one matched output → counter{{outcome=match}} +1.\nCounters: {:#?}",
        recorder.all_keys()
    );

    // ─── Scenario 2: CA + mixed [match, miss] → AND-fold → false ───
    // Fresh worker (one-shot: scenario-1's executor drained on completion).
    let _rx2 = connect_executor(&f.actor, "ca-w2", "x86_64-linux", 1).await?;
    let mixed_modular: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(b"ca-fixture:ca-mixed").into()
    };
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-mixed");
    let mut node = make_test_node("ca-mixed", "x86_64-linux");
    node.is_content_addressed = true;
    node.ca_modular_hash = mixed_modular.to_vec();
    node.output_names = vec!["out".into(), "dev".into()];
    let _ev = merge_dag(&f.actor, build_id, vec![node], vec![], false).await?;

    // Seed prior for "out" only — "dev" has no prior.
    let mixed_out = test_store_path("ca-mixed-out");
    seed_realisation(&f.pool, &[0x88; 32], "out", &mixed_out, &[0xab; 32]).await?;

    let miss_before = recorder.get(miss_key);
    let match_before2 = recorder.get(match_key);
    complete_ca(
        &f.actor,
        "ca-w2",
        &drv_path,
        &[
            ("out", &mixed_out, vec![0xab; 32]),
            ("dev", &test_store_path("ca-mixed-dev"), vec![0xcd; 32]),
        ],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-mixed")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "AND-fold: [match, miss] → ca_output_unchanged=false (not last-iter-wins)"
    );
    assert_eq!(
        recorder.get(match_key) - match_before2,
        1,
        "mixed: one match recorded"
    );
    assert_eq!(
        recorder.get(miss_key) - miss_before,
        1,
        "mixed: one miss recorded"
    );

    // ─── Scenario 3: non-CA → hook skipped ─────────────────────────
    let _rx3 = connect_executor(&f.actor, "ca-w3", "x86_64-linux", 1).await?;
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ia-skip");
    let node = make_test_node("ia-skip", "x86_64-linux"); // is_content_addressed=false
    let _ev = merge_dag(&f.actor, build_id, vec![node], vec![], false).await?;

    // Seed a prior for the IA path — if the is_ca guard were missing,
    // this would match.
    let ia_out = test_store_path("ia-skip-out");
    seed_realisation(&f.pool, &[0x99; 32], "out", &ia_out, &[0xef; 32]).await?;

    let match_before3 = recorder.get(match_key);
    let miss_before3 = recorder.get(miss_key);
    complete_ca(
        &f.actor,
        "ca-w3",
        &drv_path,
        &[("out", &ia_out, vec![0xef; 32])],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ia-skip")
        .await?
        .expect("exists");
    assert!(!info.is_ca);
    assert!(
        !info.ca_output_unchanged,
        "non-CA → hook skipped, flag stays false"
    );
    assert_eq!(
        recorder.get(match_key),
        match_before3,
        "non-CA → no match increment"
    );
    assert_eq!(
        recorder.get(miss_key),
        miss_before3,
        "non-CA → no miss increment"
    );

    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// First-build self-exclusion (bughunt-mc196 regression).
///
/// The ONLY realisation for `own_path` is keyed on THIS build's
/// modular_hash (inserted by completion.rs's `insert_realisation`
/// just before the compare fires). `query_prior_realisation(path,
/// exclude=own_modular_hash)` filters it out → None → miss →
/// `ca_output_unchanged=false`.
///
/// **BEFORE the realisation-based fix:** `ContentLookup(H,
/// exclude=path)` was used, which for CA (same content → same path)
/// always excluded the only row — so this test passed by accident
/// but the SECOND-build case failed. The modular_hash exclusion
/// handles both correctly.
///
/// Mutation check: remove the `drv_hash != $2` predicate in
/// `query_prior_realisation` and this test FAILS with
/// `ca_output_unchanged == true`.
#[tokio::test]
async fn ca_compare_first_build_excludes_own_upload() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let f = setup_ca_fixture("ca-first").await?;

    // THE SELF-MATCH SETUP: seed a realisation for own_path keyed on
    // OUR OWN modular_hash — simulating completion.rs's
    // insert_realisation having fired (it runs BEFORE the compare).
    // No OTHER modular_hash points to this path.
    let own_path = test_store_path("ca-first-out");
    let own_hash: [u8; 32] = [0x33; 32];
    seed_realisation(&f.pool, &f.modular_hash, "out", &own_path, &own_hash).await?;

    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";
    let miss_before = recorder.get(miss_key);
    let match_before = recorder.get(match_key);

    complete_ca(
        &f.actor,
        &f.executor_id,
        &f.drv_path,
        &[("out", &own_path, own_hash.to_vec())],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-first")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "FIRST build: query_prior_realisation(path, exclude=own_modular) → None → \
         ca_output_unchanged=false. (Mutation: drop `drv_hash != $2` → true.)"
    );
    assert_eq!(
        recorder.get(miss_key) - miss_before,
        1,
        "self-excluded lookup → counter{{outcome=miss}} +1"
    );
    assert_eq!(
        recorder.get(match_key),
        match_before,
        "self-excluded → NO match increment"
    );

    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// Timeout regression guard: the CA-compare's PG lookup is wrapped
/// in `CA_CUTOFF_LOOKUP_TIMEOUT` (2s). A slow/unavailable PG must
/// NOT block completion indefinitely.
///
/// With the realisation-based compare (PG, not gRPC), we can't
/// easily hang PG from a test. Instead, this test verifies the
/// timeout wrapper EXISTS by completing normally (PG is fast) and
/// checking the state advances within a 10s outer bound. If a
/// refactor removes the timeout wrapper and PG ever blocks, this
/// test won't catch it locally but the VM test `ca-cutoff.nix` will
/// (it has `globalTimeout=600s`).
///
/// The outer 10s bound is the regression surface: completion
/// processing must be fast when PG is healthy.
#[tokio::test]
async fn ca_cutoff_compare_slow_store_doesnt_block_completion() -> TestResult {
    let f = setup_ca_fixture_configured("ca-slow", |a| a.with_grpc_timeout(Duration::from_secs(3)))
        .await?;

    complete_ca(
        &f.actor,
        &f.executor_id,
        &f.drv_path,
        &[("out", &test_store_path("ca-slow-out"), vec![0xAB; 32])],
    )
    .await?;

    // Outer guard: 10s. PG lookup + completion handling should be
    // sub-second when PG is healthy. If this blocks, something in
    // the compare path is awaiting without a timeout.
    let info = tokio::time::timeout(
        Duration::from_secs(10),
        f.actor.debug_query_derivation("ca-slow"),
    )
    .await
    .expect("actor blocked past 10s — timeout wrapper removed?")?
    .expect("derivation exists");

    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "completion must proceed when PG is responsive"
    );
    // No prior realisation seeded → lookup returns None → miss.
    assert!(
        !info.ca_output_unchanged,
        "no prior → miss → ca_output_unchanged=false"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// Zero-outputs edge: `built_outputs=[]` → `all_matched` starts
/// `false` (the `!result.built_outputs.is_empty()` early-false at
/// the top of the CA-compare block in `handle_completed`) →
/// `ca_output_unchanged` stays `false`. A worker bug that emits
/// zero outputs on success shouldn't accidentally enable cutoff
/// for downstream. Defensive — the worker SHOULD always emit ≥1.
#[tokio::test]
async fn ca_compare_zero_outputs_is_not_unchanged() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let f = setup_ca_fixture("ca-zero").await?;

    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";
    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let match_before = recorder.get(match_key);
    let miss_before = recorder.get(miss_key);

    // Built success with NO outputs. Unusual (worker bug), but
    // the CA hook must not read it as "all N outputs matched
    // (where N=0)" → true.
    complete_ca(&f.actor, &f.executor_id, &f.drv_path, &[]).await?;

    let info = f
        .actor
        .debug_query_derivation("ca-zero")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "zero outputs → all_matched starts false (!is_empty() guard); \
         NEVER flip true on no data"
    );
    // Loop body doesn't execute → no counter increment either way.
    assert_eq!(recorder.get(match_key), match_before, "no match recorded");
    assert_eq!(recorder.get(miss_key), miss_before, "no miss recorded");
    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// Empty-path edge: `output_path.is_empty()` → debug +
/// `all_matched=false` + `continue` (no PG lookup). Proves the
/// empty-path guard short-circuits before the query — a worker
/// sending an empty output_path (bug) is counted as a miss
/// explicitly rather than hitting PG with an empty WHERE.
#[tokio::test]
async fn ca_compare_empty_path_counts_as_miss() -> TestResult {
    let f = setup_ca_fixture("ca-empty").await?;

    // Empty output_path. Malformed — worker should always report
    // the realized path.
    complete_ca(
        &f.actor,
        &f.executor_id,
        &f.drv_path,
        &[("out", "", vec![0xCD; 32])],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-empty")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "empty path → all_matched=false (the is_empty() guard), no PG query fired"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// No-prior-realisation edge: output_path with no prior realisation
/// in PG → `Ok(None)` → `matched=false`. Proves a fresh first-ever
/// build (nothing seeded) degrades to "no cutoff" (safe — downstream
/// rebuilds) rather than crashing.
#[tokio::test]
async fn ca_compare_no_prior_counts_as_miss() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let f = setup_ca_fixture("ca-noprior").await?;

    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let miss_before = recorder.get(miss_key);

    // Two outputs, neither seeded in PG realisations. First lookup →
    // None → miss → short-circuit break.
    complete_ca(
        &f.actor,
        &f.executor_id,
        &f.drv_path,
        &[
            ("out", &test_store_path("ca-noprior-out1"), vec![0x01; 32]),
            ("dev", &test_store_path("ca-noprior-out2"), vec![0x02; 32]),
        ],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-noprior")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "no prior realisation → matched=false → ca_output_unchanged stays false"
    );
    assert!(
        recorder.get(miss_key) > miss_before,
        "no prior → at least one miss counter increment"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-propagate]
/// `verify_cutoff_candidates` wiring: the `|h| verified.contains_key(h)`
/// closure is LOAD-BEARING. A mutant that replaces it with `|_| true`
/// (skip every candidate unconditionally) would let nodes with no
/// prior output go `Skipped`.
///
/// Setup: A(CA)→B(CA)→C(CA) chain. Seed PG with a PRIOR build's
/// realisations for A AND B (via realisation_deps), but NOT C. Seed
/// MockStore with B's output path (so FindMissingPaths finds it).
/// Complete A with a prior realisation → `ca_output_unchanged=true`.
///
/// Expected:
///   - B goes Skipped (realisation_deps walk finds prior B, output
///     present in store → verified).
///   - C does NOT go Skipped (no prior C in realisation_deps → not
///     in walk result → verify rejects).
///   - [P0399] C goes Ready instead (all_deps_completed accepts Skipped).
///
/// Mutation check: `|_| true` → C goes Skipped too → `assert_ne` fails.
#[tokio::test]
async fn cascade_only_skips_verified_candidates() -> TestResult {
    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));
    let pool = test_db.pool.clone();
    let _db = test_db;

    // Current build's modular hashes (simulate gateway's
    // populate_ca_modular_hashes).
    let a_modular: [u8; 32] = [0xA0; 32];
    let b_modular: [u8; 32] = [0xB0; 32];
    let c_modular: [u8; 32] = [0xC0; 32];

    // PRIOR build's modular hashes — different drv (marker env
    // differs), same content → same output_path.
    let a_prior: [u8; 32] = [0xA1; 32];
    let b_prior: [u8; 32] = [0xB1; 32];

    let a_out = test_store_path("verify-a");
    let b_out = test_store_path("verify-b");

    // Chain A→B→C, all CA with pname set (name-suffix match).
    let mut node_a = make_test_node("verify-a", "x86_64-linux");
    node_a.is_content_addressed = true;
    node_a.ca_modular_hash = a_modular.to_vec();
    node_a.pname = "verify-a".into();
    let mut node_b = make_test_node("verify-b", "x86_64-linux");
    node_b.is_content_addressed = true;
    node_b.ca_modular_hash = b_modular.to_vec();
    node_b.pname = "verify-b".into();
    let mut node_c = make_test_node("verify-c", "x86_64-linux");
    node_c.is_content_addressed = true;
    node_c.ca_modular_hash = c_modular.to_vec();
    node_c.pname = "verify-c".into();

    let _rx = connect_executor(&handle, "verify-worker", "x86_64-linux", 4).await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![node_a, node_b, node_c],
        vec![
            make_test_edge("verify-b", "verify-a"),
            make_test_edge("verify-c", "verify-b"),
        ],
        false,
    )
    .await?;

    // Preconditions: B and C Queued.
    assert_eq!(
        handle
            .debug_query_derivation("verify-b")
            .await?
            .unwrap()
            .status,
        DerivationStatus::Queued
    );
    assert_eq!(
        handle
            .debug_query_derivation("verify-c")
            .await?
            .unwrap()
            .status,
        DerivationStatus::Queued
    );

    // Seed PRIOR build's realisations: A and B existed, C did NOT.
    // Plus realisation_deps: B depends on A (so the walk finds B
    // from A's prior modular hash).
    seed_realisation(&pool, &a_prior, "out", &a_out, &[0xAA; 32]).await?;
    seed_realisation(&pool, &b_prior, "out", &b_out, &[0xBB; 32]).await?;
    sqlx::query(
        "INSERT INTO realisation_deps (drv_hash, output_name, dep_drv_hash, dep_output_name) \
         VALUES ($1, 'out', $2, 'out')",
    )
    .bind(b_prior.as_slice())
    .bind(a_prior.as_slice())
    .execute(&pool)
    .await?;

    // Seed B's output in MockStore so FindMissingPaths reports it
    // present. AFTER merge (cache-check didn't see it) BEFORE A
    // completes (verify WILL see it).
    store.seed_with_content(&b_out, b"b-content");

    // Complete A. Its output_path matches the prior realisation
    // (a_prior, out) → ca_output_unchanged=true.
    complete_ca(
        &handle,
        "verify-worker",
        &test_drv_path("verify-a"),
        &[("out", &a_out, vec![0xAA; 32])],
    )
    .await?;
    barrier(&handle).await;

    let info_a = handle
        .debug_query_derivation("verify-a")
        .await?
        .expect("verify-a exists");
    assert!(
        info_a.ca_output_unchanged,
        "precondition: A matched prior realisation → ca_output_unchanged=true"
    );

    // B: Skipped (realisation_deps walk found b_prior, output
    // present in store → verified). Also stamped with output_paths.
    let info_b = handle
        .debug_query_derivation("verify-b")
        .await?
        .expect("verify-b exists");
    assert_eq!(
        info_b.status,
        DerivationStatus::Skipped,
        "B found in realisation_deps walk + output present → Skipped"
    );

    // C: NOT Skipped (no prior C in realisation_deps → walk didn't
    // find it → verify rejects). Mutation `|_| true` would Skip C.
    let info_c = handle
        .debug_query_derivation("verify-c")
        .await?
        .expect("verify-c exists");
    assert_ne!(
        info_c.status,
        DerivationStatus::Skipped,
        "C has no prior → verify rejects → NOT Skipped \
         (mutation |_| true would fail here — load-bearing assert)"
    );
    assert!(
        matches!(
            info_c.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "P0399: C goes Ready after B Skipped; got {:?}",
        info_c.status
    );

    Ok(())
}

/// Second-build positive case: a PRIOR build's realisation
/// (`(M_prior, out) → P`) exists ALONGSIDE this build's own
/// (`(M_current, out) → P` — same P, CA content-addressing).
/// `query_prior_realisation(P, exclude=M_current)` finds the prior
/// row → match → `ca_output_unchanged=true`.
///
/// Paired with `ca_compare_first_build_excludes_own_upload` this
/// proves "hide self, find sibling" — the modular_hash exclusion
/// isn't "any match → None" (over-correction).
#[tokio::test]
async fn ca_compare_second_build_matches_prior() -> TestResult {
    let f = setup_ca_fixture("ca-second").await?;

    // SAME output_path (CA: same content → same path). TWO
    // realisations: prior build's modular_hash AND our own.
    let out_path = test_store_path("ca-second-out");
    let out_hash: [u8; 32] = [0x55; 32];
    let prior_modular: [u8; 32] = [0x66; 32];
    // Our own row (simulating completion.rs insert_realisation
    // having fired before the compare).
    seed_realisation(&f.pool, &f.modular_hash, "out", &out_path, &out_hash).await?;
    // Prior build's row — different modular_hash, same path.
    seed_realisation(&f.pool, &prior_modular, "out", &out_path, &out_hash).await?;

    complete_ca(
        &f.actor,
        &f.executor_id,
        &f.drv_path,
        &[("out", &out_path, out_hash.to_vec())],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-second")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        info.ca_output_unchanged,
        "SECOND build: query_prior_realisation(P, exclude=M_current) → \
         finds M_prior → ca_output_unchanged=true (downstream CAN skip)"
    );

    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// Short-circuit on first miss: a 4-output CA completion where
/// output[0] misses (no prior realisation) should record exactly
/// ONE miss + 3 skipped_after_miss, not 4 misses. The AND-fold
/// result is already `false` after the first miss; the remaining
/// lookups can't flip it. The `break` at the short-circuit saves up
/// to (N-1)×CA_CUTOFF_LOOKUP_TIMEOUT worst case.
///
/// Mutation check: remove the `break;` in the `if !matched { ... }`
/// block → miss count becomes 4 → test fails on the metric assert.
#[tokio::test]
async fn ca_compare_short_circuits_on_first_miss() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let test_db = TestDb::new(&MIGRATOR).await;
    let (_store, store_client, _store_h) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));
    let _db = test_db;

    let _rx = connect_executor(&handle, "sc-worker", "x86_64-linux", 4).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-shortcircuit");
    let mut node = make_test_node("ca-shortcircuit", "x86_64-linux");
    node.is_content_addressed = true;
    node.ca_modular_hash = vec![0xDD; 32];
    node.output_names = vec!["out".into(), "dev".into(), "doc".into(), "man".into()];
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let skip_key = "rio_scheduler_ca_hash_compares_total{outcome=skipped_after_miss}";
    let miss_before = recorder.get(miss_key);
    let skip_before = recorder.get(skip_key);

    // Four outputs, none with a prior realisation seeded. output[0]
    // → miss → break. outputs[1..3] would ALSO miss but should
    // never be queried.
    complete_ca(
        &handle,
        "sc-worker",
        &drv_path,
        &[
            ("out", &test_store_path("ca-sc-out"), vec![0x10; 32]),
            ("dev", &test_store_path("ca-sc-dev"), vec![0x11; 32]),
            ("doc", &test_store_path("ca-sc-doc"), vec![0x12; 32]),
            ("man", &test_store_path("ca-sc-man"), vec![0x13; 32]),
        ],
    )
    .await?;

    let info = handle
        .debug_query_derivation("ca-shortcircuit")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "first-output miss → AND-fold false"
    );

    // LOAD-BEARING: 1 miss + 3 skipped_after_miss. If the break is
    // removed, miss=4 and skipped=0.
    assert_eq!(
        recorder.get(miss_key) - miss_before,
        1,
        "1 miss (output[0]).\nCounters: {:#?}",
        recorder.all_keys()
    );
    assert_eq!(
        recorder.get(skip_key) - skip_before,
        3,
        "3 skipped_after_miss (outputs[1..3]).\nCounters: {:#?}",
        recorder.all_keys()
    );

    Ok(())
}

/// TransientFailure: retry on a different worker up to max_retries (default 2).
#[tokio::test]
async fn test_transient_retry_different_worker() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Register two workers
    let _rx1 = connect_executor(&handle, "worker-a", "x86_64-linux", 1).await?;
    let _rx2 = connect_executor(&handle, "worker-b", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let p_retry = test_drv_path("retry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "retry-hash", PriorityClass::Scheduled).await?;

    // Get initial worker assignment
    let info1 = handle
        .debug_query_derivation("retry-hash")
        .await?
        .expect("exists");
    let first_worker = info1
        .assigned_executor
        .clone()
        .expect("assigned to a worker");
    assert_eq!(info1.retry_count, 0);

    // Send TransientFailure from the first worker
    complete_failure(
        &handle,
        &first_worker,
        &p_retry,
        rio_proto::build_types::BuildResultStatus::TransientFailure,
        "network hiccup",
    )
    .await?;

    // Should be retried: retry_count=1. backoff_until is set so
    // the derivation stays Ready (not immediately re-dispatched).
    // failed_builders contains first_worker so when backoff elapses,
    // retry goes to the OTHER worker.
    let info2 = handle
        .debug_query_derivation("retry-hash")
        .await?
        .expect("exists");
    assert_eq!(
        info2.retry_count, 1,
        "transient failure should increment retry_count"
    );
    // Status: Ready with backoff_until set. NOT Assigned — backoff
    // blocks immediate re-dispatch. If it IS Assigned, dispatch
    // raced us between complete_failure and query — still fine for
    // the test, the worker must be different (failed_builders
    // exclusion).
    match info2.status {
        DerivationStatus::Ready => {
            // Backoff active: assigned_executor cleared.
            assert!(
                info2.assigned_executor.is_none(),
                "Ready after failure → assigned_executor cleared"
            );
        }
        DerivationStatus::Assigned => {
            // Raced: dispatch happened. Worker MUST be different
            // (failed_builders exclusion).
            let retry_worker = info2.assigned_executor.expect("Assigned → worker set");
            assert_ne!(
                retry_worker, first_worker,
                "failed_builders exclusion: retry must go to a DIFFERENT worker"
            );
        }
        other => panic!("expected Ready or Assigned, got {other:?}"),
    }
    Ok(())
}

/// max_retries (default 2) exhausted → poison (the `retry_count >=
/// max_retries` branch, distinct from POISON_THRESHOLD 3-distinct-
/// workers).
///
/// Uses `debug_force_assign` between failures to bypass backoff_until
/// and failed_builders exclusion (which correctly prevent immediate
/// same-worker retry). The test drives the state machine directly to
/// test the completion handler's max_retries logic, not dispatch.
#[tokio::test]
async fn test_transient_failure_max_retries_poisons() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("flaky-worker", "x86_64-linux", 1).await?;
    // Pad workers (non-matching system) so the all-workers-failed
    // clamp doesn't fire before max_retries — we're testing the
    // max_retries branch specifically, worker_count must exceed
    // threshold=3.
    let _rx2 = connect_executor(&handle, "mr-pad2", "aarch64-linux", 1).await?;
    let _rx3 = connect_executor(&handle, "mr-pad3", "aarch64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let p_maxretry = test_drv_path("maxretry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "maxretry-hash", PriorityClass::Scheduled).await?;

    // Default RetryPolicy::max_retries = 2. Fail 3 times:
    // retry_count 0 -> 1 (retry), 1 -> 2 (retry), 2 >= 2 -> Poisoned.
    //
    // debug_force_assign before each failure (except the first —
    // initial dispatch happened) bypasses backoff so the completion
    // handler sees Assigned state and processes the failure.
    for attempt in 0..3 {
        if attempt > 0 {
            // Force back to Assigned: backoff would block real
            // dispatch, and failed_builders now excludes flaky-worker.
            let ok = handle
                .debug_force_assign("maxretry-hash", "flaky-worker")
                .await?;
            assert!(ok, "force-assign should succeed for Ready derivation");
        }
        complete_failure(
            &handle,
            "flaky-worker",
            &p_maxretry,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("attempt {attempt} failed"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation("maxretry-hash")
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "3 transient failures (retry_count >= max_retries=2) should poison"
    );
    Ok(())
}

/// I-213: a transient failure that promotes `size_class_floor` does
/// NOT consume `max_retries`. Promotion is a sizing signal, bounded by
/// the ladder length; `max_retries` becomes "retries at the same size
/// class". Regression: firefox-unwrapped climbed tiny→small→medium and
/// poisoned at retry_count=2 with the ladder's `large`/`xlarge` never
/// tried.
// r[verify sched.retry.promotion-exempt]
#[tokio::test]
async fn test_transient_failure_promotion_exempt_from_max_retries() -> TestResult {
    use crate::assignment::SizeClassConfig;
    let db = TestDb::new(&MIGRATOR).await;
    let classes = ["tiny", "small", "medium", "large", "xlarge"];
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(
            classes
                .iter()
                .enumerate()
                .map(|(i, n)| SizeClassConfig {
                    name: (*n).into(),
                    cutoff_secs: 30.0 * (i + 1) as f64,
                    mem_limit_bytes: u64::MAX,
                    cpu_limit_cores: None,
                })
                .collect(),
        )
        .with_retry_policy(crate::RetryPolicy {
            backoff_base_secs: 0.0,
            ..Default::default()
        })
        // Disable distinct-worker poison so we test ONLY max_retries.
        .with_poison_config(crate::PoisonConfig {
            threshold: 99,
            ..Default::default()
        })
    });
    let mut rxs = Vec::new();
    for c in classes {
        rxs.push(connect_builder_classed(&handle, &format!("b-{c}"), "x86_64-linux", c).await?);
    }

    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ladder-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    let p = test_drv_path("ladder-drv");

    // Walk tiny→small→medium→large: each failure promotes,
    // retry_count stays 0.
    for (i, c) in classes[..4].iter().enumerate() {
        handle
            .debug_force_assign("ladder-drv", &format!("b-{c}"))
            .await?;
        complete_failure(
            &handle,
            &format!("b-{c}"),
            &p,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            "simulated OOM",
        )
        .await?;
        let s = handle
            .debug_query_derivation("ladder-drv")
            .await?
            .expect("exists");
        assert_eq!(
            s.size_class_floor.as_deref(),
            Some(classes[i + 1]),
            "failure on {c} → floor promoted to {}",
            classes[i + 1]
        );
        assert_eq!(
            s.retry_count, 0,
            "I-213: promotion-causing failure does NOT increment retry_count (after {c})"
        );
        assert_ne!(s.status, DerivationStatus::Poisoned);
    }

    // At xlarge (top of ladder): no promotion → retry_count
    // increments. After 3 same-tier failures (max_retries=2): poison.
    for attempt in 0..3 {
        handle.debug_force_assign("ladder-drv", "b-xlarge").await?;
        complete_failure(
            &handle,
            "b-xlarge",
            &p,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("xlarge attempt {attempt}"),
        )
        .await?;
    }
    let s = handle
        .debug_query_derivation("ladder-drv")
        .await?
        .expect("exists");
    assert_eq!(
        s.status,
        DerivationStatus::Poisoned,
        "max_retries applies once at top of ladder (no promotion)"
    );
    Ok(())
}

/// Derivation poisoned after POISON_THRESHOLD (3) distinct worker failures.
#[tokio::test]
async fn test_poison_threshold_after_distinct_workers() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Register 4 workers so the derivation can be re-dispatched after each failure.
    let _rx1 = connect_executor(&handle, "poison-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_executor(&handle, "poison-w2", "x86_64-linux", 1).await?;
    let _rx3 = connect_executor(&handle, "poison-w3", "x86_64-linux", 1).await?;
    let _rx4 = connect_executor(&handle, "poison-w4", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "poison-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Send TransientFailure from 3 DISTINCT workers. After the 3rd, poison.
    //
    // debug_force_assign before each failure: backoff_until prevents
    // immediate re-dispatch after each failure, AND the initial
    // dispatch picks any of the 4 workers (not necessarily w1).
    // Force-assign so the completion's executor_id matches
    // assigned_executor — the stale-report guard would otherwise drop
    // the completion. We're testing the poison-threshold logic
    // (3 distinct failed_builders → poisoned), not dispatch timing.
    for (i, worker) in ["poison-w1", "poison-w2", "poison-w3"].iter().enumerate() {
        let ok = handle.debug_force_assign(drv_hash, worker).await?;
        assert!(ok, "force-assign should succeed (iter {i})");
        complete_failure(
            &handle,
            worker,
            &drv_path,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "derivation should be Poisoned after {} distinct worker failures",
        PoisonConfig::default().threshold
    );

    // Build should be Failed.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after derivation is poisoned"
    );
    Ok(())
}

/// Starvation guard: 2-worker cluster (below configured threshold=3),
/// both fail → derivation POISONS via the worker_count clamp in
/// `handle_transient_failure`. Without the clamp, `failed_builders.len()
/// =2 < threshold=3` → not poisoned, but `best_executor` excludes both
/// → stuck in Ready forever. The clamp makes the effective threshold
/// `min(3, 2) = 2`, so poison fires after the 2nd distinct failure.
#[tokio::test]
async fn test_all_workers_failed_below_threshold_poisons() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Raise max_retries so we hit the worker_count clamp, not the
    // max_retries branch (default max_retries=2 would poison on the
    // 2nd failure via retry_count anyway — masking the clamp).
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |mut a| {
        a.retry_policy.max_retries = 10;
        a
    });
    let _db = db;

    // Exactly 2 workers — below PoisonConfig::default().threshold (3).
    let _rx1 = connect_executor(&handle, "starve-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_executor(&handle, "starve-w2", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "starve-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Fail on both workers. After 2nd: all live workers in
    // failed_builders → poison (clamp fires before threshold=3).
    // Force-assign before each failure so the completion's
    // executor_id matches assigned_executor (the stale-report guard
    // would otherwise drop mismatched completions).
    for (i, worker) in ["starve-w1", "starve-w2"].iter().enumerate() {
        let ok = handle.debug_force_assign(drv_hash, worker).await?;
        assert!(ok, "force-assign should succeed (iter {i})");
        complete_failure(
            &handle,
            worker,
            &drv_path,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("starve failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "2-worker cluster, both failed: should poison via worker_count \
         clamp (effective threshold = min(3, 2) = 2), not starve in Ready"
    );
    assert_eq!(
        info.failed_builders.len(),
        2,
        "both workers recorded in failed_builders"
    );
    Ok(())
}

/// I-065 regression: the original starvation guard checked
/// `self.executors.keys().all(...)` — ALL kinds. With 2 builders + 2
/// fetchers, a non-FOD drv that failed on both builders never tripped
/// it (fetchers aren't in failed_builders). Live: `diffutils-3.12.drv`
/// stuck `[Ready]` forever, autoscaler at min, no log signal. The fix
/// makes the predicate kind-aware AND adds a dispatch-time backstop.
#[tokio::test]
async fn test_fleet_exhaustion_is_kind_aware() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |mut a| {
        a.retry_policy.max_retries = 10;
        a
    });
    let _db = db;

    // 2 builders + 1 fetcher. The fetcher's presence is what broke the
    // pre-I-065 check: `executors.keys().all(in failed_builders)` was
    // false because the fetcher isn't in failed_builders, even though
    // the BUILDER fleet is exhausted.
    let _b1 = connect_executor(&handle, "exhaust-b1", "x86_64-linux", 1).await?;
    let _b2 = connect_executor(&handle, "exhaust-b2", "x86_64-linux", 1).await?;
    let _f1 = connect_executor_no_ack_kind(
        &handle,
        "exhaust-f1",
        "builtin",
        1,
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "exhaust-drv";
    let drv_path = test_drv_path(drv_hash);
    // merge_single_node makes a non-FOD drv (Builder kind).
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    for (i, worker) in ["exhaust-b1", "exhaust-b2"].iter().enumerate() {
        let ok = handle.debug_force_assign(drv_hash, worker).await?;
        assert!(ok, "force-assign should succeed (iter {i})");
        complete_failure(
            &handle,
            worker,
            &drv_path,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("exhaust failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "all kind-matching workers (2 builders) failed; the fetcher \
         is irrelevant. Pre-I-065 this stuck in Ready forever because \
         the fleet-exhaustion check counted the fetcher."
    );
    Ok(())
}

// r[verify sched.retry.per-worker-budget]
/// InfrastructureFailure is a worker-local problem (FUSE EIO, cgroup
/// setup fail, OOM-kill of the build process) — NOT the build's fault.
/// 3× InfrastructureFailure on distinct workers → failed_builders stays
/// EMPTY, derivation NOT poisoned. Contrast with the TransientFailure
/// test above, where 3 distinct failures → poison.
#[tokio::test]
async fn test_infrastructure_failure_does_not_count_toward_poison() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // 4 workers so re-dispatch always has a candidate.
    let _rx1 = connect_executor(&handle, "infra-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_executor(&handle, "infra-w2", "x86_64-linux", 1).await?;
    let _rx3 = connect_executor(&handle, "infra-w3", "x86_64-linux", 1).await?;
    let _rx4 = connect_executor(&handle, "infra-w4", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× InfrastructureFailure from distinct workers. In the
    // TransientFailure case this would poison; here it must not.
    // handle_infrastructure_failure does reset_to_ready WITHOUT
    // failed_builders insert and WITHOUT backoff — so immediate
    // re-dispatch to whatever worker wins. We drive via
    // debug_force_assign to make the per-worker assertion
    // deterministic AND so the completion's executor_id matches
    // assigned_executor (the stale-report guard drops mismatched
    // completions).
    for (i, worker) in ["infra-w1", "infra-w2", "infra-w3"].iter().enumerate() {
        let ok = handle.debug_force_assign(drv_hash, worker).await?;
        assert!(ok, "force-assign should succeed (iter {i})");
        complete_failure(
            &handle,
            worker,
            &drv_path,
            rio_proto::build_types::BuildResultStatus::InfrastructureFailure,
            &format!("infra failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    // Exit criterion: 3× InfrastructureFailure → failed_builders.is_empty()
    assert!(
        info.failed_builders.is_empty(),
        "InfrastructureFailure must NOT insert into failed_builders, got {:?}",
        info.failed_builders
    );
    assert_eq!(
        info.failure_count, 0,
        "InfrastructureFailure must NOT increment failure_count"
    );
    assert_eq!(
        info.retry_count, 0,
        "InfrastructureFailure must NOT increment retry_count (separate infra_retry_count tracks it)"
    );
    // Exit criterion: NOT poisoned. Ready-or-Assigned (no backoff →
    // immediate re-dispatch may have won the race).
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "3× InfrastructureFailure → NOT poisoned, got {:?}",
        info.status
    );

    // 4th attempt: now send TransientFailure. This DOES count — proving
    // the derivation is still live and the counting path still works.
    let ok = handle.debug_force_assign(drv_hash, "infra-w4").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "infra-w4",
        &drv_path,
        rio_proto::build_types::BuildResultStatus::TransientFailure,
        "now this one counts",
    )
    .await?;

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.failed_builders.len(),
        1,
        "1× TransientFailure after 3× InfrastructureFailure → exactly 1 failed worker"
    );
    assert_eq!(info.failure_count, 1);
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "1 failed worker < threshold(3), still not poisoned"
    );
    Ok(())
}

// r[verify sched.timeout.promote-on-exceed]
/// I-200: `TimedOut` promotes `size_class_floor` AND resets to Ready
/// (bounded by `max_timeout_retries`), then goes terminal `Cancelled`.
///
/// Before I-200, `TimedOut` went straight to Cancelled without
/// promoting — so a worker-side `daemon_timeout_secs` hit on tiny
/// gave up immediately instead of retrying on a larger class with a
/// longer deadline. Mutation check: revert `handle_timeout_failure`
/// to the pre-I-200 terminal-only path → first `assert_eq!(status,
/// Ready)` fails (gets Cancelled).
///
/// Shape mirrors `test_infrastructure_failure_max_infra_retries_
/// poisons` (cap behavior) + `builder_size_class_floor_skips_smaller`
/// (size-class setup): `max_timeout_retries=2` so the test walks
/// tiny→small→medium then terminals.
#[tokio::test]
async fn test_timeout_promotes_floor_then_cancels_at_cap() -> TestResult {
    use crate::assignment::SizeClassConfig;

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
            SizeClassConfig {
                name: "tiny".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 120.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            SizeClassConfig {
                name: "medium".into(),
                cutoff_secs: 600.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
        ])
        .with_retry_policy(crate::RetryPolicy {
            // 2 retries → walks tiny→small, small→medium, then
            // terminal on the 3rd TimedOut.
            max_timeout_retries: 2,
            ..Default::default()
        })
    });

    // Three classed builders. promote_size_class_floor reads the
    // FAILED executor's `size_class` to compute next_builder_class,
    // so each completion must come from the class the drv is on.
    let _t = connect_builder_classed(&handle, "to-tiny", "x86_64-linux", "tiny").await?;
    let _s = connect_builder_classed(&handle, "to-small", "x86_64-linux", "small").await?;
    let _m = connect_builder_classed(&handle, "to-medium", "x86_64-linux", "medium").await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "i200-timeout";
    let drv_path = test_drv_path(drv_hash);
    let _ev = merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // ── Retry 1: TimedOut on tiny → floor=small, status=Ready ──────
    let ok = handle.debug_force_assign(drv_hash, "to-tiny").await?;
    assert!(ok, "force-assign tiny should succeed");
    complete_failure(
        &handle,
        "to-tiny",
        &drv_path,
        rio_proto::build_types::BuildResultStatus::TimedOut,
        "build exceeded daemon_timeout_secs",
    )
    .await?;

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.size_class_floor.as_deref(),
        Some("small"),
        "I-200: TimedOut on tiny → floor promoted to small"
    );
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "I-200: TimedOut under cap → reset to Ready (NOT terminal Cancelled), got {:?}",
        info.status
    );
    assert_eq!(
        info.timeout_retry_count, 1,
        "timeout_retry_count incremented"
    );
    // Timeout MUST NOT eat other budgets.
    assert_eq!(
        info.retry_count, 0,
        "TimedOut must not consume transient budget"
    );
    assert_eq!(
        info.infra_retry_count, 0,
        "TimedOut must not consume infra budget"
    );
    assert!(
        info.failed_builders.is_empty(),
        "TimedOut is not per-worker — failed_builders stays empty"
    );

    // ── Retry 2: TimedOut on small → floor=medium, status=Ready ────
    let ok = handle.debug_force_assign(drv_hash, "to-small").await?;
    assert!(ok, "force-assign small should succeed");
    complete_failure(
        &handle,
        "to-small",
        &drv_path,
        rio_proto::build_types::BuildResultStatus::TimedOut,
        "build exceeded daemon_timeout_secs",
    )
    .await?;
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(info.size_class_floor.as_deref(), Some("medium"));
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "2nd TimedOut still under cap=2 → Ready, got {:?}",
        info.status
    );
    assert_eq!(info.timeout_retry_count, 2);

    // ── Cap exhausted: 3rd TimedOut on medium → terminal Cancelled ──
    // Floor still promoted (promote happens before cap check) so an
    // explicit resubmit would start at the next class — but there IS
    // no next class (medium is largest), so floor stays at medium.
    let ok = handle.debug_force_assign(drv_hash, "to-medium").await?;
    assert!(ok, "force-assign medium should succeed");
    complete_failure(
        &handle,
        "to-medium",
        &drv_path,
        rio_proto::build_types::BuildResultStatus::TimedOut,
        "build exceeded daemon_timeout_secs",
    )
    .await?;
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Cancelled,
        "3rd TimedOut at max_timeout_retries=2 → terminal Cancelled, got {:?}",
        info.status
    );
    assert_eq!(
        info.size_class_floor.as_deref(),
        Some("medium"),
        "promote ran but clamped at largest class (no demote)"
    );
    Ok(())
}

/// InfrastructureFailure hits `max_infra_retries` → poison. The cap
/// exists to convert a misclassified permanent failure (e.g. S3 auth
/// error reported as infra) into a visible poison instead of a hot
/// loop. Observed on EKS: 12 drvs × 146 dispatch cycles in 6 minutes
/// before manual intervention — each cycle re-ran the full build.
///
/// InfrastructureFailure has no backoff and doesn't touch
/// `failed_builders`, so the same worker is immediately re-eligible;
/// `debug_force_assign` here just makes the executor_id deterministic
/// (the stale-report guard drops completions whose executor_id
/// doesn't match `assigned_executor`).
#[tokio::test]
async fn test_infrastructure_failure_max_infra_retries_poisons() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("infra-cap-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-cap-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Default max_infra_retries = 10 (I-127). Fail 10 times:
    // infra_retry_count 0→1..9→10. On the 11th failure the cap
    // check (`>= max_infra_retries`) fires BEFORE reset_to_ready →
    // poison.
    //
    // Boundary: at attempt 9 (10th failure, infra_retry_count=9
    // going in) the drv is still Ready post-handling. At attempt 10
    // (11th) it poisons. Assert both sides of the boundary.
    for attempt in 0..10 {
        let ok = handle.debug_force_assign(drv_hash, "infra-cap-w").await?;
        assert!(ok, "force-assign should succeed at attempt {attempt}");
        complete_failure(
            &handle,
            "infra-cap-w",
            &drv_path,
            rio_proto::build_types::BuildResultStatus::InfrastructureFailure,
            &format!("infra attempt {attempt}"),
        )
        .await?;
    }

    // After 10 failures: infra_retry_count=10 but the drv is still
    // alive (cap check is >=, checked BEFORE increment — 9 < 10 on
    // the 10th entry, increment to 10, return Ready).
    let before = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert!(
        matches!(
            before.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "10 infra failures (infra_retry_count=10, cap=10) → still alive \
         (boundary: cap check is pre-increment), got {:?}",
        before.status
    );
    assert_eq!(before.infra_retry_count, 10);

    // 11th failure: infra_retry_count=10 >= max_infra_retries=10 → poison.
    let ok = handle.debug_force_assign(drv_hash, "infra-cap-w").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "infra-cap-w",
        &drv_path,
        rio_proto::build_types::BuildResultStatus::InfrastructureFailure,
        "infra attempt 10 (cap hit)",
    )
    .await?;

    let after = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        after.status,
        DerivationStatus::Poisoned,
        "11th infra failure (infra_retry_count=10 >= max_infra_retries=10) → poison"
    );
    // Confirm the infra path never touched the transient-failure
    // accounting (these stay at 0 the whole way through).
    assert!(after.failed_builders.is_empty());
    assert_eq!(after.retry_count, 0);
    assert_eq!(after.failure_count, 0);

    Ok(())
}

/// I-127: "concurrent PutPath" InfrastructureFailure is exempt from
/// the `max_infra_retries` cap. It means another builder is uploading
/// the SAME output — the drv succeeded; this worker lost the upload
/// race. Under shallow-1024x a leaked PutPath lock (I-125a) made 4
/// builders hit this in a row → poison at 99.7% on a fine drv.
///
/// Exemption is by error_msg substring (mirrors `is_concurrent_put_path`
/// in rio-builder/src/upload.rs). The drv stays Ready and
/// `infra_retry_count` does NOT increment, no matter how many times.
#[tokio::test]
async fn test_infrastructure_failure_concurrent_putpath_exempt() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("putpath-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "putpath-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Drive WELL past the cap (default 10) — 15 concurrent-PutPath
    // failures. None should count; the drv stays Ready throughout.
    for attempt in 0..15 {
        let ok = handle.debug_force_assign(drv_hash, "putpath-w").await?;
        assert!(ok, "force-assign should succeed at attempt {attempt}");
        complete_failure(
            &handle,
            "putpath-w",
            &drv_path,
            rio_proto::build_types::BuildResultStatus::InfrastructureFailure,
            "upload failed: concurrent PutPath in progress for this path; retry",
        )
        .await?;
    }

    let after = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert!(
        matches!(
            after.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "15× concurrent-PutPath infra failures → still alive (exempt from cap), got {:?}",
        after.status
    );
    assert_eq!(
        after.infra_retry_count, 0,
        "concurrent-PutPath must NOT increment infra_retry_count"
    );
    assert!(after.failed_builders.is_empty());
    assert_eq!(after.retry_count, 0);
    assert_eq!(after.failure_count, 0);

    // A NON-exempt infra failure after that DOES count — proves the
    // exemption is keyed on error_msg, not a blanket disable.
    let ok = handle.debug_force_assign(drv_hash, "putpath-w").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "putpath-w",
        &drv_path,
        rio_proto::build_types::BuildResultStatus::InfrastructureFailure,
        "FUSE EIO",
    )
    .await?;
    let after2 = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        after2.infra_retry_count, 1,
        "non-exempt infra failure after exempt ones → counter increments normally"
    );

    Ok(())
}

/// With `require_distinct_workers=false`, 3× TransientFailure on the
/// SAME worker → poisoned. Contrast with default distinct mode where
/// same-worker repeats don't count (HashSet insert is idempotent).
/// Primary use case: single-worker dev deployments, where 3 distinct
/// workers will never exist.
#[tokio::test]
async fn test_non_distinct_mode_counts_same_worker() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Configure: require_distinct_workers = false, threshold = 3.
    // Also max_retries = 5 so we hit the poison-threshold branch,
    // not the max_retries branch (default max_retries=2 would
    // poison at retry_count>=2, masking what we're testing).
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_poison_config(PoisonConfig {
            threshold: 3,
            require_distinct_workers: false,
        })
    });
    let _db = db;

    // 3 workers so the all-workers-failed clamp (min(threshold,
    // worker_count)) doesn't fire before we reach failure_count=3.
    // Pad workers use a non-matching system so dispatch stays on
    // solo-worker — they're just padding for worker_count >= threshold.
    let _rx = connect_executor(&handle, "solo-worker", "x86_64-linux", 1).await?;
    let _rx2 = connect_executor(&handle, "pad-w2", "aarch64-linux", 1).await?;
    let _rx3 = connect_executor(&handle, "pad-w3", "aarch64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "nondistinct-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× TransientFailure on the SAME worker. In default distinct
    // mode: failed_builders={solo-worker} stays at len()=1 < 3 → not
    // poisoned (blocked by max_retries instead). In non-distinct mode:
    // failure_count goes 1→2→3, and 3 >= threshold → poisoned.
    for i in 0..3 {
        if i > 0 {
            // Backoff + failed_builders exclusion block real dispatch
            // back to solo-worker. Force it.
            let ok = handle.debug_force_assign(drv_hash, "solo-worker").await?;
            assert!(ok, "force-assign should succeed");
        }
        complete_failure(
            &handle,
            "solo-worker",
            &drv_path,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("same-worker failure {i}"),
        )
        .await?;

        let info = handle
            .debug_query_derivation(drv_hash)
            .await?
            .expect("exists");
        // failed_builders (HashSet) stays at 1 — same worker every time.
        assert_eq!(
            info.failed_builders.len(),
            1,
            "HashSet: same worker inserted once, stays len()=1"
        );
        // failure_count increments regardless.
        assert_eq!(
            info.failure_count,
            i + 1,
            "non-distinct: failure_count increments unconditionally"
        );
    }

    // Exit criterion: 3× same-worker TransientFailure under
    // require_distinct_workers=false → POISONED.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "non-distinct mode: 3× same-worker failures → poisoned (failure_count={} >= threshold=3)",
        info.failure_count
    );
    Ok(())
}

/// Negative control for non-distinct mode: with DEFAULT config
/// (require_distinct_workers=true), 3× same-worker TransientFailure
/// does NOT poison via the threshold path — failed_builders.len()
/// stays at 1. (max_retries=2 default still poisons, but via a
/// different branch — this test isolates the distinct-workers logic
/// by raising max_retries.)
#[tokio::test]
async fn test_distinct_mode_same_worker_does_not_poison_via_threshold() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Default PoisonConfig (distinct=true, threshold=3) — but raise
    // max_retries so we can observe 3 same-worker failures WITHOUT
    // hitting max_retries. This is the control for the non-distinct
    // test above: same inputs, opposite config, opposite outcome.
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |mut a| {
        a.retry_policy.max_retries = 10;
        a
    });
    let _db = db;

    // 3 workers so the all-workers-failed clamp doesn't fire —
    // we're isolating the distinct-vs-nondistinct counting, not
    // the worker_count guard. Pad workers use a non-matching
    // system so dispatch stays on ctrl-worker.
    let _rx = connect_executor(&handle, "ctrl-worker", "x86_64-linux", 1).await?;
    let _rx2 = connect_executor(&handle, "ctrl-pad2", "aarch64-linux", 1).await?;
    let _rx3 = connect_executor(&handle, "ctrl-pad3", "aarch64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "ctrl-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    for i in 0..3 {
        if i > 0 {
            let ok = handle.debug_force_assign(drv_hash, "ctrl-worker").await?;
            assert!(ok);
        }
        complete_failure(
            &handle,
            "ctrl-worker",
            &drv_path,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("ctrl failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(info.failed_builders.len(), 1, "HashSet: same worker = 1");
    assert_eq!(info.failure_count, 3, "flat count still increments");
    // NOT poisoned: distinct mode uses failed_builders.len()=1 < 3.
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "distinct mode (default): 3× same-worker → NOT poisoned via threshold \
         (failed_builders.len()=1 < 3); would need 3 DISTINCT workers"
    );
    Ok(())
}

/// Completing a child releases its parent to Ready in a dependency chain.
#[tokio::test]
async fn test_dependency_chain_releases_parent() -> TestResult {
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("chain-worker", "x86_64-linux", 1).await?;

    // A depends on B. B is Ready (leaf), A is Queued.
    let build_id = Uuid::new_v4();
    let p_chain_a = test_drv_path("chainA");
    let p_chain_b = test_drv_path("chainB");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("chainA", "x86_64-linux"),
            make_test_node("chainB", "x86_64-linux"),
        ],
        vec![make_test_edge("chainA", "chainB")],
        false,
    )
    .await?;

    // B is dispatched first (leaf). A is Queued waiting for B.
    let info_a = handle
        .debug_query_derivation("chainA")
        .await?
        .expect("exists");
    assert_eq!(info_a.status, DerivationStatus::Queued);

    // Worker receives B's assignment.
    let assigned_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(assigned_path, p_chain_b);

    // Complete B. One-shot worker drains; connect a fresh one for A.
    complete_success_empty(&handle, "chain-worker", &p_chain_b).await?;
    let mut stream_rx = connect_executor(&handle, "chain-worker-2", "x86_64-linux", 1).await?;

    // A should now transition Queued -> Ready -> Assigned (dispatched).
    let info_a = handle
        .debug_query_derivation("chainA")
        .await?
        .expect("exists");
    assert!(
        matches!(
            info_a.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "A should be Ready or Assigned after B completes, got {:?}",
        info_a.status
    );

    // Fresh worker should receive A's assignment.
    let assigned_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(
        assigned_path, p_chain_a,
        "A should be dispatched after B completes"
    );
    Ok(())
}

/// Duplicate ProcessCompletion is an idempotent no-op.
#[tokio::test]
async fn test_duplicate_completion_idempotent() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("idem-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "idem-hash";
    let drv_path = test_drv_path(drv_hash);
    let mut event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Send completion TWICE.
    for _ in 0..2 {
        complete_success_empty(&handle, "idem-worker", &drv_path).await?;
    }

    // completed_count should be 1, not 2.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.completed_derivations, 1,
        "duplicate completion should not double-count"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should still succeed (idempotent)"
    );

    // Count BuildCompleted events: should be exactly 1 (not 2).
    // Drain available events without blocking.
    let mut completed_events = 0;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Completed(_))
        ) {
            completed_events += 1;
        }
    }
    assert_eq!(
        completed_events, 1,
        "BuildCompleted event should fire exactly once"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Completion edge cases: unknown drv, wrong state, unknown status
// ---------------------------------------------------------------------------

/// ProcessCompletion for a drv_key the actor has never seen → warn
/// + ignore. Could happen after stale worker reconnect with a build
/// from a previous scheduler generation.
#[tokio::test]
#[traced_test]
async fn test_completion_unknown_drv_key_ignored() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Completion for a drv that was never merged.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "ghost-worker".into(),
            drv_key: "never-existed-drv-hash".into(),
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    assert!(
        logs_contain("unknown derivation") || logs_contain("not in DAG"),
        "expected warn for unknown drv_key"
    );
    Ok(())
}

/// Completion for a drv in Ready (never dispatched) → warn + ignore.
/// A worker can't complete something it wasn't assigned.
#[tokio::test]
#[traced_test]
async fn test_completion_for_non_running_state_ignored() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge but DON'T connect a worker — drv stays Ready.
    let build_id = Uuid::new_v4();
    merge_single_node(&handle, build_id, "ready-drv", PriorityClass::Scheduled).await?;

    // Send completion. Drv is Ready, not Assigned/Running.
    complete_success_empty(&handle, "phantom-w", &test_drv_path("ready-drv")).await?;
    barrier(&handle).await;

    assert!(
        logs_contain("not in assigned") || logs_contain("unexpected state"),
        "expected warn for wrong-state completion"
    );

    // Drv still Ready (completion ignored).
    let info = handle
        .debug_query_derivation("ready-drv")
        .await?
        .expect("drv exists");
    assert!(
        !matches!(info.status, DerivationStatus::Completed),
        "completion from wrong state should be ignored, status={:?}",
        info.status
    );
    Ok(())
}

/// Unknown BuildResultStatus value (e.g. from a newer worker) → warn,
/// treat as transient failure. Don't panic, don't get stuck.
#[tokio::test]
#[traced_test]
async fn test_unknown_build_status_treated_as_transient() -> TestResult {
    let (_db, handle, _task, mut _rx) = setup_with_worker("unk-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("unk-status");
    merge_single_node(&handle, build_id, "unk-status", PriorityClass::Scheduled).await?;

    // Wait for dispatch (drv → Assigned).
    barrier(&handle).await;

    // Send completion with an invalid status int (9999).
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "unk-w".into(),
            drv_key: drv_path.clone(),
            result: rio_proto::build_types::BuildResult {
                status: 9999, // not a valid enum
                error_msg: "mystery".into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    assert!(
        logs_contain("unknown BuildResultStatus")
            || logs_contain("Unspecified")
            || logs_contain("unknown status"),
        "expected warn for unknown status enum"
    );
    Ok(())
}

/// After CancelBuild transitions a drv to Cancelled, a later
/// Cancelled completion from the worker is expected → no-op, debug
/// log. This is the "worker acknowledges the cancel signal" path.
#[tokio::test]
#[traced_test]
async fn test_cancelled_completion_after_cancel_is_noop() -> TestResult {
    let (_db, handle, _task, mut _rx) = setup_with_worker("cancel-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("cancel-drv");
    merge_single_node(&handle, build_id, "cancel-drv", PriorityClass::Scheduled).await?;
    barrier(&handle).await; // dispatch

    // Cancel the build (transitions drv → Cancelled, sends CancelSignal).
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "user request".into(),
            reply: reply_tx,
        })
        .await?;
    let _cancelled = reply_rx.await??;

    // Worker reports Cancelled (acknowledging the signal).
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "cancel-w".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Cancelled.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    // Drv still Cancelled (no spurious state change).
    let info = handle
        .debug_query_derivation("cancel-drv")
        .await?
        .expect("drv exists");
    assert_eq!(info.status, DerivationStatus::Cancelled);

    // Cancelled has its own early-return (debug, not warn). The
    // generic "not in assigned/running" warn must NOT fire — that's
    // the spurious-WARN-per-cancel this guards against.
    assert!(
        logs_contain("cancelled completion report (expected after CancelSignal)"),
        "expected Cancelled-specific early-return"
    );
    assert!(
        !logs_contain("not in assigned/running state"),
        "spurious WARN fired for expected Cancelled report"
    );
    Ok(())
}

// r[verify sched.classify.penalty-overwrite]
/// Misclassification detection: a build routed to "small" (30s cutoff)
/// that completes in 100s (> 2× cutoff) triggers the penalty-overwrite
/// branch (completion.rs:303-324). The detection RECORDS the misclass
/// (metric + EMA overwrite) — next classify() for this pname sees 100s
/// and picks "large".
///
/// dispatch.rs:test_size_class_routing_respects_classification proves
/// `assigned_size_class` is recorded; this proves the detection USES it.
#[tokio::test]
#[traced_test]
async fn test_misclass_detection_on_slow_completion() -> TestResult {
    use crate::assignment::SizeClassConfig;

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            SizeClassConfig {
                name: "large".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
        ])
    });

    // Small worker. No EMA pre-seed → classify() defaults to 30s →
    // routes to "small". Exactly the scenario: something LOOKED small,
    // but wasn't.
    let (tx, mut rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "w-small".into(),
            stream_tx: tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            size_class: Some("small".into()),
            executor_id: "w-small".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;

    // Merge with pname — the EMA-update block (completion.rs:247-249)
    // gates on pname.is_some(). Without it, misclass detection never fires.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("misc-drv", "x86_64-linux");
    node.pname = "slowthing".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Dispatch → assigned_size_class="small" set on the state.
    let _ = recv_assignment(&mut rx).await;

    // Complete with duration=100s (start=1000, stop=1100 unix seconds).
    // 100 > 2×30 = 60 → misclass branch fires.
    let drv_path = test_drv_path("misc-drv");
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "w-small".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: "/nix/store/zzz-slowthing-out".into(),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 1000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 1100,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    // THE ASSERTION: the warn! in completion.rs:308-315 fired. If the
    // detection branch was dead code (missing assigned_size_class, missing
    // pname, threshold wrong), this would fail.
    assert!(
        logs_contain("misclassification"),
        "expected 'misclassification: actual > 2× cutoff' warn log; \
         detection branch at completion.rs:303-324 should have fired \
         (duration=100s > 2×30s=60s small-class cutoff)"
    );

    // build_history now has the penalty-overwritten EMA. The overwrite
    // uses actual duration directly (no blend), so ema_duration_secs ≈100.
    // ≥ 60 is enough to prove the write happened (update_build_history's
    // normal blend would give 0.3*100+0.7*30 = 51 for the FIRST sample…
    // but actually the first sample has no prior to blend with — let me
    // just check > 2×cutoff, which distinguishes penalty from no-penalty).
    let ema: f64 =
        sqlx::query_scalar("SELECT ema_duration_secs FROM build_history WHERE pname = 'slowthing'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        ema > 60.0,
        "penalty-overwrite should have written actual duration (≈100s), \
         not the blended EMA; got {ema}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// build_samples write on completion (CutoffRebalancer feed)
// ---------------------------------------------------------------------------

/// Success completion with valid (pname, start_time, stop_time) writes
/// exactly one row to build_samples with the correct (pname, system,
/// duration_secs). Feeds P0229's CutoffRebalancer.
///
/// Gate conditions from completion.rs:283-296:
///   - state.pname.is_some()        ← node.pname set below
///   - result.start_time.is_some()  ← set below
///   - result.stop_time.is_some()   ← set below
///   - 0 < duration_secs < 30 days  ← 5.25s, trivially in-range
///
/// Without ALL of these, insert_build_sample is never reached — the
/// complete_success_empty() helper (no timestamps) silently skips.
#[tokio::test]
async fn test_completion_writes_build_sample() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("bs-worker", "x86_64-linux", 1).await?;

    // Merge with a distinct pname — make_test_node defaults to
    // "test-pkg", which is fine, but a unique pname makes the
    // SELECT below unambiguous if other tests ever share the pool.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("bs-drv", "x86_64-linux");
    node.pname = "sample-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Receive assignment — proves dispatch happened (state → Assigned).
    let _assignment = recv_assignment(&mut stream_rx).await;

    // Precondition: build_samples is empty. Test asserts its own
    // precondition so a stale row from a future test-helper change
    // would fail here, not in the COUNT=1 check below ("proves
    // nothing" self-invalidation guard).
    let pre: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_samples")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        pre, 0,
        "precondition: build_samples empty before completion"
    );

    // Complete with start=2000.0s, stop=2005.25s → duration=5.25s.
    // peak_memory_bytes=8 MiB, non-zero so it's the interesting case
    // (0 is also valid for build_samples, but non-zero proves the
    // value round-trips).
    let drv_path = test_drv_path("bs-drv");
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "bs-worker".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("sample-pkg-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 2000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 2005,
                    nanos: 250_000_000,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 8 * 1024 * 1024,
            output_size_bytes: 4096,
            peak_cpu_cores: 1.5,
        })
        .await?;
    barrier(&handle).await;

    // Exit criterion: exactly 1 row, correct (pname, system).
    let rows: Vec<(String, String, f64, i64)> =
        sqlx::query_as("SELECT pname, system, duration_secs, peak_memory_bytes FROM build_samples")
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        rows.len(),
        1,
        "exactly one build_samples row per successful completion"
    );
    let (pname, system, dur, mem) = &rows[0];
    assert_eq!(pname, "sample-pkg");
    assert_eq!(system, "x86_64-linux");
    // duration_secs = stop - start = 2005.25 - 2000.0 = 5.25.
    // f64 arithmetic on exactly-representable values (powers of 2):
    // 0.25 = 2^-2, 5.0 = 5, both exact. But tolerance anyway.
    assert!(
        (dur - 5.25).abs() < 1e-9,
        "duration_secs should be 5.25s (stop=2005.25 - start=2000.0), got {dur}"
    );
    assert_eq!(
        *mem,
        8 * 1024 * 1024,
        "peak_memory_bytes round-trips as i64"
    );

    Ok(())
}

/// Negative: completion WITHOUT timestamps (the complete_success_empty
/// path) writes nothing to build_samples. The sanity gate at
/// completion.rs:285 `let (Some(start), Some(stop)) = ...` rejects
/// Default::default() timestamps (None, None). Proves the gate is
/// live — if someone removes it, this test catches the regression
/// (spurious 0.0s samples would poison the rebalancer's percentiles).
#[tokio::test]
async fn test_completion_no_timestamps_no_sample() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("nt-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "nt-drv", PriorityClass::Scheduled).await?;
    let _assignment = recv_assignment(&mut stream_rx).await;

    // complete_success_empty: BuildResult::default() → start_time=None,
    // stop_time=None. The EMA block and build_samples write both gate
    // on these being Some.
    complete_success_empty(&handle, "nt-worker", &test_drv_path("nt-drv")).await?;
    barrier(&handle).await;

    // Build succeeded (completion processed)…
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "completion should succeed even without timestamps"
    );

    // …but build_samples is empty (gate rejected None timestamps).
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_samples")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        count, 0,
        "no timestamps → no build_samples row (sanity gate at completion.rs)"
    );

    Ok(())
}

/// peak_memory_bytes = u64::MAX clamps to i64::MAX, not a negative wrap.
///
/// Unclamped `u64::MAX as i64` → -1 (two's-complement wrap). A negative
/// peak_memory_bytes in build_samples poisons the CutoffRebalancer's
/// percentile computation. Clamp at completion.rs bounds to i64::MAX.
///
/// Physical RAM is well below 2^63 bytes (8 EiB), so this is defensive
/// against a misbehaving worker rather than a realistic cgroup reading.
#[tokio::test]
async fn test_completion_peak_memory_clamps_to_i64_max() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("clamp-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let mut node = make_test_node("clamp-drv", "x86_64-linux");
    node.pname = "clamp-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    let _assignment = recv_assignment(&mut stream_rx).await;

    let drv_path = test_drv_path("clamp-drv");
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "clamp-worker".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("clamp-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 3000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 3001,
                    nanos: 0,
                }),
                ..Default::default()
            },
            // The pathological input: u64::MAX from a misbehaving worker.
            // Unclamped cast wraps to -1i64.
            peak_memory_bytes: u64::MAX,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    let mem: i64 =
        sqlx::query_scalar("SELECT peak_memory_bytes FROM build_samples WHERE pname = 'clamp-pkg'")
            .fetch_one(&db.pool)
            .await?;

    // Invariant: clamp is a ceiling, never produces negative.
    assert!(mem >= 0, "clamp must never produce negative i64, got {mem}");
    // Exact expectation: u64::MAX > i64::MAX → clamps to i64::MAX.
    assert_eq!(
        mem,
        i64::MAX,
        "u64::MAX should clamp to i64::MAX; got {mem} \
         (unclamped cast wraps to -1)"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// path_tenants upsert on completion (per-tenant GC retention)
// ---------------------------------------------------------------------------

/// Two tenants submit the SAME derivation → dedup → 1 execution. On
/// completion, interested_builds = {both} → upsert inserts 2 rows
/// (same store_path_hash, distinct tenant_id). Re-call is idempotent
/// (ON CONFLICT DO NOTHING → rows_affected == 0).
// r[verify sched.gc.path-tenants-upsert]
#[tokio::test]
async fn test_completion_path_tenants_dedup_idempotent() -> TestResult {
    use sha2::Digest;

    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("pt-worker", "x86_64-linux", 1).await?;

    // ── Seed 2 tenants. FK path_tenants→tenants ON DELETE CASCADE
    // means these rows MUST exist before the upsert. ─────────────────
    let tenant_a = rio_test_support::seed_tenant(&db.pool, "pt-tenant-a").await;
    let tenant_b = rio_test_support::seed_tenant(&db.pool, "pt-tenant-b").await;

    // ── Part A: actor flow — 2 builds share 1 derivation → dedup ────
    // Both builds submit the same node (same drv_hash "pt-drv"). The
    // actor dedups on drv_hash: one DerivationState, interested_builds
    // = {build_a, build_b}. One dispatch, one completion.
    let drv_tag = "pt-drv";
    let drv_path = test_drv_path(drv_tag);
    let out_path = test_store_path("pt-out");

    for (build_id, tenant) in [(Uuid::new_v4(), tenant_a), (Uuid::new_v4(), tenant_b)] {
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                req: MergeDagRequest {
                    build_id,
                    tenant_id: Some(tenant),
                    priority_class: PriorityClass::Scheduled,
                    nodes: vec![make_test_node(drv_tag, "x86_64-linux")],
                    edges: vec![],
                    options: BuildOptions::default(),
                    keep_going: false,
                    traceparent: String::new(),
                    jti: None,
                    jwt_token: None,
                },
                reply: reply_tx,
            })
            .await?;
        let _rx = reply_rx.await??;
    }

    // ONE assignment (dedup proof). Second merge saw existing node,
    // just added build_b to interested_builds.
    let assignment = recv_assignment(&mut stream_rx).await;
    assert_eq!(assignment.drv_path, drv_path, "dedup: one dispatch");

    // Complete with a real output_path. complete_success sends
    // built_outputs[0].output_path → completion.rs:260 stores it on
    // state.output_paths → :365 reads it → :406 upserts.
    complete_success(&handle, "pt-worker", &drv_path, &out_path).await?;
    barrier(&handle).await;

    // ── Assertion: 2 rows for out_path's hash (one per tenant) ──────
    let out_hash = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
    let rows: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&out_hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        rows.len(),
        2,
        "completion hook should upsert 1 row per interested tenant; \
         got {} rows for out_path hash",
        rows.len()
    );
    assert!(
        rows.contains(&tenant_a),
        "tenant_a should be in path_tenants"
    );
    assert!(
        rows.contains(&tenant_b),
        "tenant_b should be in path_tenants"
    );

    // ── Part B: direct upsert — 3 paths × 2 tenants = 6 rows ────────
    // Exit-criterion shape: cartesian product fully materialized,
    // idempotent on re-call. Fresh paths (no overlap with Part A).
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());
    let paths = vec![
        test_store_path("pt-p1"),
        test_store_path("pt-p2"),
        test_store_path("pt-p3"),
    ];
    let tenants = vec![tenant_a, tenant_b];

    let first = sched_db.upsert_path_tenants(&paths, &tenants).await?;
    assert_eq!(first, 6, "3 paths × 2 tenants = 6 rows inserted");

    let second = sched_db.upsert_path_tenants(&paths, &tenants).await?;
    assert_eq!(
        second, 0,
        "re-call with same inputs → 0 new rows (ON CONFLICT DO NOTHING)"
    );

    // Total in table: 2 (Part A) + 6 (Part B) = 8.
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(total, 8, "2 from actor flow + 6 from direct call");

    Ok(())
}

// r[verify sched.db.assignment-terminal-on-status]
/// I-209: PermanentFailure MUST close the active `assignments` row
/// (`pending` → `failed`, `completed_at` set) and record the executor
/// in `derivations.failed_builders`. Pre-fix, only the success path
/// did the assignment write — every poisoned/cancelled derivation
/// kept a `pending` row, the pruner's `NOT EXISTS assignments` never
/// matched, and `derivations` leaked.
///
/// Part B proves the pruner can now actually delete: drop the
/// `build_derivations` link, run `gc_orphan_terminal_derivations`,
/// assert the row is gone (and the assignment row CASCADEd with it).
#[tokio::test]
async fn permanent_failure_terminals_assignment_and_records_executor() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("i209-w", "x86_64-linux", 1).await?;

    let _ev = merge_single_node(&handle, Uuid::new_v4(), "i209", PriorityClass::Scheduled).await?;
    let _ = recv_assignment(&mut rx).await;
    complete_failure(
        &handle,
        "i209-w",
        &test_drv_path("i209"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "deterministic compile error",
    )
    .await?;
    barrier(&handle).await;

    // ── Part A: PG bookkeeping ─────────────────────────────────────────
    let (assign_status, has_completed_at): (String, bool) = sqlx::query_as(
        "SELECT a.status, a.completed_at IS NOT NULL
         FROM assignments a JOIN derivations d USING (derivation_id)
         WHERE d.drv_hash = $1",
    )
    .bind("i209")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        assign_status, "failed",
        "I-209: assignment row closed (pending → failed) on poison"
    );
    assert!(has_completed_at, "completed_at stamped on terminal");

    let failed: Vec<String> =
        sqlx::query_scalar("SELECT failed_builders FROM derivations WHERE drv_hash = $1")
            .bind("i209")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        failed,
        vec!["i209-w".to_string()],
        "I-209: handle_permanent_failure records the executor"
    );

    // ── Part B: pruner is unblocked ────────────────────────────────────
    // Drop the build_derivations link (simulates the owning build's
    // cleanup) so the only remaining gate is the assignments row.
    sqlx::query(
        "DELETE FROM build_derivations WHERE derivation_id = \
                 (SELECT derivation_id FROM derivations WHERE drv_hash = $1)",
    )
    .bind("i209")
    .execute(&db.pool)
    .await?;

    let sched_db = SchedulerDb::new(db.pool.clone());
    let deleted = sched_db.gc_orphan_terminal_derivations(10).await?;
    assert!(
        deleted >= 1,
        "I-209: terminal assignment row no longer blocks the pruner"
    );

    let remaining: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM assignments a JOIN derivations d USING (derivation_id) \
         WHERE d.drv_hash = $1",
    )
    .bind("i209")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        remaining, 0,
        "034: CASCADE FK removed the assignment row with the derivation"
    );

    Ok(())
}
