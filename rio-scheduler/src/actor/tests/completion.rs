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
/// - **empty_path**: `output_path=""` → boundary filter in
///   `handle_completion` drops it (`rio_scheduler_malformed_built_
///   output_total` increments) → compare sees 0 outputs.
/// - **no_prior**: nothing seeded, 2 outputs → first lookup → None →
///   miss → short-circuit break.
#[rstest]
// bughunt-mc196: own row only → self-exclusion → miss
#[case::first_build("ca-first", CaSeed::Own, vec![("out", "ca-first-out", 32)], false, Some(1), Some(0), 0)]
// own + prior → finds sibling → match → unchanged=true
#[case::second_build("ca-second", CaSeed::OwnAndPrior, vec![("out", "ca-second-out", 32)], true, Some(0), Some(1), 0)]
// built_outputs=[] → !is_empty() guard → no loop iteration
#[case::zero_outputs("ca-zero", CaSeed::None, vec![], false, Some(0), Some(0), 0)]
// output_path="" → boundary filter drops it → malformed counter
#[case::empty_path("ca-empty", CaSeed::None, vec![("out", "", 32)], false, Some(0), Some(0), 1)]
// nothing seeded → Ok(None) → miss → break
#[case::no_prior("ca-noprior", CaSeed::None, vec![("out", "ca-noprior-out1", 32), ("dev", "ca-noprior-out2", 32)], false, Some(1), Some(0), 0)]
#[tokio::test]
async fn ca_compare_edge_cases(
    #[case] key: &str,
    #[case] seed: CaSeed,
    #[case] outputs: Vec<(&str, &str, usize)>,
    #[case] expect_unchanged: bool,
    #[case] expect_miss: Option<u64>,
    #[case] expect_match: Option<u64>,
    #[case] expect_malformed: u64,
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
    assert_eq!(
        recorder.get("rio_scheduler_malformed_built_output_total{}"),
        expect_malformed,
        "malformed-output boundary filter (handle_completion)"
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

// r[verify sched.ca.cutoff-propagate+2]
/// bug_009 regression: `verify_cutoff_candidates` matched prior
/// outputs by string suffix (`p.ends_with("-{name}")`). A prior
/// `…-python-requests` satisfied `.ends_with("-requests")`, so a
/// candidate with `name="requests"` cross-matched a DIFFERENT
/// package, was Skipped, and a poisoned `(candidate_modular →
/// wrong_path)` realisation row was written to PG.
///
/// Setup: A→B and A→C (siblings). B's drv_name=`python-requests`,
/// C's drv_name=`requests`. Seed prior realisations for A and B
/// only; realisation_deps(B depends on A). C has NO prior
/// realisation — it MUST NOT be Skipped, and PG MUST have no
/// realisation row for C's modular_hash.
///
/// Mutation check: revert to `ends_with("-{name}")` → C
/// cross-matches B's `…-python-requests` path → C goes Skipped →
/// the `assert_ne` below fails. (Match key is `drv_name()` derived
/// from the `.drv` store-path, not `pname` — see bug_006.)
#[tokio::test]
async fn cascade_rejects_pname_suffix_cross_match() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let pool = db.pool.clone();
    let _db = db;

    let a_modular: [u8; 32] = [0xA0; 32];
    let b_modular: [u8; 32] = [0xB0; 32];
    let c_modular: [u8; 32] = [0xC0; 32];
    let a_prior: [u8; 32] = [0xA1; 32];
    let b_prior: [u8; 32] = [0xB1; 32];

    let a_out = test_store_path("xmatch-a");
    // Load-bearing: b_out's name segment is `python-requests`, which
    // ends with `-requests` (C's drv_name). With suffix matching, C
    // would cross-match this path.
    let b_out = test_store_path("python-requests");

    let mut node_a = make_node("xmatch-a");
    node_a.is_content_addressed = true;
    node_a.ca_modular_hash = a_modular.to_vec();
    let mut node_b = make_node("python-requests");
    node_b.is_content_addressed = true;
    node_b.ca_modular_hash = b_modular.to_vec();
    let mut node_c = make_node("requests");
    node_c.is_content_addressed = true;
    node_c.ca_modular_hash = c_modular.to_vec();

    let _rx = connect_executor(&handle, "xmatch-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![node_a, node_b, node_c],
        vec![
            make_test_edge("python-requests", "xmatch-a"),
            make_test_edge("requests", "xmatch-a"),
        ],
        false,
    )
    .await?;

    // Seed PRIOR build's realisations for A and B; realisation_deps
    // B→A so the walk from A's prior modular_hash finds B.
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

    store.seed_with_content(&b_out, b"b-content");

    complete_ca(
        &handle,
        "xmatch-worker",
        &test_drv_path("xmatch-a"),
        &[("out", &a_out, vec![0xAA; 32])],
    )
    .await?;
    barrier(&handle).await;

    let info_a = expect_drv(&handle, "xmatch-a").await;
    assert!(info_a.ca.output_unchanged, "precondition: A matched prior");

    // B: Skipped, stamped with the CORRECT path (its own).
    let info_b = expect_drv(&handle, "python-requests").await;
    assert_eq!(
        info_b.status,
        DerivationStatus::Skipped,
        "B has prior realisation + output present → Skipped"
    );
    assert_eq!(info_b.output_paths, vec![b_out.clone()]);

    // C: NOT Skipped. With the suffix-match bug, C would cross-match
    // b_out (…-python-requests ends_with -requests) and go Skipped.
    let info_c = expect_drv(&handle, "requests").await;
    assert_ne!(
        info_c.status,
        DerivationStatus::Skipped,
        "C MUST NOT cross-match B's `…-python-requests` path \
         (bug_009: suffix-match would Skip C here)"
    );
    assert!(
        matches!(
            info_c.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "C builds normally; got {:?}",
        info_c.status
    );

    // Poisoned-PG check: no realisation row for C's modular_hash.
    // With the bug, `(c_modular, out) → b_out` would have been
    // inserted — gateway QueryRealisation would return python-
    // requests' content for `requests`.
    let (n_rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM realisations WHERE drv_hash = $1")
        .bind(c_modular.as_slice())
        .fetch_one(&pool)
        .await?;
    assert_eq!(
        n_rows, 0,
        "no poisoned realisation row written for C's modular_hash"
    );

    Ok(())
}

// r[verify sched.ca.cutoff-propagate+2]
/// bug_006 regression: `verify_cutoff_candidates` keyed
/// `expected_name` on bare `pname` (`"hello"`), but store-path name
/// segments are `"${name}"` = `"${pname}-${version}"` for stdenv
/// (`"hello-2.10"`). The match never fired for any versioned
/// package — the entire cascade was dead code for ~all of nixpkgs.
///
/// Setup: A→B with B's drv_name=`hello-2.10` (so a real stdenv path
/// `…-hello-2.10` matches), `pname="hello"`, `version="2.10"`. Seed
/// B's prior realisation at `…-hello-2.10`. Pre-fix: `expected_name
/// = "hello"`, never matches `"hello-2.10"` → B stays Queued.
/// Post-fix: `expected_name = drv_name() = "hello-2.10"` → Skipped.
#[tokio::test]
async fn cascade_matches_versioned_stdenv_name() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let pool = db.pool.clone();
    let _db = db;

    let a_modular: [u8; 32] = [0xA0; 32];
    let b_modular: [u8; 32] = [0xB0; 32];
    let a_prior: [u8; 32] = [0xA1; 32];
    let b_prior: [u8; 32] = [0xB1; 32];

    let a_out = test_store_path("stdenv-a");
    // Load-bearing: store-path name segment is `hello-2.10`
    // (`${pname}-${version}`), as for any real stdenv package.
    let b_out = test_store_path("hello-2.10");

    let mut node_a = make_node("stdenv-a");
    node_a.is_content_addressed = true;
    node_a.ca_modular_hash = a_modular.to_vec();
    // drv_path = …-hello-2.10.drv → drv_name() = "hello-2.10".
    // pname/version diverge from drv_name exactly as stdenv does;
    // pre-fix the cascade keyed on pname="hello" and never matched.
    let mut node_b = make_node("hello-2.10");
    node_b.is_content_addressed = true;
    node_b.ca_modular_hash = b_modular.to_vec();
    node_b.pname = "hello".into();
    node_b.version = Some("2.10".into());

    let _rx = connect_executor(&handle, "stdenv-w", "x86_64-linux").await?;
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![node_a, node_b],
        vec![make_test_edge("hello-2.10", "stdenv-a")],
        false,
    )
    .await?;

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
    store.seed_with_content(&b_out, b"hello-content");

    complete_ca(
        &handle,
        "stdenv-w",
        &test_drv_path("stdenv-a"),
        &[("out", &a_out, vec![0xAA; 32])],
    )
    .await?;
    barrier(&handle).await;

    let info_a = expect_drv(&handle, "stdenv-a").await;
    assert!(info_a.ca.output_unchanged, "precondition: A matched prior");

    let info_b = expect_drv(&handle, "hello-2.10").await;
    assert_eq!(
        info_b.status,
        DerivationStatus::Skipped,
        "stdenv-shaped name (`{{pname}}-{{version}}`) MUST match prior \
         realisation → Skipped (pre-fix: pname=\"hello\" != \"hello-2.10\" → \
         stayed Queued, cascade dead for all of nixpkgs)"
    );
    assert_eq!(info_b.output_paths, vec![b_out]);
    Ok(())
}

// r[verify sched.completion.output-membership]
/// bug_071 regression: `handle_completion` validated worker-supplied
/// `built_outputs` only by `StorePath::parse` format, never by
/// membership in scheduler-trusted `output_names` or by cardinality.
/// A compromised worker reporting on its own assigned 1-output drv
/// could send ~30k fabricated entries (4MB tonic limit ÷ ~130B/entry),
/// all reaching `state.output_paths` → `upsert_path_tenants`
/// (arbitrary worker-chosen paths pinned against GC) and the
/// sequential `insert_realisation` loop (~150s actor stall).
///
/// Post-fix: entries with `output_name ∉ output_names` are dropped
/// AND duplicates by `output_name` are dropped, so
/// `output_paths.len() ≤ output_names.len()` regardless of what the
/// worker sends.
#[tokio::test]
async fn built_outputs_membership_filter() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, handle, _task, _rx) = setup_with_worker("memb-w", "x86_64-linux").await?;
    let drv_hash = "memb-drv";
    let drv_path = test_drv_path(drv_hash);
    // make_node → output_names = ["out"] (1 declared output).
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;
    assert!(handle.debug_force_assign(drv_hash, "memb-w").await?);

    // 1 valid "out" + 100 fabricated names + 1 duplicate "out". All
    // paths are well-formed (the format-filter passes); only the
    // membership/dedup filter catches them.
    let valid = test_store_path("memb-out");
    let mut outs = vec![rio_proto::types::BuiltOutput {
        output_name: "out".into(),
        output_path: valid.clone(),
        output_hash: vec![0u8; 32],
    }];
    for i in 0..100 {
        outs.push(rio_proto::types::BuiltOutput {
            output_name: format!("fake{i}"),
            output_path: test_store_path(&format!("fake-{i}")),
            output_hash: vec![0u8; 32],
        });
    }
    outs.push(rio_proto::types::BuiltOutput {
        output_name: "out".into(),
        output_path: test_store_path("memb-dup"),
        output_hash: vec![0u8; 32],
    });

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "memb-w".into(),
            drv_key: drv_path,
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: outs,
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_resources: None,
        })
        .await?;
    barrier(&handle).await;

    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(info.status, DerivationStatus::Completed);
    assert_eq!(
        info.output_paths,
        vec![valid],
        "only the declared `out` survives; 100 fabricated + 1 dup dropped \
         (pre-fix: output_paths.len() == 102)"
    );
    assert_eq!(
        recorder.get("rio_scheduler_undeclared_built_output_total{}"),
        100,
        "one counter increment per undeclared output_name"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-propagate+2]
