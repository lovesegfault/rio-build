//! Completion handling: retry/poison thresholds, dep-chain release, duplicate idempotence.
// r[verify sched.completion.idempotent]
// r[verify sched.state.transitions]
// r[verify sched.state.terminal-idempotent]

use super::*;
use rstest::rstest;
use tracing_test::traced_test;

/// What to seed in the realisations table before driving a
/// [`CaFixture`] to completion. See [`ca_compare_edge_cases`].
enum CaSeed {
    /// No realisation rows — fresh first-ever build.
    None,
    /// Only this build's own `(f.modular_hash, out)` row — simulates
    /// `insert_realisation` having fired before the compare (it does).
    Own,
    /// Own row + a PRIOR build's row at the same path (different
    /// modular_hash) — the second-build positive case.
    OwnAndPrior,
}

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

    let info = expect_drv(&f.actor, "ca-race-guard").await;
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
        info.ca.output_unchanged,
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
    let pre = expect_drv(&f.actor, "ca-match").await;
    assert!(pre.ca.is_ca, "precondition: merged with is_ca=true");
    assert!(!pre.ca.output_unchanged, "default false before completion");

    let match_before = recorder.get(match_key);
    complete_ca(
        &f.actor,
        &f.executor_id,
        &f.drv_path,
        &[("out", &out_path, out_hash.to_vec())],
    )
    .await?;

    let info = expect_drv(&f.actor, "ca-match").await;
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        info.ca.output_unchanged,
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
    let _rx2 = connect_executor(&f.actor, "ca-w2", "x86_64-linux").await?;
    let mixed_modular: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(b"ca-fixture:ca-mixed").into()
    };
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-mixed");
    let mut node = make_node("ca-mixed");
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

    let info = expect_drv(&f.actor, "ca-mixed").await;
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca.output_unchanged,
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
    let _rx3 = connect_executor(&f.actor, "ca-w3", "x86_64-linux").await?;
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ia-skip");
    let node = make_node("ia-skip"); // is_content_addressed=false
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

    let info = expect_drv(&f.actor, "ia-skip").await;
    assert!(!info.ca.is_ca);
    assert!(
        !info.ca.output_unchanged,
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
/// CA-compare edge-case matrix: for each `(seed, outputs)` pair, the
/// compare must produce the expected `ca_output_unchanged` flag and
/// `{miss,match}` counter deltas. All cases share the
/// [`setup_ca_fixture`] preamble; only the realisation-table seed and
/// the reported `built_outputs` vary.
///
/// Load-bearing cases:
///
/// - **first_build** (bughunt-mc196): own row only → `query_prior_
///   realisation(path, exclude=own_modular)` filters it → miss. Mutation
///   check: drop `drv_hash != $2` predicate → unchanged flips true.
/// - **second_build**: own + prior row at same path → exclusion hides
///   self, finds sibling → match. Paired with first_build, proves the
///   exclusion isn't "any match → None".
/// - **zero_outputs**: `built_outputs=[]` → `!is_empty()` early-false →
///   loop body never runs → no counter increment, unchanged stays false.
/// - **empty_path**: `output_path=""` → guard short-circuits before PG
///   query → counted as miss explicitly.
/// - **no_prior**: nothing seeded, 2 outputs → first lookup → None →
///   miss → short-circuit break.
#[rstest]
// bughunt-mc196: own row only → self-exclusion → miss
#[case::first_build("ca-first", CaSeed::Own, vec![("out", "ca-first-out", 32)], false, Some(1), Some(0))]
// own + prior → finds sibling → match → unchanged=true
#[case::second_build("ca-second", CaSeed::OwnAndPrior, vec![("out", "ca-second-out", 32)], true, Some(0), Some(1))]
// built_outputs=[] → !is_empty() guard → no loop iteration
#[case::zero_outputs("ca-zero", CaSeed::None, vec![], false, Some(0), Some(0))]
// output_path="" → is_empty() guard short-circuits before PG
#[case::empty_path("ca-empty", CaSeed::None, vec![("out", "", 32)], false, None, None)]
// nothing seeded → Ok(None) → miss → break
#[case::no_prior("ca-noprior", CaSeed::None, vec![("out", "ca-noprior-out1", 32), ("dev", "ca-noprior-out2", 32)], false, Some(1), Some(0))]
#[tokio::test]
async fn ca_compare_edge_cases(
    #[case] key: &str,
    #[case] seed: CaSeed,
    #[case] outputs: Vec<(&str, &str, usize)>,
    #[case] expect_unchanged: bool,
    #[case] expect_miss: Option<u64>,
    #[case] expect_match: Option<u64>,
) -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let f = setup_ca_fixture(key).await?;

    // Build output tuples: empty path-tag means literal "".
    let outputs: Vec<_> = outputs
        .into_iter()
        .map(|(n, p, hlen)| {
            let path = if p.is_empty() {
                String::new()
            } else {
                test_store_path(p)
            };
            (n, path, vec![0x55u8; hlen])
        })
        .collect();

    // Seed realisations for the FIRST output (the one the compare hits).
    if let Some((name, path, hash)) = outputs.first() {
        let hash: [u8; 32] = hash.as_slice().try_into().unwrap();
        match seed {
            CaSeed::None => {}
            CaSeed::Own => {
                seed_realisation(&f.pool, &f.modular_hash, name, path, &hash).await?;
            }
            CaSeed::OwnAndPrior => {
                seed_realisation(&f.pool, &f.modular_hash, name, path, &hash).await?;
                seed_realisation(&f.pool, &[0x66; 32], name, path, &hash).await?;
            }
        }
    }

    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";
    let miss_before = recorder.get(miss_key);
    let match_before = recorder.get(match_key);

    let outputs_ref: Vec<_> = outputs
        .iter()
        .map(|(n, p, h)| (*n, p.as_str(), h.clone()))
        .collect();
    complete_ca(&f.actor, &f.executor_id, &f.drv_path, &outputs_ref).await?;

    let info = expect_drv(&f.actor, key).await;
    assert_eq!(info.status, DerivationStatus::Completed);
    assert_eq!(
        info.ca.output_unchanged, expect_unchanged,
        "ca_output_unchanged mismatch for {key}"
    );
    if let Some(m) = expect_miss {
        assert_eq!(recorder.get(miss_key) - miss_before, m, "miss delta");
    }
    if let Some(m) = expect_match {
        assert_eq!(recorder.get(match_key) - match_before, m, "match delta");
    }
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
    let f = setup_ca_fixture_configured("ca-slow", |c, _| c.grpc_timeout = Duration::from_secs(3))
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
        !info.ca.output_unchanged,
        "no prior → miss → ca_output_unchanged=false"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-propagate+2]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let pool = db.pool.clone();
    let _db = db;

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
    let mut node_a = make_node("verify-a");
    node_a.is_content_addressed = true;
    node_a.ca_modular_hash = a_modular.to_vec();
    node_a.pname = "verify-a".into();
    let mut node_b = make_node("verify-b");
    node_b.is_content_addressed = true;
    node_b.ca_modular_hash = b_modular.to_vec();
    node_b.pname = "verify-b".into();
    let mut node_c = make_node("verify-c");
    node_c.is_content_addressed = true;
    node_c.ca_modular_hash = c_modular.to_vec();
    node_c.pname = "verify-c".into();

    let _rx = connect_executor(&handle, "verify-worker", "x86_64-linux").await?;

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
        expect_drv(&handle, "verify-b").await.status,
        DerivationStatus::Queued
    );
    assert_eq!(
        expect_drv(&handle, "verify-c").await.status,
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

    let info_a = expect_drv(&handle, "verify-a").await;
    assert!(
        info_a.ca.output_unchanged,
        "precondition: A matched prior realisation → ca_output_unchanged=true"
    );

    // B: Skipped (realisation_deps walk found b_prior, output
    // present in store → verified). Also stamped with output_paths.
    let info_b = expect_drv(&handle, "verify-b").await;
    assert_eq!(
        info_b.status,
        DerivationStatus::Skipped,
        "B found in realisation_deps walk + output present → Skipped"
    );

    // C: NOT Skipped (no prior C in realisation_deps → walk didn't
    // find it → verify rejects). Mutation `|_| true` would Skip C.
    let info_c = expect_drv(&handle, "verify-c").await;
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

    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;

    let _rx = connect_executor(&handle, "sc-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-shortcircuit");
    let mut node = make_node("ca-shortcircuit");
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

    let info = expect_drv(&handle, "ca-shortcircuit").await;
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca.output_unchanged,
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
    let _rx1 = connect_executor(&handle, "worker-a", "x86_64-linux").await?;
    let _rx2 = connect_executor(&handle, "worker-b", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let p_retry = test_drv_path("retry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "retry-hash", PriorityClass::Scheduled).await?;

    // Get initial worker assignment
    let info1 = expect_drv(&handle, "retry-hash").await;
    let first_worker = info1
        .assigned_executor
        .clone()
        .expect("assigned to a worker");
    assert_eq!(info1.retry.count, 0);

    // Send TransientFailure from the first worker
    complete_failure(
        &handle,
        &first_worker,
        &p_retry,
        rio_proto::types::BuildResultStatus::TransientFailure,
        "network hiccup",
    )
    .await?;

    // Should be retried: retry_count=1. backoff_until is set so
    // the derivation stays Ready (not immediately re-dispatched).
    // failed_builders contains first_worker so when backoff elapses,
    // retry goes to the OTHER worker.
    let info2 = expect_drv(&handle, "retry-hash").await;
    assert_eq!(
        info2.retry.count, 1,
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
    let (_db, handle, _task, _rx) = setup_with_worker("flaky-worker", "x86_64-linux").await?;
    // Pad workers (non-matching system) so the all-workers-failed
    // clamp doesn't fire before max_retries — we're testing the
    // max_retries branch specifically, worker_count must exceed
    // threshold=3.
    let _rx2 = connect_executor(&handle, "mr-pad2", "aarch64-linux").await?;
    let _rx3 = connect_executor(&handle, "mr-pad3", "aarch64-linux").await?;

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
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("attempt {attempt} failed"),
        )
        .await?;
    }

    let info = expect_drv(&handle, "maxretry-hash").await;
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "3 transient failures (retry_count >= max_retries=2) should poison"
    );
    Ok(())
}