/// bug_009 ambiguity-guard regression: when TWO prior outputs share
/// the exact same name segment, the candidate is excluded (degrades
/// to no-skip). The old `.find()` took whichever the HashMap
/// happened to yield first — non-deterministic, no ambiguity check.
///
/// Setup: A→B. Seed two prior realisations whose output_paths BOTH
/// have name segment exactly `ambig-pkg` (different hash-parts,
/// different prior modular hashes), both reachable via
/// realisation_deps from A's prior. B's pname=`ambig-pkg`. The
/// match is ambiguous → B MUST NOT be Skipped.
#[tokio::test]
async fn cascade_rejects_ambiguous_prior_match() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let pool = db.pool.clone();
    let _db = db;

    let a_modular: [u8; 32] = [0xA0; 32];
    let b_modular: [u8; 32] = [0xB0; 32];
    let a_prior: [u8; 32] = [0xA1; 32];
    let b_prior_1: [u8; 32] = [0xB1; 32];
    let b_prior_2: [u8; 32] = [0xB2; 32];

    let a_out = test_store_path("ambig-a");
    // Two distinct store paths, SAME name segment `ambig-pkg`.
    let b_out_1 = test_store_path("ambig-pkg");
    let b_out_2 = format!("/nix/store/{}-ambig-pkg", "b".repeat(32));

    let mut node_a = make_node("ambig-a");
    node_a.is_content_addressed = true;
    node_a.ca_modular_hash = a_modular.to_vec();
    // drv_name = "ambig-pkg" (matches BOTH b_out_1 and b_out_2's
    // name segment → triggers the >1-hit ambiguity guard).
    let mut node_b = make_node("ambig-pkg");
    node_b.is_content_addressed = true;
    node_b.ca_modular_hash = b_modular.to_vec();

    let _rx = connect_executor(&handle, "ambig-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![node_a, node_b],
        vec![make_test_edge("ambig-pkg", "ambig-a")],
        false,
    )
    .await?;

    seed_realisation(&pool, &a_prior, "out", &a_out, &[0xAA; 32]).await?;
    seed_realisation(&pool, &b_prior_1, "out", &b_out_1, &[0xB1; 32]).await?;
    seed_realisation(&pool, &b_prior_2, "out", &b_out_2, &[0xB2; 32]).await?;
    // Both prior B's depend on prior A → walk_dependent_realisations
    // returns both → two exact-name hits for `ambig-pkg`.
    for prior in [&b_prior_1, &b_prior_2] {
        sqlx::query(
            "INSERT INTO realisation_deps (drv_hash, output_name, dep_drv_hash, dep_output_name) \
             VALUES ($1, 'out', $2, 'out')",
        )
        .bind(prior.as_slice())
        .bind(a_prior.as_slice())
        .execute(&pool)
        .await?;
    }
    store.seed_with_content(&b_out_1, b"b1");
    store.seed_with_content(&b_out_2, b"b2");

    complete_ca(
        &handle,
        "ambig-worker",
        &test_drv_path("ambig-a"),
        &[("out", &a_out, vec![0xAA; 32])],
    )
    .await?;
    barrier(&handle).await;

    let info_a = expect_drv(&handle, "ambig-a").await;
    assert!(info_a.ca.output_unchanged, "precondition: A matched prior");

    let info_b = expect_drv(&handle, "ambig-pkg").await;
    assert_ne!(
        info_b.status,
        DerivationStatus::Skipped,
        "two prior outputs both named `ambig-pkg` → ambiguous → \
         B excluded from cascade (must build, not skip)"
    );
    assert!(
        matches!(
            info_b.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "B builds normally; got {:?}",
        info_b.status
    );

    Ok(())
}

// r[verify sched.ca.cutoff-propagate+2]
// r[verify sched.db.batch-unnest]
/// bug_012 functional-equivalence guard: the batched
/// `ca_cutoff_cascade` writes the SAME PG state as the per-item
/// loop did. 5-node CA chain A→B→C→D→E; seed prior realisations for
/// A-D, NOT E. After A completes:
///
/// - B,C,D go Skipped → PG `derivations.status='skipped'` for each
///   (proves `persist_status_batch` ran).
/// - PG `realisations` has rows for `(b|c|d_modular, out)` (proves
///   `insert_realisation_batch` ran).
/// - E goes Ready (proves the batched ready-promotion ran) → PG
///   `derivations.status='ready'` for E.
///
/// Per ci-failure-patterns.md ("structural > retry > widen"): the
/// N→3 round-trip reduction is verified structurally (no per-item
/// `.await` left in the loop body) + the `insert_realisation_batch`
/// unit test, NOT by wall-clock.
#[tokio::test]
async fn cascade_batch_persists_skipped_and_ready() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let pool = db.pool.clone();
    let _db = db;

    let tags = ["batch-a", "batch-b", "batch-c", "batch-d", "batch-e"];
    let modular: [[u8; 32]; 5] = [[0xA0; 32], [0xB0; 32], [0xC0; 32], [0xD0; 32], [0xE0; 32]];
    let prior: [[u8; 32]; 4] = [[0xA1; 32], [0xB1; 32], [0xC1; 32], [0xD1; 32]];
    let outs: Vec<String> = tags.iter().map(|t| test_store_path(t)).collect();

    let nodes: Vec<_> = tags
        .iter()
        .zip(&modular)
        .map(|(t, m)| {
            let mut n = make_node(t);
            n.is_content_addressed = true;
            n.ca_modular_hash = m.to_vec();
            n.pname = (*t).into();
            n
        })
        .collect();

    let _rx = connect_executor(&handle, "batch-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        nodes,
        vec![
            make_test_edge("batch-b", "batch-a"),
            make_test_edge("batch-c", "batch-b"),
            make_test_edge("batch-d", "batch-c"),
            make_test_edge("batch-e", "batch-d"),
        ],
        false,
    )
    .await?;

    // Seed PRIOR realisations + chain deps for A-D (not E). Seed
    // B-D's outputs in MockStore so FindMissingPaths finds them.
    for i in 0..4 {
        seed_realisation(&pool, &prior[i], "out", &outs[i], &[0x77; 32]).await?;
        if i > 0 {
            sqlx::query(
                "INSERT INTO realisation_deps \
                 (drv_hash, output_name, dep_drv_hash, dep_output_name) \
                 VALUES ($1, 'out', $2, 'out')",
            )
            .bind(prior[i].as_slice())
            .bind(prior[i - 1].as_slice())
            .execute(&pool)
            .await?;
            store.seed_with_content(&outs[i], b"content");
        }
    }

    complete_ca(
        &handle,
        "batch-worker",
        &test_drv_path("batch-a"),
        &[("out", &outs[0], vec![0x77; 32])],
    )
    .await?;
    barrier(&handle).await;

    let info_a = expect_drv(&handle, "batch-a").await;
    assert!(info_a.ca.output_unchanged, "precondition: A matched prior");

    // B,C,D Skipped in-mem AND in PG.
    for (i, tag) in tags[1..4].iter().enumerate() {
        let info = expect_drv(&handle, tag).await;
        assert_eq!(
            info.status,
            DerivationStatus::Skipped,
            "{tag} should be Skipped"
        );
        assert_eq!(
            info.output_paths,
            vec![outs[i + 1].clone()],
            "{tag} stamped with prior output_path"
        );
        let (st,): (String,) = sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
            .bind(tag)
            .fetch_one(&pool)
            .await?;
        assert_eq!(st, "skipped", "{tag} PG status (persist_status_batch ran)");
        let (n,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM realisations WHERE drv_hash = $1")
            .bind(modular[i + 1].as_slice())
            .fetch_one(&pool)
            .await?;
        assert_eq!(n, 1, "{tag} realisation row (insert_realisation_batch ran)");
    }

    // E: Ready (batched ready-promotion ran). PG status = 'ready'.
    let info_e = expect_drv(&handle, "batch-e").await;
    assert!(
        matches!(
            info_e.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "E goes Ready after D Skipped; got {:?}",
        info_e.status
    );
    let (st_e,): (String,) = sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
        .bind("batch-e")
        .fetch_one(&pool)
        .await?;
    assert!(
        st_e == "ready" || st_e == "assigned",
        "E PG status persisted (batched ready-promotion ran); got {st_e}"
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
///
/// `backoff_base_secs=0` so the retry dispatches DETERMINISTICALLY in
/// the same tick (the previous default 5.0 meant the load-bearing
/// `assert_ne!(retry_worker, first_worker)` arm never executed —
/// status was always Ready under backoff).
#[tokio::test]
async fn test_transient_retry_different_worker() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.backoff_base_secs = 0.0;
    });
    let _db = db;

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

    // backoff=0 → re-dispatched in the same handle_completion tail.
    // failed_builders contains first_worker so retry goes to the
    // OTHER worker. Deterministic — no match-on-status; the
    // load-bearing assertion is `retry_worker != first_worker`.
    barrier(&handle).await;
    let info2 = expect_drv(&handle, "retry-hash").await;
    assert_eq!(
        info2.retry.count, 1,
        "transient failure should increment retry_count"
    );
    assert_eq!(
        info2.status,
        DerivationStatus::Assigned,
        "backoff=0 → immediate re-dispatch"
    );
    let retry_worker = info2.assigned_executor.expect("Assigned → worker set");
    assert_ne!(
        retry_worker, first_worker,
        "failed_builders exclusion: retry must go to a DIFFERENT worker"
    );
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
    // Pad workers (statically-eligible — same system) so the
    // fleet-exhaustion clamp (`r[sched.dispatch.fleet-exhaust]`)
    // doesn't fire before max_retries; we're testing the max_retries
    // branch specifically.
    let _rx2 = connect_executor(&handle, "mr-pad2", "x86_64-linux").await?;
    let _rx3 = connect_executor(&handle, "mr-pad3", "x86_64-linux").await?;

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
        // Force-assign (including attempt=0): with 3 statically-
        // eligible workers, natural dispatch may pick a padding worker.
        let ok = handle
            .debug_force_assign("maxretry-hash", "flaky-worker")
            .await?;
        assert!(ok, "force-assign should succeed (attempt {attempt})");
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
// r[verify sched.retry.promotion-exempt+3]
// r[verify sched.sla.reactive-floor+2]
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
            &format!("{}; bumping resource floor", rio_proto::CGROUP_OOM_MSG),
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

/// Distinct-worker poison-threshold matrix under one-shot (I-188)
/// semantics. N TransientFailures on distinct workers:
///
/// - **threshold**: 3 distinct of 4 workers → `failed_builders.len()
///   ≥ POISON_THRESHOLD` → Poisoned. Independent of fleet-exhaust.
/// - **pool_2_no_premature_poison**: 2 distinct of 2 workers (below
///   threshold=3) → Ready (re-queued for fresh workers), NOT
///   Poisoned. Under one-shot, both failed workers are draining →
///   `failed_builders_exhausts_fleet` excludes them → empty fleet →
///   `false` → re-queue. The controller spawns fresh `executor_id`s
///   ∉ `failed_builders`; poison flows ONLY through
///   `is_poisoned(threshold)` once 3 distinct accumulate.
/// - **pool_1_no_premature_poison** (bug_108 regression): poolSize=1,
///   1 transient → Ready, NOT Poisoned. Pre-fix the just-failed
///   draining worker counted toward "the fleet" → fleet={E1},
///   failed={E1} → exhausted → poisoned on the FIRST failure,
///   bypassing `max_retries` and `poison_config.threshold`.
///
/// The kind/system/features clauses of `statically_eligible` are
/// independently pinned by the unit-level
/// `assignment::statically_eligible_agrees_with_rejection_reason`;
/// the actor-level `kind_aware` case (bug_160) is no longer
/// reachable under one-shot (failed workers drain → excluded
/// regardless of kind), so it's subsumed here.
///
/// `max_retries=10` for ALL cases so the `retry_count>=max_retries`
/// branch doesn't mask the threshold under test (default
/// `max_retries=2` would trip on the same iteration as
/// `is_poisoned(3≥3)` for the threshold case — vacuous coverage).
#[rstest]
#[case::threshold(4, &["pt-w1", "pt-w2", "pt-w3"], DerivationStatus::Poisoned)]
#[case::pool_2_no_premature_poison(2, &["pt-w1", "pt-w2"], DerivationStatus::Ready)]
#[case::pool_1_no_premature_poison(1, &["pt-w1"], DerivationStatus::Ready)]
#[tokio::test]
async fn test_distinct_transient_poison_matrix(
    #[case] n_builders: usize,
    #[case] fail_on: &[&str],
    #[case] expected_status: DerivationStatus,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.max_retries = 10;
    });
    let _db = db;

    let mut _rxs = Vec::new();
    for i in 1..=n_builders {
        _rxs.push(connect_executor(&handle, &format!("pt-w{i}"), "x86_64-linux").await?);
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
        expected_status,
        "{} distinct TransientFailures on pool of {n_builders} → {expected_status:?} \
         (failed_builders={:?})",
        fail_on.len(),
        info.retry.failed_builders
    );
    assert_eq!(info.retry.failed_builders.len(), fail_on.len());
    if expected_status == DerivationStatus::Poisoned {
        assert_eq!(
            query_status(&handle, build_id).await?.state,
            rio_proto::types::BuildState::Failed as i32
        );
    }
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
    let mut _rxs = Vec::with_capacity(4);
    for i in 1..=4 {
        _rxs.push(connect_executor(&handle, &format!("infra-w{i}"), "x86_64-linux").await?);
    }

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

// r[verify sched.timeout.promote-on-exceed+3]
/// I-200: `TimedOut` promotes `resource_floor` AND resets to Ready
/// (bounded by `max_timeout_retries`), then goes terminal `Cancelled`.
///
/// Before I-200, `TimedOut` went straight to Cancelled without
/// promoting — so a worker-side `build_timeout_secs` hit on a small
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
        "build exceeded build_timeout_secs",
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
        "build exceeded build_timeout_secs",
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
        "build exceeded build_timeout_secs",
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

/// bug_468 / I-127: PutPathBatch's `Aborted` message used to read
/// "concurrent upload in progress" — `.contains("concurrent PutPath")`
/// never matched, so multi-output drvs hitting placeholder contention
/// got `infra_count++` and poisoned. Both store emit sites now use
/// `rio_proto::CONCURRENT_PUTPATH_MSG`; this test feeds the BATCH
/// shape (output-index prefix + suffix) and asserts the exemption.
#[tokio::test]
async fn i127_batch_concurrent_putpath_exempt() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("batch-w", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "batch-putpath-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    let batch_msg = format!(
        "output upload failed: output 1: {}; retry",
        rio_proto::CONCURRENT_PUTPATH_MSG
    );
    for attempt in 0..15 {
        let ok = handle.debug_force_assign(drv_hash, "batch-w").await?;
        assert!(ok, "force-assign should succeed at attempt {attempt}");
        complete_failure(
            &handle,
            "batch-w",
            &drv_path,
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            &batch_msg,
        )
        .await?;
    }

    let after = expect_drv(&handle, drv_hash).await;
    assert!(
        matches!(
            after.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "batch-shape concurrent-PutPath → exempt from cap, got {:?}",
        after.status
    );
    assert_eq!(
        after.retry.infra_count, 0,
        "batch-shape concurrent-PutPath must NOT increment infra_retry_count"
    );
    assert_eq!(
        after.retry.exempt_infra_count, 15,
        "exempt attempts tracked separately (15 < default max_exempt_infra_retries=50)"
    );

    Ok(())
}

// r[verify sched.retry.exempt-infra-cap]
/// `max_exempt_infra_retries` is the scheduler-side terminal for the
/// I-127 CONCURRENT_PUTPATH cap-exemption. Without it, a leaked
/// store-side placeholder lock (the I-125a class) makes every honest
/// worker report the exempt message → infinite pod churn with
/// `infra_count=0` and no `warn!`. With it, the drv poisons at the
/// configured cap with a `warn!` signal so the operator investigates.
#[tokio::test]
async fn exempt_infra_cap_terminates_leaked_lock() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.max_exempt_infra_retries = 5;
    });
    let _db = db;
    let _rx = connect_executor(&handle, "leak-w", "x86_64-linux").await?;

    let drv_hash = "leak-drv";
    let drv_path = test_drv_path(drv_hash);
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    let leaked_msg = format!("upload failed: {}", rio_proto::CONCURRENT_PUTPATH_MSG);
    for attempt in 0..4 {
        let ok = handle.debug_force_assign(drv_hash, "leak-w").await?;
        assert!(ok, "force-assign at attempt {attempt}");
        complete_failure(
            &handle,
            "leak-w",
            &drv_path,
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            &leaked_msg,
        )
        .await?;
        let s = expect_drv(&handle, drv_hash).await;
        assert_eq!(s.retry.exempt_infra_count, attempt + 1);
        assert_eq!(s.retry.infra_count, 0, "exempt → infra_count stays 0");
        assert!(
            matches!(
                s.status,
                DerivationStatus::Ready | DerivationStatus::Assigned
            ),
            "attempt {} (< cap=5): still re-queued, got {:?}",
            attempt + 1,
            s.status
        );
    }

    // 5th attempt: exempt_infra_count 5 >= max=5 → poison.
    let ok = handle.debug_force_assign(drv_hash, "leak-w").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "leak-w",
        &drv_path,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        &leaked_msg,
    )
    .await?;
    let s = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        s.status,
        DerivationStatus::Poisoned,
        "exempt_infra_count=5 >= max_exempt_infra_retries=5 → poisoned \
         (pre-fix: status stayed Ready forever, no terminal)"
    );
    assert_eq!(s.retry.exempt_infra_count, 5);
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
/// Both: 3 x86 workers so the statically-eligible fleet is 3 and the
/// fleet-exhaustion clamp (`r[sched.dispatch.fleet-exhaust]`) doesn't
/// fire before `failure_count=3`. (Padding workers used to be aarch64,
/// which only worked because the clamp was system-blind — that bug is
/// now fixed.)
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
    let _rx2 = connect_executor(&handle, "pad-w2", "x86_64-linux").await?;
    let _rx3 = connect_executor(&handle, "pad-w3", "x86_64-linux").await?;

    let drv_hash = "distinct-mode-drv";
    let drv_path = test_drv_path(drv_hash);
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    for i in 0..3 {
        // Always force-assign (including i=0): with 3 statically-
        // eligible workers, natural dispatch from merge may pick a
        // padding worker. debug_force_assign resets Assigned→Ready
        // then re-assigns to solo-worker.
        assert!(handle.debug_force_assign(drv_hash, "solo-worker").await?);
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

// r[verify sched.merge.substitute-topdown+12]
/// Completion-time clear, worker path: a topdown-pruned parent whose
/// only child is produced by a normal worker completion must lose the
/// mark right then — in memory AND in PG — not only at the next merge
/// or at its own walk failure. The persisted column is what a failover
/// restores: left set, a new leader would resurrect the doomed-dispatch
/// guard onto a node whose closure IS in the store, and a later
/// substitute failure would wrongly fail-fast the build.
#[tokio::test]
async fn test_topdown_pruned_cleared_when_children_complete_normally() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("tdcomp-worker", "x86_64-linux").await?;

    // Full merge (nothing substitutable, no prune): parent → child.
    let build_id = Uuid::new_v4();
    let p_child = test_drv_path("tdcomp-child");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![make_node("tdcomp-parent"), make_node("tdcomp-child")],
        vec![make_test_edge("tdcomp-parent", "tdcomp-child")],
        false,
    )
    .await?;

    // The leaf child is dispatched; the parent waits Queued.
    let assigned = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(assigned, p_child);
    assert_eq!(
        expect_drv(&handle, "tdcomp-parent").await.status,
        DerivationStatus::Queued
    );

    // Stage: the parent was topdown-pruned by an earlier build and the
    // mark is still set in memory and in PG (the post-pruned-merge
    // shape: its child arrived unbuilt, so no merge-time pass could
    // clear it).
    handle
        .debug_set_topdown_pruned("tdcomp-parent", true)
        .await?;
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE drv_hash = 'tdcomp-parent'")
        .execute(&db.pool)
        .await?;

    // The child completes normally through the worker path.
    complete_success_empty(&handle, "tdcomp-worker", &p_child).await?;
    barrier(&handle).await;

    // The parent's closure is now fully in the store: the mark must be
    // gone in memory…
    let parent = expect_drv(&handle, "tdcomp-parent").await;
    assert!(
        !parent.topdown_pruned,
        "normal child completion must clear the parent's topdown_pruned mark \
         (children all produced ⇒ a from-source dispatch is no longer doomed)"
    );
    // …and in PG, so a failover cannot resurrect it.
    let (pg,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdcomp-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pg,
        "the completion-time clear must also clear the persisted column — \
         it is what a failover restores"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+12]
/// Same worker-path clear as above, but with the parent NOT Queued
/// (forced Substituting — the realistic mid-fetch shape for a flagged
/// node): the child's completion promotes nothing to Ready, so this
/// pins that the clear runs BEFORE `promote_newly_ready_batch`'s
/// empty-`newly_ready` early return — moved behind it, the mark would
/// survive exactly this shape.
#[tokio::test]
async fn test_topdown_pruned_cleared_when_child_completes_while_parent_not_queued() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("tdnq-worker", "x86_64-linux").await?;

    // Full merge (nothing substitutable, no prune): parent → child.
    let build_id = Uuid::new_v4();
    let p_child = test_drv_path("tdnq-child");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![make_node("tdnq-parent"), make_node("tdnq-child")],
        vec![make_test_edge("tdnq-parent", "tdnq-child")],
        false,
    )
    .await?;

    // The leaf child is dispatched to the worker.
    let assigned = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(assigned, p_child);

    // Stage: the parent is mid-substitution (NOT Queued, so the child's
    // completion will promote nothing to Ready) and carries the
    // topdown_pruned mark in memory and in PG.
    handle
        .debug_force_status("tdnq-parent", DerivationStatus::Substituting)
        .await?;
    handle.debug_set_topdown_pruned("tdnq-parent", true).await?;
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE drv_hash = 'tdnq-parent'")
        .execute(&db.pool)
        .await?;

    // The child completes normally through the worker path.
    complete_success_empty(&handle, "tdnq-worker", &p_child).await?;
    barrier(&handle).await;

    // No node became newly Ready, yet the mark must still be gone — in
    // memory and in PG.
    let parent = expect_drv(&handle, "tdnq-parent").await;
    assert_eq!(
        parent.status,
        DerivationStatus::Substituting,
        "fixture premise: the parent stayed non-Queued, so the completion \
         promoted nothing to Ready"
    );
    assert!(
        !parent.topdown_pruned,
        "the completion-time clear must run even when the completion promotes \
         nothing to Ready (parent not Queued)"
    );
    let (pg,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdnq-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pg,
        "the persisted column must be cleared too — it is what a failover restores"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+12]
/// Same clear, substitution/store path: the child completes via
/// `complete_ready_from_store` (`SubstituteComplete{ok: true}`), which
/// promotes dependents through its own inline loop rather than
/// `promote_newly_ready_batch` — the flagged parent must still lose
/// the mark, in memory and in PG.
#[tokio::test]
async fn test_topdown_pruned_cleared_when_child_substitution_succeeds() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Full merge: parent → child.
    let build_id = Uuid::new_v4();
    merge_dag(
        &handle,
        build_id,
        vec![make_node("tdsub-parent"), make_node("tdsub-child")],
        vec![make_test_edge("tdsub-parent", "tdsub-child")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Stage: the child is mid-substitution; the parent carries the
    // topdown_pruned mark from an earlier pruned merge (memory + PG).
    handle
        .debug_force_status("tdsub-child", DerivationStatus::Substituting)
        .await?;
    handle
        .debug_set_topdown_pruned("tdsub-parent", true)
        .await?;
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE drv_hash = 'tdsub-parent'")
        .execute(&db.pool)
        .await?;

    // The child's substitute fetch succeeds → complete_ready_from_store.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdsub-child".into(),
            ok: true,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "tdsub-child").await.status,
        DerivationStatus::Completed,
        "fixture premise: ok=true SubstituteComplete completes the child from store"
    );
    let parent = expect_drv(&handle, "tdsub-parent").await;
    assert!(
        !parent.topdown_pruned,
        "a child produced by substitution must clear the parent's topdown_pruned mark"
    );
    let (pg,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdsub-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pg,
        "the persisted column must be cleared too — it is what a failover restores"
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
            hw_class: None,
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

    // Wait for dispatch (drv → Assigned to unk-w, the only worker).
    barrier(&handle).await;
    // Padding so fleet-exhaust doesn't poison on the single failure —
    // we want the retry path, not the poison path. Connect AFTER
    // dispatch so the drv is deterministically on unk-w.
    let _rx2 = connect_executor(&handle, "unk-w2", "x86_64-linux").await?;
    let _rx3 = connect_executor(&handle, "unk-w3", "x86_64-linux").await?;

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
            hw_class: None,
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
    // Behavioral assertion (the log-only check above passes even if
    // handle_transient_failure is dropped from the Unspecified arm):
    // retry.count incremented, NOT poisoned.
    let info = expect_drv(&handle, "unk-status").await;
    assert_eq!(
        info.retry.count, 1,
        "Unspecified → handle_transient_failure → retry.count++"
    );
    assert_ne!(info.status, DerivationStatus::Poisoned);
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
            caller_tenant: None,
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
            hw_class: None,
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
/// Without ALL of these, write_build_sample is never reached — the
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
            hw_class: None,
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

/// `build_samples.{hw_class, cpu_limit_cores}` are populated from the
/// `CompletionReport.hw_class` and `state.sched.last_intent.cores`
/// respectively — NOT left NULL and NOT taken from the cgroup
/// `final_resources.cpu_limit_cores`. On intent-miss fallback a
/// 2-core solve can land on a 16-core wildcard pod; the fit's
/// independent variable must be the parallelism the build RAN at
/// (assigned_cores), not the pod ceiling.
// r[verify sched.sla.hw-ref-seconds]
// r[verify sched.sla.intent-match]
#[tokio::test]
async fn test_completion_writes_hw_class_and_intent_cores() -> TestResult {
    let (db, handle, _task, mut stream_rx) = setup_with_worker("hw-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut node = make_node("hw-drv");
    node.pname = "hw-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    let assignment = recv_assignment(&mut stream_rx).await;
    // Dispatch sets last_intent → assigned_cores; capture it so the
    // assertion is robust to whatever solve_intent_for picks under the
    // test config.
    let assigned = assignment
        .assigned_cores
        .expect("dispatch sets assigned_cores from last_intent");

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "hw-worker".into(),
            drv_key: test_drv_path("hw-drv"),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("hw-pkg-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 2000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 2010,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 1 << 20,
            peak_cpu_cores: 1.0,
            node_name: Some("ip-10-0-1-5.ec2.internal".into()),
            hw_class: Some("aws-7-ebs".into()),
            // Cgroup says 99 cores — the intent-miss case where the pod
            // is bigger than the solve. Stored value must be `assigned`.
            final_resources: Some(rio_proto::types::ResourceUsage {
                cpu_limit_cores: Some(99.0),
                ..Default::default()
            }),
        })
        .await?;
    barrier(&handle).await;

    let (hw, cores): (Option<String>, Option<f64>) = sqlx::query_as(
        "SELECT hw_class, cpu_limit_cores FROM build_samples WHERE pname = 'hw-pkg'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        hw.as_deref(),
        Some("aws-7-ebs"),
        "hw_class written through from CompletionReport (not NULL)"
    );
    assert_eq!(
        cores,
        Some(f64::from(assigned)),
        "cpu_limit_cores = last_intent.cores ({assigned}), not cgroup cpu.max (99)"
    );

    // ── Inverse: cgroup < assigned (dispatch-time re-solve picked more
    // cores than the spawn-time pod has). Builder clamps NIX_BUILD_CORES
    // to cgroup, so the recorded sample must be cgroup-bounded. One-shot
    // worker drained on the first completion; connect a fresh one.
    let mut stream_rx = connect_executor(&handle, "hw-worker-2", "x86_64-linux").await?;
    let mut node = make_node("hw-drv2");
    node.pname = "hw-pkg2".into();
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let assignment2 = recv_assignment(&mut stream_rx).await;
    let assigned2 = assignment2.assigned_cores.unwrap();
    // cgroup strictly below assigned. assigned ≥ 1 under any config, so
    // 0.5 guarantees min() picks cgroup.
    let cgroup2 = f64::from(assigned2) - 0.5;
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "hw-worker-2".into(),
            drv_key: test_drv_path("hw-drv2"),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("hw-pkg2-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 2000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 2010,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 1 << 20,
            peak_cpu_cores: 1.0,
            node_name: None,
            hw_class: None,
            final_resources: Some(rio_proto::types::ResourceUsage {
                cpu_limit_cores: Some(cgroup2),
                ..Default::default()
            }),
        })
        .await?;
    barrier(&handle).await;
    let (cores2,): (Option<f64>,) =
        sqlx::query_as("SELECT cpu_limit_cores FROM build_samples WHERE pname = 'hw-pkg2'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        cores2,
        Some(cgroup2),
        "assigned > cgroup → cpu_limit_cores = min(assigned={assigned2}, cgroup={cgroup2})"
    );

    Ok(())
}