/// I-213 + D4: worker-reported `CgroupOom` (build child hit cgroup
/// memory.max while pod survived → `InfrastructureFailure` with
/// "cgroup OOM …" message) doubles `resource_floor.mem_bytes` and
/// does NOT consume `max_infra_retries` for the climb itself
/// (`promoted=true` → `exempt_from_cap=true`). Doubling is bounded by
/// `Ceilings.max_mem`, not the retry budget. Regression:
/// firefox-unwrapped climbed tiny→small→medium and poisoned at
/// retry_count=2 with `large`/`xlarge` never tried.
///
/// `TransientFailure` (build script exited nonzero) does NOT promote
/// — that's a build-determinism signal. The previous test used
/// TransientFailure to drive the ladder; under the controller-
/// reports-reason design that's wrong. CgroupOom is the worker-
/// reported sizing signal (pod-level OOMKilled is controller-
/// reported via `ReportExecutorTermination`).
// r[verify sched.retry.promotion-exempt+2]
// r[verify sched.sla.reactive-floor]
#[tokio::test]
async fn test_transient_failure_promotion_exempt_from_max_retries() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let names = ["tiny", "small", "medium", "large", "xlarge"];
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy = crate::RetryPolicy {
            backoff_base_secs: 0.0,
            ..Default::default()
        };
        // Disable distinct-worker poison so we test ONLY max_retries.
        c.poison = crate::PoisonConfig {
            threshold: 99,
            ..Default::default()
        };
        // 256 GiB ceiling so 5× doublings (2→4→8→16→32 GiB) stay below
        // max_mem; the test asserts each rung promoted, not capped.
        c.sla = crate::actor::tests::test_sla_config();
    });
    let mut rxs = Vec::new();
    for n in names {
        rxs.push(connect_builder(&handle, &format!("b-{n}"), "x86_64-linux").await?);
    }

    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ladder-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    let p = test_drv_path("ladder-drv");

    // Seed est_memory_bytes (the doubling base).
    handle
        .debug_seed_sched_hint("ladder-drv", Some(2 << 30), None, None, None)
        .await?;

    // Walk via worker-reported CgroupOom (InfrastructureFailure with
    // the CgroupOom error string): each doubles mem floor;
    // retry_count (transient budget) and infra_count stay 0
    // (promoted=true → exempt_from_cap).
    let mut prev_mem = 0u64;
    for c in &names[..4] {
        handle
            .debug_force_assign("ladder-drv", &format!("b-{c}"))
            .await?;
        complete_failure(
            &handle,
            &format!("b-{c}"),
            &p,
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            "cgroup OOM during build; promoting size class",
        )
        .await?;
        let s = expect_drv(&handle, "ladder-drv").await;
        assert!(
            s.sched.resource_floor.mem_bytes > prev_mem,
            "CgroupOom on {c} → mem floor doubled (was {prev_mem}, now {})",
            s.sched.resource_floor.mem_bytes
        );
        prev_mem = s.sched.resource_floor.mem_bytes;
        assert_eq!(
            s.retry.count, 0,
            "InfrastructureFailure does NOT consume transient budget (after {c})"
        );
        assert_eq!(
            s.retry.infra_count, 0,
            "D4: promoted=true → exempt_from_cap → infra_count stays 0 (after {c})"
        );
        assert_ne!(s.status, DerivationStatus::Poisoned);
    }
    // D2: dispatch_ready refreshes est_memory_bytes from
    // solve_intent_for after each completion (clamped at floor), so
    // the doubling base picks up the probe default. The loop above
    // already proved monotone increase + budget exemption; the exact
    // ladder rungs depend on probe defaults.
    assert!(prev_mem >= 32 << 30, "≥4 doublings from a 2GiB seed");

    // Sanity: a non-OOM InfrastructureFailure does NOT bump
    // (the over-broad I-199 promote is gone).
    handle.debug_force_assign("ladder-drv", "b-xlarge").await?;
    complete_failure(
        &handle,
        "b-xlarge",
        &p,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        "FUSE EIO: store unreachable",
    )
    .await?;
    let s = expect_drv(&handle, "ladder-drv").await;
    assert_eq!(
        s.sched.resource_floor.mem_bytes, prev_mem,
        "non-OOM InfrastructureFailure (FUSE EIO) must NOT bump"
    );

    // At xlarge (top of ladder): TransientFailure (build script
    // exited nonzero — build-determinism signal). After 3 (max_
    // retries=2): poison. TransientFailure NEVER promotes.
    for attempt in 0..3 {
        handle.debug_force_assign("ladder-drv", "b-xlarge").await?;
        complete_failure(
            &handle,
            "b-xlarge",
            &p,
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("xlarge attempt {attempt}"),
        )
        .await?;
    }
    let s = expect_drv(&handle, "ladder-drv").await;
    assert_eq!(
        s.status,
        DerivationStatus::Poisoned,
        "max_retries applies once at top of ladder (no promotion)"
    );
    Ok(())
}

/// Distinct-worker poison-threshold matrix. N TransientFailures on
/// distinct workers → Poisoned, via three branches:
///
/// - **threshold**: 3 distinct of 4 workers → `failed_builders.len()
///   ≥ POISON_THRESHOLD` → poison.
/// - **clamp**: 2 distinct of 2 workers (below threshold=3) → poison
///   via `worker_count` clamp `min(3,2)=2`. Without clamp, would
///   starve Ready forever (best_executor excludes both).
/// - **kind_aware** (I-065): 2 distinct builders + 1 fetcher present.
///   Pre-fix `executors.keys().all(in failed_builders)` was false
///   (fetcher not in set) → diffutils-3.12.drv stuck Ready forever.
///   Fix: predicate is kind-aware.
///
/// `max_retries=10` for clamp/kind_aware so the `retry_count>=2`
/// branch doesn't mask the threshold/clamp under test.
#[rstest]
#[case::threshold(false, 4, &["pt-w1", "pt-w2", "pt-w3"], false)]
#[case::clamp(true, 2, &["pt-w1", "pt-w2"], false)]
#[case::kind_aware(true, 2, &["pt-w1", "pt-w2"], true)]
#[tokio::test]
async fn test_distinct_transient_poison_matrix(
    #[case] raise_max_retries: bool,
    #[case] n_builders: usize,
    #[case] fail_on: &[&str],
    #[case] add_fetcher: bool,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        if raise_max_retries {
            c.retry_policy.max_retries = 10;
        }
    });
    let _db = db;

    let mut _rxs = Vec::new();
    for i in 1..=n_builders {
        _rxs.push(connect_executor(&handle, &format!("pt-w{i}"), "x86_64-linux").await?);
    }
    if add_fetcher {
        // Fetcher's presence is what broke pre-I-065: not in failed_builders.
        _rxs.push(
            connect_executor_no_ack_kind(
                &handle,
                "pt-fetcher",
                "builtin",
                rio_proto::types::ExecutorKind::Fetcher,
            )
            .await?,
        );
    }

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "pt-drv", PriorityClass::Scheduled).await?;

    fail_on_workers(
        &handle,
        "pt-drv",
        rio_proto::types::BuildResultStatus::TransientFailure,
        fail_on,
    )
    .await?;

    let info = expect_drv(&handle, "pt-drv").await;
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "{} distinct TransientFailures → Poisoned (failed_builders={:?})",
        fail_on.len(),
        info.retry.failed_builders
    );
    assert_eq!(info.retry.failed_builders.len(), fail_on.len());
    assert_eq!(
        query_status(&handle, build_id).await?.state,
        rio_proto::types::BuildState::Failed as i32
    );
    Ok(())
}