/// Negative: completion WITHOUT timestamps (the complete_success_empty
/// path) writes nothing to build_samples. The sanity gate at
/// completion.rs:285 `let (Some(start), Some(stop)) = ...` rejects
/// Default::default() timestamps (None, None). Proves the gate is
/// live — if someone removes it, this test catches the regression
/// (spurious 0.0s samples would poison the SLA estimator's percentiles).
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
            hw_class: None,
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

/// A worker-supplied `peak_cpu_cores = +Inf` is recorded as NULL ("not
/// reported"), not persisted raw into build_samples where the SLA fit's
/// saturation check reads it back. The row itself IS still written
/// (null-out, not row-skip): duration_secs survives. The companion
/// `final_resources.cpu_limit_cores = +Inf` can never displace the
/// scheduler's own assigned-cores figure (`min(assigned, +Inf) =
/// assigned` on this intent-present arm; the no-intent recovery arm is
/// covered structurally — see the note on
/// `test_completion_nonfinite_final_resources_recorded_as_null`).
/// `cpu_seconds_total = 0.0` stays persistable — pins the `>=` (not
/// `>`) boundary of the domain check.
// r[verify sched.executor.input-bounds+2]
#[tokio::test]
async fn test_completion_infinite_peak_cpu_recorded_as_null() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("infcpu-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut node = make_node("infcpu-drv");
    node.pname = "infcpu-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    let assignment = recv_assignment(&mut stream_rx).await;
    let assigned = assignment
        .assigned_cores
        .expect("dispatch sets assigned_cores from last_intent");

    // Precondition: no row for this pname yet, so the count below
    // proves THIS completion wrote it.
    let pre: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM build_samples WHERE pname = 'infcpu-pkg'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(pre, 0, "precondition: no infcpu-pkg row before completion");

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "infcpu-worker".into(),
            drv_key: test_drv_path("infcpu-drv"),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("infcpu-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 2000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 2010,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 1 << 20,
            // The pathological input: worker reports +Inf for both the
            // poller peak and the cgroup limit.
            peak_cpu_cores: f64::INFINITY,
            node_name: None,
            hw_class: None,
            final_resources: Some(rio_proto::types::ResourceUsage {
                cpu_limit_cores: Some(f64::INFINITY),
                cpu_seconds_total: Some(0.0),
                ..Default::default()
            }),
        })
        .await?;
    barrier(&handle).await;

    #[allow(clippy::type_complexity)]
    let rows: Vec<(Option<f64>, f64, Option<f64>, Option<f64>)> = sqlx::query_as(
        "SELECT peak_cpu_cores, duration_secs, cpu_limit_cores, cpu_seconds_total \
         FROM build_samples WHERE pname = 'infcpu-pkg'",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows.len(),
        1,
        "one bad telemetry float must not suppress the sample row (null-out, not row-skip)"
    );
    let (peak_cpu, dur, cpu_limit, cpu_secs) = &rows[0];
    assert_eq!(
        *peak_cpu, None,
        "+Inf peak_cpu_cores must be recorded as NULL, not persisted raw"
    );
    assert!(
        (dur - 10.0).abs() < 1e-9,
        "row still written with the real duration (stop=2010 - start=2000), got {dur}"
    );
    assert_eq!(
        *cpu_limit,
        Some(f64::from(assigned)),
        "+Inf cgroup limit must never displace the scheduler's assigned cores ({assigned})"
    );
    assert_eq!(
        *cpu_secs,
        Some(0.0),
        "cpu_seconds_total = 0.0 is in-domain and stays persistable (>= boundary)"
    );

    Ok(())
}

/// Non-finite / negative `final_resources` floats are recorded as NULL
/// rather than persisted raw: a negative cgroup `cpu_limit_cores` must
/// not win the `min()` against the scheduler's own assigned-cores
/// figure, and `cpu_seconds_total = +Inf` / `peak_io_pressure_pct =
/// NaN` must not reach the SLA fit's read path. `peak_disk_bytes =
/// u64::MAX` keeps its existing i64::MAX clamp, and a NaN
/// `peak_cpu_cores` stays NULL (the explicit `is_finite()` check
/// preserves what the old `> 0.0` comparison rejected incidentally).
///
/// No-intent (recovery) arm coverage is structural: every test in this
/// harness dispatches (which sets `last_intent`), so only the
/// `(Some(assigned), cgroup)` arm of the cpu_limit_cores fold is driven
/// directly — but the filter runs on the cgroup value BEFORE the
/// `match`, so the no-intent `(None, Some(cgroup))` arm sees the
/// already-sanitized value by construction.
// r[verify sched.executor.input-bounds+2]
#[tokio::test]
async fn test_completion_nonfinite_final_resources_recorded_as_null() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("finres-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut node = make_node("finres-drv");
    node.pname = "finres-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    let assignment = recv_assignment(&mut stream_rx).await;
    let assigned = assignment
        .assigned_cores
        .expect("dispatch sets assigned_cores from last_intent");

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "finres-worker".into(),
            drv_key: test_drv_path("finres-drv"),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("finres-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 2000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 2010,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 1 << 20,
            peak_cpu_cores: f64::NAN,
            node_name: None,
            hw_class: None,
            final_resources: Some(rio_proto::types::ResourceUsage {
                cpu_limit_cores: Some(-3.0),
                cpu_seconds_total: Some(f64::INFINITY),
                peak_io_pressure_pct: Some(f64::NAN),
                peak_disk_bytes: Some(u64::MAX),
                ..Default::default()
            }),
        })
        .await?;
    barrier(&handle).await;

    #[allow(clippy::type_complexity)]
    let (cpu_limit, cpu_secs, io_pct, disk, peak_cpu): (
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<i64>,
        Option<f64>,
    ) = sqlx::query_as(
        "SELECT cpu_limit_cores, cpu_seconds_total, peak_io_pressure_pct, \
                peak_disk_bytes, peak_cpu_cores \
         FROM build_samples WHERE pname = 'finres-pkg'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        cpu_limit,
        Some(f64::from(assigned)),
        "negative cgroup cpu_limit_cores must not win min() against assigned ({assigned})"
    );
    assert_eq!(
        cpu_secs, None,
        "+Inf cpu_seconds_total must be recorded as NULL"
    );
    assert_eq!(
        io_pct, None,
        "NaN peak_io_pressure_pct must be recorded as NULL"
    );
    assert_eq!(
        disk,
        Some(i64::MAX),
        "u64::MAX peak_disk_bytes keeps the existing i64::MAX clamp"
    );
    assert_eq!(
        peak_cpu, None,
        "NaN peak_cpu_cores stays NULL (is_finite() preserves the old incidental rejection)"
    );

    Ok(())
}

/// Finite-but-out-of-domain readings are likewise recorded as NULL: a
/// `peak_cpu_cores` above the structural cores ceiling
/// (`sla::config::MAX_CORES_HARD`) would otherwise saturate the SLA
/// fit's per-c anchor key and act as an extreme-leverage point; a 0.0
/// cgroup `cpu_limit_cores` is "no signal", not a real limit, and must
/// fall back to the assigned cores instead of winning the min(); a
/// negative cumulative CPU-seconds counter and a 150% PSI reading are
/// outside their physical domains (proto contract: pct in [0, 100]).
// r[verify sched.executor.input-bounds+2]
#[tokio::test]
async fn test_completion_out_of_domain_final_resources_recorded_as_null() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("iodom-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut node = make_node("iodom-drv");
    node.pname = "iodom-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    let assignment = recv_assignment(&mut stream_rx).await;
    let assigned = assignment
        .assigned_cores
        .expect("dispatch sets assigned_cores from last_intent");

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "iodom-worker".into(),
            drv_key: test_drv_path("iodom-drv"),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("iodom-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 2000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 2010,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 1 << 20,
            // Finite but above MAX_CORES_HARD (1024): no node in the
            // fleet can report this honestly.
            peak_cpu_cores: 5000.0,
            node_name: None,
            hw_class: None,
            final_resources: Some(rio_proto::types::ResourceUsage {
                cpu_limit_cores: Some(0.0),
                cpu_seconds_total: Some(-1.0),
                peak_io_pressure_pct: Some(150.0),
                ..Default::default()
            }),
        })
        .await?;
    barrier(&handle).await;

    let (peak_cpu, cpu_limit, cpu_secs, io_pct): (
        Option<f64>,
        Option<f64>,
        Option<f64>,
        Option<f64>,
    ) = sqlx::query_as(
        "SELECT peak_cpu_cores, cpu_limit_cores, cpu_seconds_total, peak_io_pressure_pct \
         FROM build_samples WHERE pname = 'iodom-pkg'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        peak_cpu, None,
        "finite peak_cpu_cores above MAX_CORES_HARD must be recorded as NULL"
    );
    assert_eq!(
        cpu_limit,
        Some(f64::from(assigned)),
        "0.0 cgroup limit is no-signal: falls back to assigned ({assigned}), not min(assigned, 0)"
    );
    assert_eq!(
        cpu_secs, None,
        "negative cpu_seconds_total must be recorded as NULL"
    );
    assert_eq!(
        io_pct, None,
        "150% peak_io_pressure_pct is outside the proto's [0, 100] domain, must be NULL"
    );

    Ok(())
}