// r[verify sched.retry.per-executor-budget]
/// InfrastructureFailure is a worker-local problem (FUSE EIO, cgroup
/// setup fail, OOM-kill of the build process) — NOT the build's fault.
/// 3× InfrastructureFailure on distinct workers → failed_builders stays
/// EMPTY, derivation NOT poisoned. Contrast with the TransientFailure
/// test above, where 3 distinct failures → poison.
#[tokio::test]
async fn test_infrastructure_failure_does_not_count_toward_poison() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // 4 workers so re-dispatch always has a candidate.
    let _rxs = connect_n_executors(&handle, "infra-w", "x86_64-linux", 4).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-drv";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× InfrastructureFailure from distinct workers. TransientFailure
    // would poison; here it must not (reset_to_ready WITHOUT
    // failed_builders insert / backoff).
    fail_on_workers(
        &handle,
        drv_hash,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        &["infra-w1", "infra-w2", "infra-w3"],
    )
    .await?;

    let info = expect_drv(&handle, drv_hash).await;
    // Exit criterion: 3× InfrastructureFailure → failed_builders.is_empty()
    assert!(
        info.retry.failed_builders.is_empty(),
        "InfrastructureFailure must NOT insert into failed_builders, got {:?}",
        info.retry.failed_builders
    );
    assert_eq!(
        info.retry.failure_count, 0,
        "InfrastructureFailure must NOT increment failure_count"
    );
    assert_eq!(
        info.retry.count, 0,
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
    fail_on_workers(
        &handle,
        drv_hash,
        rio_proto::types::BuildResultStatus::TransientFailure,
        &["infra-w4"],
    )
    .await?;

    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.retry.failed_builders.len(),
        1,
        "1× TransientFailure after 3× InfrastructureFailure → exactly 1 failed worker"
    );
    assert_eq!(info.retry.failure_count, 1);
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "1 failed worker < threshold(3), still not poisoned"
    );
    Ok(())
}

// r[verify sched.timeout.promote-on-exceed+2]
/// I-200: `TimedOut` promotes `resource_floor` AND resets to Ready
/// (bounded by `max_timeout_retries`), then goes terminal `Cancelled`.
///
/// Before I-200, `TimedOut` went straight to Cancelled without
/// promoting — so a worker-side `daemon_timeout_secs` hit on a small
/// pod gave up immediately instead of retrying with more resources
/// and a longer deadline. Mutation check: revert
/// `handle_timeout_failure` to the pre-I-200 terminal-only path →
/// first `assert_eq!(status, Ready)` fails (gets Cancelled).
///
/// Shape mirrors `test_infrastructure_failure_max_infra_retries_
/// poisons` (cap behavior): `max_timeout_retries=2` so the test walks
/// two floor doublings then terminals.
#[tokio::test]
async fn test_timeout_promotes_floor_then_cancels_at_cap() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        // 2 retries → walks tiny→small, small→medium, then terminal on 3rd TimedOut.
        c.retry_policy = crate::RetryPolicy {
            max_timeout_retries: 2,
            ..Default::default()
        };
    });

    // D4: bump_floor_or_count reads est_deadline_secs as the doubling
    // base; class is irrelevant.
    let _t = connect_builder(&handle, "to-tiny", "x86_64-linux").await?;
    let _s = connect_builder(&handle, "to-small", "x86_64-linux").await?;
    let _m = connect_builder(&handle, "to-medium", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "i200-timeout";
    let drv_path = test_drv_path(drv_hash);
    let _ev = merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    handle
        .debug_seed_sched_hint(drv_hash, None, None, Some(300), None)
        .await?;

    // ── Retry 1: TimedOut → floor.deadline=600, status=Ready ──────
    let ok = handle.debug_force_assign(drv_hash, "to-tiny").await?;
    assert!(ok, "force-assign tiny should succeed");
    complete_failure(
        &handle,
        "to-tiny",
        &drv_path,
        rio_proto::types::BuildResultStatus::TimedOut,
        "build exceeded daemon_timeout_secs",
    )
    .await?;

    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.sched.resource_floor.deadline_secs, 600,
        "I-200: TimedOut → deadline floor doubled (300→600)"
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
        info.retry.timeout_count, 1,
        "timeout_retry_count incremented"
    );
    // Timeout MUST NOT eat other budgets.
    assert_eq!(
        info.retry.count, 0,
        "TimedOut must not consume transient budget"
    );
    assert_eq!(
        info.retry.infra_count, 0,
        "TimedOut must not consume infra budget"
    );
    assert!(
        info.retry.failed_builders.is_empty(),
        "TimedOut is not per-worker — failed_builders stays empty"
    );

    // ── Retry 2: TimedOut on small → floor doubled again, Ready ────
    // D2: dispatch_ready (triggered by completion above) overwrote
    // est_deadline_secs from solve_intent_for. Re-seed to keep the
    // doubling base under test control.
    handle
        .debug_seed_sched_hint(drv_hash, None, None, Some(600), None)
        .await?;
    let ok = handle.debug_force_assign(drv_hash, "to-small").await?;
    assert!(ok, "force-assign small should succeed");
    complete_failure(
        &handle,
        "to-small",
        &drv_path,
        rio_proto::types::BuildResultStatus::TimedOut,
        "build exceeded daemon_timeout_secs",
    )
    .await?;
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(info.sched.resource_floor.deadline_secs, 1200);
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "2nd TimedOut still under cap=2 → Ready, got {:?}",
        info.status
    );
    assert_eq!(info.retry.timeout_count, 2);

    // ── Cap exhausted: 3rd TimedOut on medium → terminal Cancelled ──
    // Floor still promoted (promote happens before cap check) so an
    // explicit resubmit would start higher.
    handle
        .debug_seed_sched_hint(drv_hash, None, None, Some(1200), None)
        .await?;
    let ok = handle.debug_force_assign(drv_hash, "to-medium").await?;
    assert!(ok, "force-assign medium should succeed");
    complete_failure(
        &handle,
        "to-medium",
        &drv_path,
        rio_proto::types::BuildResultStatus::TimedOut,
        "build exceeded daemon_timeout_secs",
    )
    .await?;
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.status,
        DerivationStatus::Cancelled,
        "3rd TimedOut at max_timeout_retries=2 → terminal Cancelled, got {:?}",
        info.status
    );
    assert_eq!(
        info.sched.resource_floor.deadline_secs, 2400,
        "bump ran on terminal path too (so explicit resubmit starts higher)"
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
    let (_db, handle, _task, _rx) = setup_with_worker("infra-cap-w", "x86_64-linux").await?;

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
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            &format!("infra attempt {attempt}"),
        )
        .await?;
    }

    // After 10 failures: infra_retry_count=10 but the drv is still
    // alive (cap check is >=, checked BEFORE increment — 9 < 10 on
    // the 10th entry, increment to 10, return Ready).
    let before = expect_drv(&handle, drv_hash).await;
    assert!(
        matches!(
            before.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "10 infra failures (infra_retry_count=10, cap=10) → still alive \
         (boundary: cap check is pre-increment), got {:?}",
        before.status
    );
    assert_eq!(before.retry.infra_count, 10);

    // 11th failure: infra_retry_count=10 >= max_infra_retries=10 → poison.
    let ok = handle.debug_force_assign(drv_hash, "infra-cap-w").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "infra-cap-w",
        &drv_path,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        "infra attempt 10 (cap hit)",
    )
    .await?;

    let after = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        after.status,
        DerivationStatus::Poisoned,
        "11th infra failure (infra_retry_count=10 >= max_infra_retries=10) → poison"
    );
    // Confirm the infra path never touched the transient-failure
    // accounting (these stay at 0 the whole way through).
    assert!(after.retry.failed_builders.is_empty());
    assert_eq!(after.retry.count, 0);
    assert_eq!(after.retry.failure_count, 0);

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
    let (_db, handle, _task, _rx) = setup_with_worker("putpath-w", "x86_64-linux").await?;

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
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            "upload failed: concurrent PutPath in progress for this path; retry",
        )
        .await?;
    }

    let after = expect_drv(&handle, drv_hash).await;
    assert!(
        matches!(
            after.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "15× concurrent-PutPath infra failures → still alive (exempt from cap), got {:?}",
        after.status
    );
    assert_eq!(
        after.retry.infra_count, 0,
        "concurrent-PutPath must NOT increment infra_retry_count"
    );
    assert!(after.retry.failed_builders.is_empty());
    assert_eq!(after.retry.count, 0);
    assert_eq!(after.retry.failure_count, 0);

    // A NON-exempt infra failure after that DOES count — proves the
    // exemption is keyed on error_msg, not a blanket disable.
    let ok = handle.debug_force_assign(drv_hash, "putpath-w").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "putpath-w",
        &drv_path,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        "FUSE EIO",
    )
    .await?;
    let after2 = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        after2.retry.infra_count, 1,
        "non-exempt infra failure after exempt ones → counter increments normally"
    );

    Ok(())
}