/// Over-filtering guard: legitimate in-domain readings round-trip
/// unfiltered. (The valid-cgroup `cpu_limit_cores` round-trip — both
/// directions of the min() — is already covered by
/// `test_completion_writes_hw_class_and_intent_cores`.)
// r[verify sched.executor.input-bounds+2]
#[tokio::test]
async fn test_completion_valid_final_resources_round_trip() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("okres-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut node = make_node("okres-drv");
    node.pname = "okres-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    let _assignment = recv_assignment(&mut stream_rx).await;

    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "okres-worker".into(),
            drv_key: test_drv_path("okres-drv"),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("okres-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 2000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 2010,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 1 << 20,
            peak_cpu_cores: 1.5,
            node_name: None,
            hw_class: None,
            final_resources: Some(rio_proto::types::ResourceUsage {
                cpu_seconds_total: Some(12.5),
                peak_io_pressure_pct: Some(42.5),
                ..Default::default()
            }),
        })
        .await?;
    barrier(&handle).await;

    let (cpu_secs, io_pct, peak_cpu): (Option<f64>, Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT cpu_seconds_total, peak_io_pressure_pct, peak_cpu_cores \
         FROM build_samples WHERE pname = 'okres-pkg'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        cpu_secs,
        Some(12.5),
        "in-domain cpu_seconds_total round-trips unfiltered"
    );
    assert_eq!(
        io_pct,
        Some(42.5),
        "in-domain peak_io_pressure_pct round-trips unfiltered"
    );
    assert_eq!(
        peak_cpu,
        Some(1.5),
        "in-domain peak_cpu_cores round-trips unfiltered"
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

// r[verify sched.db.assignment-terminal-on-status+2]
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
    // Capture derivation_id BEFORE GC deletes the derivations row —
    // the previous JOIN-after-GC query was vacuous (JOIN yields 0
    // whether CASCADE fired or not, since the parent row is gone).
    let derivation_id: Uuid =
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind("i209")
            .fetch_one(&db.pool)
            .await?;
    // Drop the build_derivations link (simulates the owning build's
    // cleanup) so the only remaining gate is the assignments row.
    sqlx::query("DELETE FROM build_derivations WHERE derivation_id = $1")
        .bind(derivation_id)
        .execute(&db.pool)
        .await?;

    let sched_db = SchedulerDb::new(db.pool.clone());
    let deleted = sched_db.gc_orphan_terminal_derivations(10).await?;
    assert!(
        deleted >= 1,
        "I-209: terminal assignment row no longer blocks the pruner"
    );

    let remaining: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM assignments WHERE derivation_id = $1")
            .bind(derivation_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        remaining, 0,
        "034: CASCADE FK removed the assignment row with the derivation"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// G01 regressions: batching, terminal-event emission, retry-cap uniformity
// ---------------------------------------------------------------------------

/// Drain all DerivationEvents currently buffered on `rx` and partition
/// by kind.
fn drain_derivation_events(
    rx: &mut tokio::sync::broadcast::Receiver<rio_proto::types::BuildEvent>,
) -> Vec<rio_proto::types::DerivationEvent> {
    let mut out = Vec::new();
    while let Ok(ev) = rx.try_recv() {
        if let Some(rio_proto::types::build_event::Event::Derivation(d)) = ev.event {
            out.push(d);
        }
    }
    out
}

// r[verify sched.db.batch-unnest]
/// `promote_newly_ready` is internally batched: completing a leaf with
/// 150 Queued parents transitions all 150 to Ready and persists in ONE
/// batch (no per-item PG round-trip). Correctness check here; batching
/// is structurally enforced (the helper has exactly one
/// `persist_status_batch` call).
#[tokio::test]
async fn test_high_fanin_completion_batches_ready() -> TestResult {
    const N: usize = 150;
    let (db, handle, _task, mut rx) = setup_with_worker("fanin-w", "x86_64-linux").await?;

    let mut nodes = vec![make_node("fanin-leaf")];
    let mut edges = Vec::with_capacity(N);
    for i in 0..N {
        let tag = format!("fanin-p{i:03}");
        nodes.push(make_node(&tag));
        edges.push(make_test_edge(&tag, "fanin-leaf"));
    }
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, edges, true).await?;
    let _ = recv_assignment(&mut rx).await;
    complete_success_empty(&handle, "fanin-w", &test_drv_path("fanin-leaf")).await?;
    barrier(&handle).await;

    // All N parents Ready in-mem AND in PG.
    for i in 0..N {
        let info = expect_drv(&handle, &format!("fanin-p{i:03}")).await;
        assert_eq!(info.status, DerivationStatus::Ready, "parent {i}");
    }
    let n_ready: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM derivations WHERE drv_hash LIKE 'fanin-p%' AND status = 'ready'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(n_ready as usize, N, "all parents persisted Ready");
    Ok(())
}

// r[verify sched.db.batch-unnest]
// r[verify sched.event.derivation-terminal]
/// `cascade_dependency_failure` collects-then-batches AND emits a
/// `DerivationFailed{DependencyFailed}` event per cascaded ancestor.
/// Permanent-fail the leaf of a 30-deep chain; all 29 ancestors go
/// DependencyFailed in PG and the WatchBuild stream sees one Failed
/// event for the leaf + 29 for the chain.
#[tokio::test]
async fn test_cascade_emits_failed_per_node() -> TestResult {
    const N: usize = 30;
    let (db, handle, _task, mut rx) = setup_with_worker("casc-w", "x86_64-linux").await?;

    let nodes: Vec<_> = (0..N).map(|i| make_node(&format!("casc{i:02}"))).collect();
    let edges: Vec<_> = (0..N - 1)
        .map(|i| make_test_edge(&format!("casc{:02}", i + 1), &format!("casc{i:02}")))
        .collect();
    let mut ev = merge_dag(&handle, Uuid::new_v4(), nodes, edges, true).await?;
    let _ = recv_assignment(&mut rx).await;
    complete_failure(
        &handle,
        "casc-w",
        &test_drv_path("casc00"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "leaf busted",
    )
    .await?;
    barrier(&handle).await;

    let n_dep_failed: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM derivations WHERE drv_hash LIKE 'casc%' \
         AND status = 'dependency_failed'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        n_dep_failed as usize,
        N - 1,
        "all ancestors persisted DependencyFailed"
    );

    let drv_events = drain_derivation_events(&mut ev);
    let dep_failed = rio_proto::types::BuildResultStatus::DependencyFailed as i32;
    let perm_failed = rio_proto::types::BuildResultStatus::PermanentFailure as i32;
    let failed_kind = rio_proto::types::DerivationEventKind::Failed as i32;
    let trigger_count = drv_events
        .iter()
        .filter(|d| d.kind == failed_kind && d.failure_status == perm_failed)
        .count();
    let cascaded_count = drv_events
        .iter()
        .filter(|d| d.kind == failed_kind && d.failure_status == dep_failed)
        .count();
    assert_eq!(
        trigger_count, 1,
        "one PermanentFailure for the trigger leaf"
    );
    assert_eq!(
        cascaded_count,
        N - 1,
        "one DependencyFailed event per cascaded ancestor"
    );
    // Cascaded error_msg names the trigger.
    let leaf_path = test_drv_path("casc00");
    assert!(
        drv_events
            .iter()
            .filter(|d| d.failure_status == dep_failed)
            .all(|d| d.error_message.contains(&leaf_path)),
        "cascaded events name the failed dependency"
    );
    Ok(())
}

// r[verify sched.event.derivation-terminal]
/// Poison-via-max_retries-exhaustion emits `DerivationFailed` and
/// triggers log flush. Previously `poison_and_cascade` passed
/// `event=None` so the event + flush were silently skipped.
#[tokio::test]
async fn test_poison_via_max_retries_emits_failed_event() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.max_retries = 0; // first transient → poison
        c.retry_policy.backoff_base_secs = 0.0;
    });
    let _db = db;
    // Padding so fleet-exhaust doesn't fire (different poison reason).
    let _rx1 = connect_executor(&handle, "pmax-w1", "x86_64-linux").await?;
    let _rx2 = connect_executor(&handle, "pmax-w2", "x86_64-linux").await?;
    let _rx3 = connect_executor(&handle, "pmax-w3", "x86_64-linux").await?;

    let mut ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "pmax-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;
    let first = expect_drv(&handle, "pmax-drv")
        .await
        .assigned_executor
        .expect("assigned");
    complete_failure(
        &handle,
        &first,
        &test_drv_path("pmax-drv"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        "boom",
    )
    .await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "pmax-drv").await.status,
        DerivationStatus::Poisoned
    );
    let drv_events = drain_derivation_events(&mut ev);
    let failed_kind = rio_proto::types::DerivationEventKind::Failed as i32;
    let failed: Vec<_> = drv_events
        .iter()
        .filter(|d| d.kind == failed_kind)
        .collect();
    assert_eq!(
        failed.len(),
        1,
        "exactly one DerivationFailed for poison-via-exhaustion"
    );
    assert!(
        failed[0].error_message.contains("max_retries"),
        "error_message names the poison reason: {:?}",
        failed[0].error_message
    );
    Ok(())
}

/// `handle_transient_failure` retry path emits BuildProgress (C2c) so
/// the dashboard sees `running_count` drop during the backoff window.
#[tokio::test]
async fn test_transient_retry_emits_progress() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("prog-w", "x86_64-linux").await?;

    let mut ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "prog-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    // Padding so fleet-exhaust doesn't poison on the single failure.
    // Connect AFTER merge so dispatch deterministically picked prog-w.
    let _rx2 = connect_executor(&handle, "prog-w2", "x86_64-linux").await?;
    let _rx3 = connect_executor(&handle, "prog-w3", "x86_64-linux").await?;
    let _ = recv_assignment(&mut rx).await;
    barrier(&handle).await;
    // Drain pre-failure events and clear the 250ms emit_progress
    // debounce so the retry-path emit isn't suppressed.
    while ev.try_recv().is_ok() {}
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    complete_failure(
        &handle,
        "prog-w",
        &test_drv_path("prog-drv"),
        rio_proto::types::BuildResultStatus::TransientFailure,
        "flake",
    )
    .await?;
    barrier(&handle).await;

    // Default backoff_base_secs=5.0 → still Ready (not re-dispatched).
    let info = expect_drv(&handle, "prog-drv").await;
    assert_eq!(info.status, DerivationStatus::Ready);
    assert_eq!(info.retry.count, 1);
    // A BuildProgress event was emitted post-failure.
    let mut saw_progress = false;
    while let Ok(e) = ev.try_recv() {
        if matches!(
            e.event,
            Some(rio_proto::types::build_event::Event::Progress(_))
        ) {
            saw_progress = true;
        }
    }
    assert!(
        saw_progress,
        "transient retry must emit BuildProgress (running_count dropped)"
    );
    Ok(())
}

// r[verify gw.activity.progress-before-stop]
/// Success completion emits Progress (with this drv counted) BEFORE
/// the per-drv `DerivationEvent::Completed`, and Completed BEFORE the
/// build-level `BuildCompleted`. nom marks an actBuild ✔ only when
/// `Progress.done` increments while the activity is still open
/// (native nix `Goal::done()` ordering: parent counter before
/// `Activity` destructor). With the previous Completed-then-Progress
/// order, every drv except the last stayed ⏵ in nom.
#[tokio::test]
async fn test_progress_precedes_drv_completed_on_state_channel() -> TestResult {
    use rio_proto::types::{DerivationEventKind, build_event::Event};

    let (_db, handle, _task, _rx) = setup_with_worker("ord-w", "x86_64-linux").await?;
    let build_id = Uuid::new_v4();
    let mut ev = merge_single_node(&handle, build_id, "ord-drv", PriorityClass::Scheduled).await?;

    // Drain dispatch-time events; stop once we see the dispatch-phase
    // Progress (DrvStarted precedes it per dispatch.rs ordering).
    loop {
        let e = tokio::time::timeout(Duration::from_secs(5), ev.recv())
            .await
            .expect("dispatch event within 5s")?;
        if matches!(e.event, Some(Event::Progress(_))) {
            break;
        }
    }

    complete_success_empty(&handle, "ord-w", &test_drv_path("ord-drv")).await?;
    barrier(&handle).await;

    // Collect post-completion events in arrival order; record positions
    // of the FIRST Progress with completed≥1, the DrvCompleted, and
    // BuildCompleted.
    let (mut p_pos, mut d_pos, mut b_pos) = (None, None, None);
    let mut i = 0usize;
    while let Ok(e) = ev.try_recv() {
        match e.event {
            Some(Event::Progress(p)) if p.completed >= 1 && p_pos.is_none() => p_pos = Some(i),
            Some(Event::Derivation(d)) if d.kind() == DerivationEventKind::Completed => {
                assert!(d_pos.is_none(), "DrvCompleted emitted more than once");
                d_pos = Some(i);
            }
            Some(Event::Completed(_)) => b_pos = Some(i),
            _ => {}
        }
        i += 1;
    }
    let p = p_pos.expect("Progress with completed≥1 not emitted");
    let d = d_pos.expect("DerivationEvent::Completed not emitted");
    let b = b_pos.expect("BuildCompleted not emitted");
    assert!(
        p < d,
        "Progress(completed≥1) must precede DerivationCompleted: positions {p} vs {d}"
    );
    assert!(
        d < b,
        "DerivationCompleted must precede BuildCompleted: positions {d} vs {b}"
    );
    Ok(())
}