/// `require_distinct_workers` mode: 3× TransientFailure on the SAME
/// worker poisons iff `require_distinct_workers=false`. Same inputs,
/// opposite config, opposite outcome.
///
/// - **non_distinct** (`false`): `failure_count` 1→2→3 ≥ threshold →
///   poisoned. Primary use case: single-worker dev deployments.
/// - **distinct** (default `true`): `failed_builders.len()=1 < 3` →
///   NOT poisoned via threshold (would need 3 DISTINCT workers).
///   `max_retries` raised so the `retry_count>=2` branch doesn't mask.
///
/// Both: 3 workers (one real + 2 aarch64 padding) so the
/// all-workers-failed clamp (`min(threshold, worker_count)`) doesn't
/// fire before `failure_count=3`.
#[rstest]
#[case::non_distinct(false, true)]
#[case::distinct(true, false)]
#[tokio::test]
async fn test_same_worker_poison_threshold_distinct_mode(
    #[case] require_distinct: bool,
    #[case] expect_poisoned: bool,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.poison = PoisonConfig {
            threshold: 3,
            require_distinct_workers: require_distinct,
        };
        // Raise max_retries so the retry_count>=2 branch doesn't mask
        // what we're testing (the threshold branch).
        c.retry_policy.max_retries = 10;
    });
    let _db = db;

    let _rx = connect_executor(&handle, "solo-worker", "x86_64-linux").await?;
    let _rx2 = connect_executor(&handle, "pad-w2", "aarch64-linux").await?;
    let _rx3 = connect_executor(&handle, "pad-w3", "aarch64-linux").await?;

    let drv_hash = "distinct-mode-drv";
    let drv_path = test_drv_path(drv_hash);
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    for i in 0..3 {
        if i > 0 {
            assert!(handle.debug_force_assign(drv_hash, "solo-worker").await?);
        }
        complete_failure(
            &handle,
            "solo-worker",
            &drv_path,
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("same-worker failure {i}"),
        )
        .await?;
    }

    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.retry.failed_builders.len(),
        1,
        "HashSet: same worker inserted once, stays len()=1"
    );
    assert_eq!(info.retry.failure_count, 3, "flat count always increments");
    assert_eq!(
        info.status == DerivationStatus::Poisoned,
        expect_poisoned,
        "require_distinct_workers={require_distinct}: 3× same-worker → poisoned={expect_poisoned}"
    );
    Ok(())
}

/// Completing a child releases its parent to Ready in a dependency chain.
#[tokio::test]
async fn test_dependency_chain_releases_parent() -> TestResult {
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("chain-worker", "x86_64-linux").await?;

    // A depends on B. B is Ready (leaf), A is Queued.
    let build_id = Uuid::new_v4();
    let p_chain_a = test_drv_path("chainA");
    let p_chain_b = test_drv_path("chainB");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![make_node("chainA"), make_node("chainB")],
        vec![make_test_edge("chainA", "chainB")],
        false,
    )
    .await?;

    // B is dispatched first (leaf). A is Queued waiting for B.
    let info_a = expect_drv(&handle, "chainA").await;
    assert_eq!(info_a.status, DerivationStatus::Queued);

    // Worker receives B's assignment.
    let assigned_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(assigned_path, p_chain_b);

    // Complete B. One-shot worker drains; connect a fresh one for A.
    complete_success_empty(&handle, "chain-worker", &p_chain_b).await?;
    let mut stream_rx = connect_executor(&handle, "chain-worker-2", "x86_64-linux").await?;

    // A should now transition Queued -> Ready -> Assigned (dispatched).
    let info_a = expect_drv(&handle, "chainA").await;
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
    let (_db, handle, _task, _stream_rx) = setup_with_worker("idem-worker", "x86_64-linux").await?;

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
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            final_resources: None,
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
    let info = expect_drv(&handle, "ready-drv").await;
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
    let (_db, handle, _task, mut _rx) = setup_with_worker("unk-w", "x86_64-linux").await?;

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
            result: rio_proto::types::BuildResult {
                status: 9999, // not a valid enum
                error_msg: "mystery".into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            final_resources: None,
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
    let (_db, handle, _task, mut _rx) = setup_with_worker("cancel-w", "x86_64-linux").await?;

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
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Cancelled.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            final_resources: None,
        })
        .await?;
    barrier(&handle).await;

    // Drv still Cancelled (no spurious state change).
    let info = expect_drv(&handle, "cancel-drv").await;
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