// r[verify sched.build.keep-going]
/// `keep_going=true` build's eventual `BuildFailed` records the FIRST
/// failed derivation. Previously `error_summary`/`failed_derivation`
/// were only set in the `!keep_going` branch → empty strings.
#[tokio::test]
async fn test_keep_going_build_failed_records_first_failure() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("kg-w", "x86_64-linux").await?;

    // Two independent derivations under keep_going; fail one,
    // succeed the other.
    let nodes = vec![make_node("kg-fail"), make_node("kg-ok")];
    let mut ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], true).await?;
    barrier(&handle).await;

    handle.debug_force_assign("kg-fail", "kg-w").await?;
    let _ = recv_assignment(&mut rx).await;
    complete_failure(
        &handle,
        "kg-w",
        &test_drv_path("kg-fail"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "boom",
    )
    .await?;
    barrier(&handle).await;
    let _rx2 = connect_executor(&handle, "kg-w2", "x86_64-linux").await?;
    handle.debug_force_assign("kg-ok", "kg-w2").await?;
    complete_success_empty(&handle, "kg-w2", &test_drv_path("kg-ok")).await?;
    barrier(&handle).await;

    // BuildFailed event must name kg-fail.
    let mut saw_failed = false;
    while let Ok(e) = ev.try_recv() {
        if let Some(rio_proto::types::build_event::Event::Failed(f)) = e.event {
            saw_failed = true;
            assert!(
                f.failed_derivation.contains("kg-fail"),
                "failed_derivation must name the first-failed drv: {f:?}"
            );
            assert!(!f.error_message.is_empty(), "error_message non-empty");
        }
    }
    assert!(saw_failed, "keep_going build must emit BuildFailed");
    Ok(())
}

/// Unsolicited `Cancelled` from a worker (DAG status NOT Cancelled) is
/// treated as infrastructure failure — drv resets to Ready,
/// `infra_count` bumps. Previously the match arm was a comment-only
/// no-op, leaving the drv stuck after the slot was freed.
#[tokio::test]
async fn test_unsolicited_cancelled_resets_to_ready() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("ucan-w", "x86_64-linux").await?;

    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ucan-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    let _ = recv_assignment(&mut rx).await;
    let pre = expect_drv(&handle, "ucan-drv").await;
    assert_eq!(pre.status, DerivationStatus::Assigned);
    assert_eq!(pre.retry.infra_count, 0);

    complete_failure(
        &handle,
        "ucan-w",
        &test_drv_path("ucan-drv"),
        rio_proto::types::BuildResultStatus::Cancelled,
        "rogue",
    )
    .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, "ucan-drv").await;
    assert_eq!(
        post.status,
        DerivationStatus::Ready,
        "unsolicited Cancelled → reset_to_ready (NOT stuck Assigned)"
    );
    assert_eq!(
        post.retry.infra_count, 1,
        "treated as infrastructure failure"
    );
    Ok(())
}

// r[verify sched.retry.per-executor-budget]
/// `max_infra_retries` is a uniform bound: at-cap cgroup-OOM and
/// non-floor infra (FUSE EIO) poison at the SAME attempt number.
/// Previously `bump_floor_or_count` incremented BEFORE the cap check
/// for at-cap, so it poisoned one attempt earlier.
#[rstest]
#[case::at_cap_oom(rio_proto::CGROUP_OOM_MSG, true)]
#[case::fuse_eio("FUSE EIO on lower mount", false)]
#[tokio::test]
async fn test_infra_retry_cap_uniform_across_reasons(
    #[case] error_msg: &str,
    #[case] at_cap_oom: bool,
) -> TestResult {
    const MAX: u32 = 3;
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.max_infra_retries = MAX;
    });
    let _db = db;
    let _rx = connect_executor(&handle, "uni-w", "x86_64-linux").await?;
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "uni-drv", PriorityClass::Scheduled).await?;
    barrier(&handle).await;

    if at_cap_oom {
        // Pin floor.mem at the ceiling so every cgroup-OOM is at-cap.
        handle
            .debug_seed_sched_hint(
                "uni-drv",
                None,
                None,
                None,
                Some(crate::state::ResourceFloor {
                    mem_bytes: u64::MAX,
                    ..Default::default()
                }),
            )
            .await?;
    }

    let p = test_drv_path("uni-drv");
    // MAX failures → NOT poisoned yet (cap-check is BEFORE increment).
    for i in 0..MAX {
        handle.debug_force_assign("uni-drv", "uni-w").await?;
        complete_failure(
            &handle,
            "uni-w",
            &p,
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            error_msg,
        )
        .await?;
        barrier(&handle).await;
        let info = expect_drv(&handle, "uni-drv").await;
        assert_ne!(
            info.status,
            DerivationStatus::Poisoned,
            "attempt {} of {MAX}: NOT poisoned yet ({error_msg})",
            i + 1
        );
    }
    // MAX+1-th failure → poison.
    handle.debug_force_assign("uni-drv", "uni-w").await?;
    complete_failure(
        &handle,
        "uni-w",
        &p,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        error_msg,
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "uni-drv").await.status,
        DerivationStatus::Poisoned,
        "attempt {}: poisoned ({error_msg})",
        MAX + 1
    );
    Ok(())
}

/// `build_derivations.exec_id` is set on `Completed` to the `exec_id`
/// the build actually observed for this derivation, so the dashboard
/// build view can fetch the
/// exact `drv_logs` row instead of falling back to "latest exec for
/// this drv" — which is wrong after a retry or a later build's rebuild
/// of the same drv.
///
/// Uses the `exec_id` from the `WorkAssignment` (the wire carrier) as
/// the expected value — that's the same UUIDv7 minted in
/// `assign_to_worker` and stamped on `DerivationState`,
/// `assignments.exec_id`, and the `LogBuffers` ring buffer entry. The
/// completion handler reads `state.exec_id`; if any carrier disagrees
/// this test would catch it as a mismatch.
///
/// The write is fire-and-forget (`spawn_monitored`), so the test polls
/// PG with the established 10ms × 100 pattern rather than asserting
/// immediately after `barrier()` (which only drains the actor loop, not
/// background tasks).
// r[verify sched.merge.exec-correlation+7]
#[tokio::test]
async fn completion_records_build_exec_correlation() -> TestResult {
    let (db, handle, _task, mut stream_rx) = setup_with_worker("ec-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let node = make_node("ec-drv");
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Dispatch happened — capture the minted exec_id from the wire
    // carrier. This is the same UUIDv7 stamped on `state.exec_id` that
    // the completion handler will read.
    let assignment = recv_assignment(&mut stream_rx).await;
    let expected_exec: Uuid = assignment
        .exec_id
        .parse()
        .expect("WorkAssignment.exec_id must be a UUID after dispatch");

    // Precondition: build_derivations.exec_id is NULL before completion
    // (set on terminal, not on dispatch). Asserting the precondition
    // makes the post-completion check non-vacuous.
    let pre: Option<Uuid> = sqlx::query_scalar(
        "SELECT bd.exec_id FROM build_derivations bd \
         JOIN derivations d ON d.derivation_id = bd.derivation_id \
         WHERE bd.build_id = $1 AND d.drv_path = $2",
    )
    .bind(build_id)
    .bind(test_drv_path("ec-drv"))
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        pre, None,
        "exec_id NULL before completion (set on terminal)"
    );

    complete_success(
        &handle,
        "ec-worker",
        &test_drv_path("ec-drv"),
        &test_store_path("ec-out"),
    )
    .await?;
    barrier(&handle).await;

    // Poll PG: the exec-correlation write is spawned (fire-and-forget),
    // so it's not done when barrier() returns. Established 10ms × 100
    // pattern (see helpers::wait_for_status); the write is one UPDATE
    // and should land within a few ticks.
    let mut got: Option<Uuid> = None;
    for _ in 0..100 {
        got = sqlx::query_scalar(
            "SELECT bd.exec_id FROM build_derivations bd \
             JOIN derivations d ON d.derivation_id = bd.derivation_id \
             WHERE bd.build_id = $1 AND d.drv_path = $2",
        )
        .bind(build_id)
        .bind(test_drv_path("ec-drv"))
        .fetch_one(&db.pool)
        .await?;
        if got.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        got,
        Some(expected_exec),
        "build_derivations.exec_id records the execution this build \
         observed (the WorkAssignment.exec_id sent at dispatch)"
    );
    Ok(())
}

/// `reset_to_ready()` (worker disconnect, phantom drain, orphan
/// reconcile) clears `state.exec_id` WITHOUT discarding the
/// `LogBuffers` entry — the entry's lines and stamped exec_id are
/// still needed by the periodic flusher in the disconnect→re-dispatch
/// window. If a poison path (I-065 fleet exhaustion, max_infra/timeout
/// retries cap) reaches `terminal_failure_epilogue` BEFORE the next
/// `assign_to_worker` re-stamps, `state.exec_id` is `None` while the
/// buffer holds the disconnected execution's lines.
///
/// The epilogue's single resolution MUST fall back to the buffer's
/// stamped exec_id so the execution that DID run gets its final
/// `.log.zst` blob, `drv_logs` row marked
/// `is_complete=true`/`status='failed'`, and `record_exec_correlation`
/// gets a non-NULL `build_derivations.exec_id`. `trigger_log_flush`
/// receives the resolved value as a parameter.
///
/// Pre-fix: the resolution early-returned on `state.exec_id == None`,
/// no `FlushRequest` queued.
///
/// Verifies the actor-side carrier resolution that feeds the
/// exec-keyed storage; the storage-key shape itself is verified in
/// `logs/flush.rs` and `admin/tests/logs_tests.rs`.
///
/// r[verify obs.log.exec-keyed]
#[tokio::test]
async fn flush_falls_back_to_buffer_exec_id_after_reset_to_ready() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(log_buffers.clone()),
            log_flush_tx: Some(flush_tx),
            ..Default::default()
        },
    );

    // Inject a Ready node. `RecoveryDerivationRow::test_default` leaves
    // `exec_id: None` — the shape `reset_to_ready()` produces after a
    // worker disconnect, before re-dispatch.
    actor.test_inject_ready("fb-drv", None, "x86_64-linux", false);
    let drv_path = test_drv_path("fb-drv");

    // Pre-stamp the LogBuffers entry as `assign_to_worker` did before
    // the disconnect. `reset_to_ready()` does NOT discard this entry —
    // only `rollback_assignment` and the next `assign_to_worker` do.
    let exec_id = uuid::Uuid::now_v7();
    log_buffers.set_exec(&drv_path, exec_id, "old-worker");
    assert!(
        log_buffers.push_for(
            &drv_path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: drv_path.clone(),
                lines: vec![b"line from disconnected execution".to_vec()],
                first_line_number: 0,
                executor_id: "old-worker".into(),
            },
            "old-worker",
        ),
        "push_for should accept a batch from the stamped executor"
    );

    // Drive the full epilogue (the resolution now lives there, not in
    // trigger_log_flush). Pre-fix the resolution early-returned on
    // `state.exec_id == None`; post-fix it falls back to the buffer's
    // stamp and threads the value to the flush.
    actor.terminal_log_epilogue(&DrvHash::from("fb-drv"), "failed", &[]);

    let req = flush_rx.try_recv().expect(
        "the epilogue's resolution should fall back to the LogBuffers \
         entry's exec_id when state.exec_id is None (reset_to_ready \
         cleared it before poison-while-Ready reached \
         terminal_failure_epilogue)",
    );
    assert_eq!(
        req.exec_id, exec_id,
        "request pins the buffer's stamped exec_id"
    );
    assert_eq!(req.drv_path, drv_path);
    assert_eq!(req.status.as_deref(), Some("failed"));
    // Inverse of `epilogue_skips_never_dispatched_drv`: a drv WITH a
    // resolvable execution still gets sealed.
    assert!(
        log_buffers.is_sealed(&drv_path),
        "a drv with a resolvable execution must still be sealed"
    );
    Ok(())
}

/// A never-dispatched drv (no exec_id from either carrier, no LogBuffers
/// entry) reaching `terminal_log_epilogue` must be a complete no-op:
/// no FlushRequest, no `bd.exec_id` write, and — the regression — no
/// seal tombstone. `seal()` inserts into `LogBuffers::sealed`
/// unconditionally and the only reactive removal is `flush_final`'s
/// post-drain `unseal()`, which never runs if no request is enqueued;
/// an ungated seal therefore lingers until `CleanupTerminalBuild` reaps
/// the build (hours under --keep-going) and inflates `sealed_count()`,
/// the documented leak signal.
///
/// Production shape: `handle_substitute_complete`'s ok=false revert on a
/// first-attempt Substituting drv whose dep is Poisoned
/// (`revert_target_for` → DependencyFailed → `terminal_failure_epilogue`
/// → this epilogue), and `poison_and_cascade` on a Ready drv whose prior
/// buffer is gone. Both call the epilogue ungated; the gate must live
/// inside it.
///
/// r[verify sched.merge.exec-correlation+7]
/// r[verify obs.log.exec-keyed]
#[tokio::test]
async fn epilogue_skips_never_dispatched_drv() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(log_buffers.clone()),
            log_flush_tx: Some(flush_tx),
            ..Default::default()
        },
    );

    // Ready node, no LogBuffers entry, state.exec_id None — never dispatched.
    actor.test_inject_ready("nd-drv", None, "x86_64-linux", false);
    let drv_path = test_drv_path("nd-drv");

    // `interested_builds` deliberately non-empty so the correlate skip is
    // attributable to the exec_id gate, not the `is_empty()` early-return.
    actor.terminal_log_epilogue(&DrvHash::from("nd-drv"), "failed", &[Uuid::new_v4()]);

    // (1) No FlushRequest. Also proves no bd.exec_id UPDATE was issued:
    // flush and correlate are gated by the same single resolution, so a
    // skipped flush implies a skipped correlate (a direct NULL-check on
    // build_derivations would need a flat sleep against a spawned write —
    // a negative proven by sleeping can only false-pass under load).
    assert!(
        flush_rx.try_recv().is_err(),
        "never-dispatched drv must not enqueue a FlushRequest"
    );
    // (2) THE REGRESSION: no seal tombstone for a buffer that never
    // existed. Pre-fix, seal_log_buffer ran before the gate.
    assert!(
        !log_buffers.is_sealed(&drv_path),
        "never-dispatched drv must not leave a seal tombstone (nothing will ever unseal it)"
    );
    // Key-independent backstop: even if the assertion key above ever
    // diverged from the key the actor seals (the drv_log_hash
    // fixture-collision trap), an empty set cannot hide a tombstone.
    assert_eq!(
        log_buffers.sealed_count(),
        0,
        "sealed_count is the leak signal"
    );
    Ok(())
}

/// The correlate step must record the buffer-fallback exec_id when
/// `state.exec_id` is `None`. Post-fix this also pins Facet B's
/// structural fix: the correlate receives the exec_id resolved
/// *before* `trigger_log_flush` woke the flusher, so a flusher that
/// drains the buffer between the two steps cannot null out the
/// correlation. (The race itself is not deterministically testable —
/// same stance as `has_buffered_exec_log`'s "no test can
/// deterministically exercise the window" note — but the value's
/// provenance is now a function parameter, not a re-read.)
///
/// Seeds `derivations`/`builds`/`build_derivations` directly via SQL
/// (the established pattern in `admin/tests/graph_tests.rs`) so the
/// in-memory node's `db_id` matches a real row the UPDATE can hit —
/// `test_inject_ready` alone does not persist to PG.
///
/// Pre-fix: the per-step resolution early-returned on
/// `state.exec_id == None`, no UPDATE, `build_derivations.exec_id`
/// stays NULL forever.
///
/// r[verify sched.merge.exec-correlation+7]
#[tokio::test]
async fn exec_correlation_falls_back_to_buffer_exec_id() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Seed the rows record_exec_correlation's UPDATE targets.
    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'ready') \
         RETURNING derivation_id",
    )
    .bind("ec-fb-drv")
    .bind(test_drv_path("ec-fb-drv"))
    .fetch_one(&db.pool)
    .await?;
    let build_id = Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
        .bind(build_id)
        .execute(&db.pool)
        .await?;
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(derivation_id)
        .execute(&db.pool)
        .await?;

    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(log_buffers.clone()),
            ..Default::default()
        },
    );

    // Inject the Ready node with the SAME derivation_id as the seeded
    // row — `from_recovery_row` maps it to `state.db_id`. `exec_id`
    // stays `None` (test_default) — the post-`reset_to_ready` shape.
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id,
        ..crate::db::RecoveryDerivationRow::test_default("ec-fb-drv", "x86_64-linux")
    });

    // Pre-stamp the LogBuffers entry as `assign_to_worker` did before
    // the disconnect.
    let exec_id = uuid::Uuid::now_v7();
    log_buffers.set_exec(&test_drv_path("ec-fb-drv"), exec_id, "old-worker");

    // Drive the full epilogue. Pre-fix the per-step resolution
    // early-returned on `state.exec_id == None`, no UPDATE.
    actor.terminal_log_epilogue(&DrvHash::from("ec-fb-drv"), "failed", &[build_id]);

    // Poll PG: the exec-correlation write is spawned (fire-and-forget).
    // Established 10ms × 100 pattern (see helpers::wait_for_status).
    let mut got: Option<Uuid> = None;
    for _ in 0..100 {
        got = sqlx::query_scalar(
            "SELECT exec_id FROM build_derivations \
             WHERE build_id = $1 AND derivation_id = $2",
        )
        .bind(build_id)
        .bind(derivation_id)
        .fetch_one(&db.pool)
        .await?;
        if got.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        got,
        Some(exec_id),
        "record_exec_correlation should fall back to the LogBuffers \
         entry's exec_id when state.exec_id is None"
    );
    Ok(())
}

/// A finished build's `bd.exec_id` is final. Its `interested_builds`
/// membership outlives its completion by TERMINAL_CLEANUP_DELAY (~60s)
/// — `complete_build` only *schedules* the interest removal — so a drv
/// reset out of a terminal state (I-047 GC'd-output reset, I-094
/// reprobe) and re-completed inside that window fires
/// `record_exec_correlation` again with the finished build still in
/// the fan-out. The UPDATE must not revise the observation it recorded
/// when the build was active (`AND exec_id IS NULL` — write-once per
/// `(build, drv)`). The NULL-rowed sibling in the same fan-out is the
/// positive control: it must get the new exec_id (proves the guard is
/// per-row, not a dropped statement).
///
/// Pre-fix: the UPDATE binds every interested build unconditionally;
/// B1's row is overwritten X1 → X2 and the dashboard presents X2's log
/// as the exact log B1 observed (the "approximate" banner gates on
/// `execId === ''`, not on correctness).
///
/// r[verify sched.merge.exec-correlation+7]
#[tokio::test]
async fn exec_correlation_skips_terminal_builds() -> TestResult {
    use crate::state::{BuildInfo, BuildState};

    let db = TestDb::new(&MIGRATOR).await;

    // One drv, two builds. B1 finished and already recorded X1; B2 is
    // still active and NULL.
    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'ready') \
         RETURNING derivation_id",
    )
    .bind("ec-term-drv")
    .bind(test_drv_path("ec-term-drv"))
    .fetch_one(&db.pool)
    .await?;
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();
    let x1 = uuid::Uuid::now_v7();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'succeeded'), ($2, 'active')")
        .bind(b1)
        .bind(b2)
        .execute(&db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO build_derivations (build_id, derivation_id, exec_id) \
         VALUES ($1, $3, $4), ($2, $3, NULL)",
    )
    .bind(b1)
    .bind(b2)
    .bind(derivation_id)
    .bind(x1)
    .execute(&db.pool)
    .await?;

    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(log_buffers.clone()),
            ..Default::default()
        },
    );

    // The re-executed node (post-reset, re-dispatched as X2).
    // `state.exec_id = Some(x2)` is the carrier `exec_id_for_terminal`
    // reads first.
    let x2 = uuid::Uuid::now_v7();
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id,
        exec_id: Some(x2),
        ..crate::db::RecoveryDerivationRow::test_default("ec-term-drv", "x86_64-linux")
    });

    // Register both builds in the actor's map: B1 terminal, B2 active.
    // Pending → Active → Succeeded is the validated path
    // (Pending → Succeeded directly is rejected by validate_transition).
    let mk = |bid: Uuid| {
        BuildInfo::new_pending(
            bid,
            None,
            PriorityClass::Scheduled,
            false,
            BuildOptions::default(),
            std::iter::once(DrvHash::from("ec-term-drv")).collect(),
        )
    };
    let mut info1 = mk(b1);
    info1.transition(BuildState::Active).unwrap();
    info1.transition(BuildState::Succeeded).unwrap();
    actor.builds.insert(b1, info1);
    let mut info2 = mk(b2);
    info2.transition(BuildState::Active).unwrap();
    actor.builds.insert(b2, info2);

    // The re-completion's fan-out still contains the finished B1.
    actor.record_exec_correlation(&DrvHash::from("ec-term-drv"), x2, &[b1, b2]);

    // Wait for the write that SHOULD happen (B2 → X2), then check the
    // one that shouldn't (B1 stays X1). Established 10ms × 100 poll —
    // the exec-correlation write is spawned (fire-and-forget). Polling
    // for B1 to *stay* X1 would be a timing-dependent absence
    // assertion; anchoring on B2's positive write makes B1's read
    // deterministic (same UPDATE statement, so by the time B2's row is
    // visible the statement has committed in full).
    let mut b2_got: Option<Uuid> = None;
    for _ in 0..100 {
        b2_got = sqlx::query_scalar(
            "SELECT exec_id FROM build_derivations \
             WHERE build_id = $1 AND derivation_id = $2",
        )
        .bind(b2)
        .bind(derivation_id)
        .fetch_one(&db.pool)
        .await?;
        if b2_got.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        b2_got,
        Some(x2),
        "build with no recorded observation in the same fan-out must \
         still get the new exec_id (positive control: the write-once \
         guard is per-row, not a dropped statement)"
    );
    let b1_got: Option<Uuid> = sqlx::query_scalar(
        "SELECT exec_id FROM build_derivations \
         WHERE build_id = $1 AND derivation_id = $2",
    )
    .bind(b1)
    .bind(derivation_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        b1_got,
        Some(x1),
        "a finished build's recorded observation is final — a post-completion \
         re-execution of the drv must not overwrite bd.exec_id"
    );
    Ok(())
}

/// A cascaded `DependencyFailed` ancestor that was dispatched, reset on
/// worker disconnect (`reset_to_ready()` clears `state.exec_id`,
/// retains the stamped `LogBuffers` entry), and swept by
/// `cascade_dependency_failure` MUST have that retained execution
/// finalized — seal, flush (`status="cancelled"`, `is_complete=true`,
/// `.partial` swap), correlate to the ancestor's OWN interested builds —
/// exactly as the build-cancel sweep's `to_depfail` arm does for the
/// same `Queued|Ready → DependencyFailed` transition (both now via
/// `finalize_buffered_exec_log`). The never-dispatched trigger is the
/// negative control: no buffer, no FlushRequest, no seal tombstone.
///
/// Pre-fix: `terminal_failure_epilogue` finalized only the trigger, on
/// the (false) claim that cascaded ancestors "never executed".
///
/// r[verify sched.merge.exec-correlation+7]
/// r[verify obs.log.exec-keyed]
#[tokio::test]
async fn cascade_finalizes_reset_ancestor_exec_log() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Seed the rows record_exec_correlation's UPDATE targets for the
    // PARENT (the cascaded node). The trigger's row is seeded too —
    // not for correlation (it never dispatched, its epilogue is a
    // no-op) but so persist_poisoned/unpin_best_effort hit a real row
    // instead of logging best-effort errors.
    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'ready') \
         RETURNING derivation_id",
    )
    .bind("cflz-parent")
    .bind(test_drv_path("cflz-parent"))
    .fetch_one(&db.pool)
    .await?;
    let trigger_path = "/nix/store/cccccccccccccccccccccccccccccccc-cflz-trigger.drv".to_string();
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'ready')",
    )
    .bind("cflz-trigger")
    .bind(&trigger_path)
    .execute(&db.pool)
    .await?;
    let build_id = Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
        .bind(build_id)
        .execute(&db.pool)
        .await?;
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(derivation_id)
        .execute(&db.pool)
        .await?;

    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(log_buffers.clone()),
            log_flush_tx: Some(flush_tx),
            ..Default::default()
        },
    );

    // The trigger: Ready, never dispatched, no buffer. Distinct
    // store-path hash so its LogBuffers key cannot alias onto the
    // parent's stamped entry (drv_log_hash keys on the hash component;
    // test_drv_path stamps the same TEST_HASH into every fixture).
    // Its interested set is a build the PARENT is not interested in —
    // if the cascade finalization used the trigger's set instead of
    // the cascaded node's own, assertion (4) would target a
    // nonexistent bd row and time out at NULL.
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        drv_path: trigger_path.clone(),
        ..crate::db::RecoveryDerivationRow::test_default("cflz-trigger", "x86_64-linux")
    });
    actor
        .dag
        .node_mut("cflz-trigger")
        .expect("just injected")
        .interested_builds
        .insert(Uuid::new_v4());

    // The parent (depends on the trigger): the post-reset shape —
    // Queued (I-047 demotion), state.exec_id None (reset_to_ready
    // cleared it), LogBuffers entry still stamped with the reset
    // execution's exec_id and holding its streamed lines.
    let exec_id = uuid::Uuid::now_v7();
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id,
        ..crate::db::RecoveryDerivationRow::test_default("cflz-parent", "x86_64-linux")
    });
    {
        let s = actor.dag.node_mut("cflz-parent").expect("just injected");
        s.set_status_for_test(DerivationStatus::Queued);
        s.interested_builds.insert(build_id);
    }
    actor.test_inject_edge("cflz-parent", "cflz-trigger");
    let parent_path = test_drv_path("cflz-parent");
    log_buffers.set_exec(&parent_path, exec_id, "old-worker");
    assert!(log_buffers.push_for(
        &parent_path,
        &rio_proto::types::BuildLogBatch {
            derivation_path: parent_path.clone(),
            lines: vec![b"line from the reset execution".to_vec()],
            first_line_number: 0,
            executor_id: "old-worker".into(),
        },
        "old-worker",
    ));

    // Poison the never-dispatched trigger (Ready → Poisoned is the
    // direct I-065 fleet-exhaustion arm; poison_and_cascade validates
    // the precondition, transitions, then runs
    // terminal_failure_epilogue → cascade_dependency_failure).
    actor
        .poison_and_cascade(&DrvHash::from("cflz-trigger"), "leaf busted")
        .await;

    // (1) Exactly one FlushRequest — the parent's retained exec, with
    // the scheduler's "cancelled" disposition. The never-dispatched
    // trigger contributes none (its epilogue self-gates on a missing
    // exec_id).
    let req = flush_rx.try_recv().expect(
        "the cascade must finalize a swept ancestor's retained \
         prior-exec log instead of leaving its drv_logs row at \
         is_complete=false for the 30-day TTL",
    );
    assert_eq!(
        req.exec_id, exec_id,
        "request pins the retained buffer's stamp"
    );
    assert_eq!(req.drv_path, parent_path);
    assert_eq!(req.status.as_deref(), Some("cancelled"));
    assert!(
        flush_rx.try_recv().is_err(),
        "the never-dispatched trigger has no exec to flush"
    );

    // (2) Seal discipline: the parent's buffer is sealed (late batches
    // from the lost worker can't recreate it); the trigger gets no
    // tombstone. sealed_count is the key-independent leak signal.
    assert!(log_buffers.is_sealed(&parent_path));
    assert!(!log_buffers.is_sealed(&trigger_path));
    assert_eq!(log_buffers.sealed_count(), 1);

    // (3) Both reached the right terminal — the finalization didn't
    // perturb the transitions.
    assert_eq!(
        actor.dag.node("cflz-trigger").expect("in DAG").status(),
        DerivationStatus::Poisoned
    );
    assert_eq!(
        actor.dag.node("cflz-parent").expect("in DAG").status(),
        DerivationStatus::DependencyFailed
    );

    // (4) bd.exec_id recorded for the build that observed the parent's
    // reset execution — keyed on the PARENT's own interested build
    // (spawned write; established 10ms × 100 poll).
    let mut got: Option<Uuid> = None;
    for _ in 0..100 {
        got = sqlx::query_scalar(
            "SELECT exec_id FROM build_derivations \
             WHERE build_id = $1 AND derivation_id = $2",
        )
        .bind(build_id)
        .bind(derivation_id)
        .fetch_one(&db.pool)
        .await?;
        if got.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        got,
        Some(exec_id),
        "cascade must record bd.exec_id for a swept ancestor's \
         observed execution, against the ancestor's own interested set"
    );
    Ok(())
}