// ---------------------------------------------------------------------------
// build_samples write on completion (SLA fit feed)
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
    let (db, handle, _task, mut stream_rx) = setup_with_worker("bs-worker", "x86_64-linux").await?;

    // Merge with a distinct pname — make_test_node defaults to
    // "test-pkg", which is fine, but a unique pname makes the
    // SELECT below unambiguous if other tests ever share the pool.
    let build_id = Uuid::new_v4();
    let mut node = make_node("bs-drv");
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
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
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
            peak_cpu_cores: 1.5,
            node_name: None,
            final_resources: None,
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
    let (db, handle, _task, mut stream_rx) = setup_with_worker("nt-worker", "x86_64-linux").await?;

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
        setup_with_worker("clamp-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut node = make_node("clamp-drv");
    node.pname = "clamp-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    let _assignment = recv_assignment(&mut stream_rx).await;

    let drv_path = test_drv_path("clamp-drv");
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "clamp-worker".into(),
            drv_key: drv_path,
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
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
            peak_cpu_cores: 0.0,
            node_name: None,
            final_resources: None,
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

    let (db, handle, _task, mut stream_rx) = setup_with_worker("pt-worker", "x86_64-linux").await?;

    // ── Seed 2 tenants. FK path_tenants→tenants ON DELETE CASCADE
    // means these rows MUST exist before the upsert. ─────────────────
    let tenant_a = rio_store::test_helpers::seed_tenant(&db.pool, "pt-tenant-a").await;
    let tenant_b = rio_store::test_helpers::seed_tenant(&db.pool, "pt-tenant-b").await;

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
                    nodes: vec![make_node(drv_tag)],
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
/// in `derivations.retry.failed_builders`. Pre-fix, only the success path
/// did the assignment write — every poisoned/cancelled derivation
/// kept a `pending` row, the pruner's `NOT EXISTS assignments` never
/// matched, and `derivations` leaked.
///
/// Part B proves the pruner can now actually delete: drop the
/// `build_derivations` link, run `gc_orphan_terminal_derivations`,
/// assert the row is gone (and the assignment row CASCADEd with it).
#[tokio::test]
async fn permanent_failure_terminals_assignment_and_records_executor() -> TestResult {
    let (db, handle, _task, mut rx) = setup_with_worker("i209-w", "x86_64-linux").await?;

    let _ev = merge_single_node(&handle, Uuid::new_v4(), "i209", PriorityClass::Scheduled).await?;
    let _ = recv_assignment(&mut rx).await;
    complete_failure(
        &handle,
        "i209-w",
        &test_drv_path("i209"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
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