// r[verify sched.build.terminal-status-settled+2]
/// A build that already reached a terminal state must not receive further
/// `BuildProgress` events from shared-node completion fan-outs: a
/// keep_going build fails, stays resident with its interest retained, the
/// failed node is resubmitted by another build (carrying that interest),
/// and when the node then completes only the live build gets Progress —
/// the terminal build's event stream stays settled after its BuildFailed
/// and QueryBuildStatus keeps serving the frozen counts.
#[tokio::test]
async fn test_terminal_build_skipped_by_completion_progress_fanout() -> TestResult {
    use rio_proto::types::build_event::Event;
    let (_db, handle, _task, mut wrx) = setup_with_worker("tsf-w", "x86_64-linux").await?;
    let x_path = test_drv_path("tsf-x");
    let x_out = test_store_path("tsf-x-out");

    // B1 (keep_going): single node fails permanently → build Failed,
    // interest retained on the poisoned node.
    let b1 = Uuid::new_v4();
    let mut x = make_node("tsf-x");
    x.expected_output_paths = vec![x_out.clone()];
    let mut rx1 = merge_dag(&handle, b1, vec![x], vec![], true).await?;
    let assn = recv_assignment(&mut wrx).await;
    assert_eq!(assn.drv_path, x_path);
    complete_failure(
        &handle,
        "tsf-w",
        &x_path,
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "boom",
    )
    .await?;
    barrier(&handle).await;
    let frozen = query_status(&handle, b1).await?;
    assert_eq!(frozen.state, rio_proto::types::BuildState::Failed as i32);
    assert_eq!(frozen.total_derivations, 1);
    // Drain B1's pre-terminal event history (it ends with BuildFailed).
    let mut saw_failed = false;
    while let Ok(ev) = rx1.try_recv() {
        if matches!(ev.event, Some(Event::Failed(_))) {
            saw_failed = true;
        }
    }
    assert!(saw_failed, "B1 should have received its BuildFailed");

    // B2 resubmits the same derivation (resubmit-reset carries B1's
    // interest onto the fresh node) and it now succeeds. Fresh worker
    // for the re-dispatch (the harness pattern for chained dispatches).
    disconnect(&handle, "tsf-w").await?;
    let b2 = Uuid::new_v4();
    let mut x2 = make_node("tsf-x");
    x2.expected_output_paths = vec![x_out.clone()];
    let mut rx2 = merge_dag(&handle, b2, vec![x2], vec![], false).await?;
    barrier(&handle).await;
    let mut wrx2 = connect_executor(&handle, "tsf-w2", "x86_64-linux").await?;
    let assn = recv_assignment(&mut wrx2).await;
    assert_eq!(assn.drv_path, x_path);
    complete_success(&handle, "tsf-w2", &x_path, &x_out).await?;
    barrier(&handle).await;

    // Live build: Progress flowed and it succeeded.
    assert_eq!(
        query_status(&handle, b2).await?.state,
        rio_proto::types::BuildState::Succeeded as i32
    );
    let mut b2_saw_progress = false;
    while let Ok(ev) = rx2.try_recv() {
        if matches!(ev.event, Some(Event::Progress(_))) {
            b2_saw_progress = true;
        }
    }
    assert!(b2_saw_progress, "live build still gets Progress");

    // Terminal build: no Progress after BuildFailed (per-derivation
    // events may still flow; aggregate progress must not).
    while let Ok(ev) = rx1.try_recv() {
        assert!(
            !matches!(ev.event, Some(Event::Progress(_))),
            "terminal build received a post-BuildFailed BuildProgress: {:?}",
            ev.event
        );
    }
    // And its served status is still the frozen snapshot.
    let after = query_status(&handle, b1).await?;
    assert_eq!(after.state, rio_proto::types::BuildState::Failed as i32);
    assert_eq!(after.total_derivations, frozen.total_derivations);
    assert_eq!(after.completed_derivations, frozen.completed_derivations);
    assert_eq!(after.failed_derivations, frozen.failed_derivations);
    Ok(())
}

// r[verify sched.build.terminal-status-settled+2]
/// A late failure of a shared node must not rewrite a resident terminal
/// build's settled outcome: the terminal build keeps its first-failure
/// summary and frozen counts when the node it still holds interest in is
/// resubmitted by another build and fails again with a different error.
/// (The sticky first-failure get_or_insert already protects the summary;
/// the terminal early-return in handle_derivation_failure is the
/// structural guard that keeps every other side effect away from a
/// settled build.)
#[tokio::test]
async fn test_terminal_build_outcome_not_mutated_by_late_shared_failure() -> TestResult {
    let (_db, handle, _task, mut wrx) = setup_with_worker("tlf-w", "x86_64-linux").await?;
    let x_path = test_drv_path("tlf-x");
    let x_out = test_store_path("tlf-x-out");

    // B1 (keep_going): fails once → Failed with the first error frozen.
    let b1 = Uuid::new_v4();
    let mut x = make_node("tlf-x");
    x.expected_output_paths = vec![x_out.clone()];
    let _rx1 = merge_dag(&handle, b1, vec![x], vec![], true).await?;
    let assn = recv_assignment(&mut wrx).await;
    assert_eq!(assn.drv_path, x_path);
    complete_failure(
        &handle,
        "tlf-w",
        &x_path,
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "first error",
    )
    .await?;
    barrier(&handle).await;
    let frozen = query_status(&handle, b1).await?;
    assert_eq!(frozen.state, rio_proto::types::BuildState::Failed as i32);
    assert!(!frozen.error_summary.is_empty(), "first failure recorded");

    // B2 resubmits the node; it fails again with a different error.
    // Fresh worker for the re-dispatch (harness chained-dispatch pattern).
    disconnect(&handle, "tlf-w").await?;
    let b2 = Uuid::new_v4();
    let mut x2 = make_node("tlf-x");
    x2.expected_output_paths = vec![x_out.clone()];
    let _rx2 = merge_dag(&handle, b2, vec![x2], vec![], false).await?;
    barrier(&handle).await;
    let mut wrx2 = connect_executor(&handle, "tlf-w2", "x86_64-linux").await?;
    let assn = recv_assignment(&mut wrx2).await;
    assert_eq!(assn.drv_path, x_path);
    complete_failure(
        &handle,
        "tlf-w2",
        &x_path,
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "second error",
    )
    .await?;
    barrier(&handle).await;

    // The other build fails normally; the terminal build's settled
    // outcome (state, summary, counts) is byte-for-byte unchanged.
    assert_eq!(
        query_status(&handle, b2).await?.state,
        rio_proto::types::BuildState::Failed as i32,
        "the resubmitting build fails on its own"
    );
    let after = query_status(&handle, b1).await?;
    assert_eq!(after.state, frozen.state);
    assert_eq!(after.error_summary, frozen.error_summary);
    assert_eq!(after.total_derivations, frozen.total_derivations);
    assert_eq!(after.completed_derivations, frozen.completed_derivations);
    assert_eq!(after.failed_derivations, frozen.failed_derivations);
    Ok(())
}

/// Consumer-side guard for gw.dag.modulo-hash-all-nodes: a PLAIN IA node
/// (is_ca=false, needs_resolve=false, statically-known path) that carries
/// a populated ca_modular_hash must NOT be treated as CA anywhere — its
/// completion registers no realisation row, because the realisation gate
/// keys on is_ca/needs_resolve, never on hash presence.
// r[verify gw.dag.modulo-hash-all-nodes]
#[tokio::test]
async fn plain_ia_modular_hash_does_not_trigger_ca_paths() -> TestResult {
    let (db, handle, _task, mut wrx) = setup_with_worker("ia-guard-w", "x86_64-linux").await?;
    let build_id = Uuid::new_v4();
    let modular_hash = [0x42u8; 32];
    let static_out = test_store_path("ia-guard-out");
    let mut node = make_node("ia-guard");
    node.is_content_addressed = false;
    node.needs_resolve = false;
    node.is_fixed_output = false;
    node.expected_output_paths = vec![static_out.clone()];
    // The gateway now populates this for plain IA nodes too.
    node.ca_modular_hash = modular_hash.to_vec();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let assn = recv_assignment(&mut wrx).await;
    assert_eq!(assn.drv_path, test_drv_path("ia-guard"));
    complete_success(
        &handle,
        "ia-guard-w",
        &test_drv_path("ia-guard"),
        &static_out,
    )
    .await?;
    barrier(&handle).await;

    // Build succeeded normally…
    assert_eq!(
        query_status(&handle, build_id).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
    );
    // …and NO realisation row was registered for the hash.
    let (n_rows,): (i64,) = sqlx::query_as("SELECT COUNT(*) FROM realisations WHERE drv_hash = $1")
        .bind(modular_hash.as_slice())
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        n_rows, 0,
        "plain IA completion must not register a realisation (gate is is_ca/needs_resolve, not hash presence)"
    );
    Ok(())
}

/// Live-path pin for the deferred-IA realisation contract: a
/// non-recovered deferred-IA completion (is_ca=false, needs_resolve set
/// on the submitted node, gateway-provided modular hash) writes the
/// realisation row that `wopQueryDerivationOutputMap` serves. The
/// recovery-path counterpart
/// (`test_recovery_registers_realisation_for_deferred_ia_node`) asserts
/// the same outcome post-failover; this baseline keeps a regression
/// there from hiding behind a regression here.
#[tokio::test]
async fn test_live_deferred_ia_completion_registers_realisation() -> TestResult {
    let (db, handle, _task, mut wrx) = setup_with_worker("dia-w", "x86_64-linux").await?;
    let build_id = Uuid::new_v4();
    let modular_hash = [0x6bu8; 32];
    let mut node = make_node("dia-live");
    node.is_content_addressed = false;
    node.needs_resolve = true;
    node.expected_output_paths = vec![String::new()];
    node.ca_modular_hash = modular_hash.to_vec();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let assn = recv_assignment(&mut wrx).await;
    assert_eq!(assn.drv_path, test_drv_path("dia-live"));
    complete_ca(
        &handle,
        "dia-w",
        &test_drv_path("dia-live"),
        &[(
            "out",
            "/nix/store/dddddddddddddddddddddddddddddddd-dia-live-out",
            vec![0x11u8; 32],
        )],
    )
    .await?;
    barrier(&handle).await;

    let (path,): (String,) = sqlx::query_as(
        "SELECT output_path FROM realisations WHERE drv_hash = $1 AND output_name = 'out'",
    )
    .bind(modular_hash.as_slice())
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        path, "/nix/store/dddddddddddddddddddddddddddddddd-dia-live-out",
        "live deferred-IA completion registers the realisation row"
    );
    Ok(())
}
