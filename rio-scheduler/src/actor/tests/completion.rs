//! Completion handling: retry/poison thresholds, dep-chain release, duplicate idempotence.
/// merged_bug_013 (round 3): the corroboration set is keyed by NODE —
/// non-optional — so an unattributed sighting (a flagged report whose
/// pull attempt has no controller-authoritative binding yet) cannot
/// mint a distinct-node count. Pre-fix the map was keyed
/// `Option<String>`: one node's pre-binding `None` sighting plus its
/// later attributed `Some` sighting counted as 2 "distinct nodes" and
/// self-corroborated into the uncharged Paced lane.
// r[verify sched.retry.store-degraded-uncharged+4]
#[test]
fn store_degraded_mixed_unattributed_then_attributed_is_one_node() {
    use crate::actor::completion::note_store_degraded_sighting;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    let w = Duration::from_secs(600);
    let mut sightings: HashMap<String, Instant> = HashMap::new();
    let t0 = Instant::now();
    // Pre-binding flagged report: unattributed — NOT inserted.
    assert_eq!(note_store_degraded_sighting(&mut sightings, None, t0, w), 0);
    // The same node, now controller-bound: ONE distinct node.
    assert_eq!(
        note_store_degraded_sighting(&mut sightings, Some("n1".into()), t0, w),
        1,
        "the mixed None+Some pair must collapse to one distinct node"
    );
    // Further unattributed reports still cannot self-corroborate.
    assert_eq!(note_store_degraded_sighting(&mut sightings, None, t0, w), 1);
    // Corroboration requires a SECOND controller-bound node.
    assert_eq!(
        note_store_degraded_sighting(&mut sightings, Some("n2".into()), t0, w),
        2
    );
}

/// merged_bug_013 companion: sightings age out of the corroboration
/// window — the count is evidence-fresh, not cumulative.
#[test]
fn store_degraded_sightings_expire_outside_window() {
    use crate::actor::completion::note_store_degraded_sighting;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};
    let w = Duration::from_secs(600);
    let mut sightings: HashMap<String, Instant> = HashMap::new();
    let t0 = Instant::now();
    assert_eq!(
        note_store_degraded_sighting(&mut sightings, Some("n1".into()), t0, w),
        1
    );
    // 601s later, n1's sighting is stale: a fresh n2 stands alone.
    let t1 = t0 + Duration::from_secs(601);
    assert_eq!(
        note_store_degraded_sighting(&mut sightings, Some("n2".into()), t1, w),
        1,
        "stale sightings must not corroborate"
    );
}
// r[verify sched.completion.idempotent]
// r[verify sched.state.transitions]
// r[verify sched.state.terminal-idempotent+2]

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

    let f = setup_pull_ca_fixture("ca-match").await?;

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
    pull_complete_ca(
        &f.actor,
        "ca-match",
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
    let mixed_modular: [u8; 32] = {
        use sha2::{Digest, Sha256};
        Sha256::digest(b"ca-fixture:ca-mixed").into()
    };
    let build_id = Uuid::new_v4();
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
    pull_complete_ca(
        &f.actor,
        "ca-mixed",
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
    let build_id = Uuid::new_v4();
    let node = make_node("ia-skip"); // is_content_addressed=false
    let _ev = merge_dag(&f.actor, build_id, vec![node], vec![], false).await?;

    // Seed a prior for the IA path — if the is_ca guard were missing,
    // this would match.
    let ia_out = test_store_path("ia-skip-out");
    seed_realisation(&f.pool, &[0x99; 32], "out", &ia_out, &[0xef; 32]).await?;

    let match_before3 = recorder.get(match_key);
    let miss_before3 = recorder.get(miss_key);
    pull_complete_ca(&f.actor, "ia-skip", &[("out", &ia_out, vec![0xef; 32])]).await?;

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

    let f = setup_pull_ca_fixture(key).await?;

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
    pull_complete_ca(&f.actor, key, &outputs_ref).await?;

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
    let f =
        setup_pull_ca_fixture_configured("ca-slow", |c, _| c.grpc_timeout = Duration::from_secs(3))
            .await?;

    pull_complete_ca(
        &f.actor,
        "ca-slow",
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
    pull_complete_ca(&handle, "verify-a", &[("out", &a_out, vec![0xAA; 32])]).await?;
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

    pull_complete_ca(&handle, "xmatch-a", &[("out", &a_out, vec![0xAA; 32])]).await?;
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

    pull_complete_ca(&handle, "stdenv-a", &[("out", &a_out, vec![0xAA; 32])]).await?;
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

    let (_db, handle, _task) = setup().await;
    let drv_hash = "memb-drv";
    // make_node → output_names = ["out"] (1 declared output). The
    // expected set carries the lawful path (production-faithful:
    // the gateway always populates it for IA — bug_138's membership
    // law fail-closes on a node without one).
    let valid = test_store_path("memb-out");
    let mut node = make_node(drv_hash);
    node.expected_output_paths = vec![valid.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    // 1 valid "out" + 100 fabricated names + 1 duplicate "out". All
    // paths are well-formed (the format-filter passes); only the
    // membership/dedup filter catches them.
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

    pull_report(
        &handle,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::Built.into(),
            built_outputs: outs,
            ..Default::default()
        }),
    )
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

    pull_complete_ca(&handle, "ambig-a", &[("out", &a_out, vec![0xAA; 32])]).await?;
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

    pull_complete_ca(&handle, "batch-a", &[("out", &outs[0], vec![0x77; 32])]).await?;
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

    let build_id = Uuid::new_v4();
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
    pull_complete_ca(
        &handle,
        "ca-shortcircuit",
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

/// max_retries (default 2) exhausted → poison (the `retry_count >=
/// max_retries` branch, distinct from POISON_THRESHOLD 3-distinct-
/// workers).
///
/// Pull-mode delivery: each attempt is a fresh pull (mint) followed by
/// a failure report through the report intake — no binding, so every
/// attempt charges the same intent identity and only the max_retries
/// branch (not the distinct-source threshold) is in play.
#[tokio::test]
async fn test_transient_failure_max_retries_poisons() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _event_rx =
        merge_single_node(&handle, build_id, "maxretry-hash", PriorityClass::Scheduled).await?;

    // Default RetryPolicy::max_retries = 2. Fail 3 times:
    // retry_count 0 -> 1 (retry), 1 -> 2 (retry), 2 >= 2 -> Poisoned.
    for attempt in 0..3 {
        pull_complete_failure(
            &handle,
            "maxretry-hash",
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
/// TransientFailure to drive the ladder; under the worker-reported
/// promotion design that's wrong. CgroupOom is the worker-reported
/// sizing signal (pod-level OOMKilled arrives as the controller's
/// `ReportAttemptOutcome` classification fill, which never
/// promotes).
// r[verify sched.retry.promotion-exempt+3]
// r[verify sched.sla.reactive-floor+4]
#[tokio::test]
async fn test_transient_failure_promotion_exempt_from_max_retries() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    let _ = names; // class names retained for the ladder narrative below

    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ladder-drv",
        PriorityClass::Scheduled,
    )
    .await?;

    // Seed est_memory_bytes (the doubling base).
    handle
        .debug_seed_sched_hint("ladder-drv", Some(2 << 30), None, None, None)
        .await?;

    // Walk via worker-reported CgroupOom (InfrastructureFailure with
    // the CgroupOom error string), each attempt a fresh pull + report:
    // each doubles mem floor; retry_count (transient budget) and
    // infra_count stay 0 (promoted=true → exempt_from_cap). The
    // doubling base is re-seeded to the promoted floor before each
    // rung (the dispatch-time D2 refresh that did this on the stream
    // path is placement-side; the report intake does not re-solve).
    let mut prev_mem = 0u64;
    for c in &names[..4] {
        if prev_mem > 0 {
            handle
                .debug_seed_sched_hint("ladder-drv", Some(prev_mem), None, None, None)
                .await?;
        }
        // bug_090: each rung is a typed claim corroborated at the
        // CURRENT minted intent (the pull mints, then the report
        // claims a peak at that shape — the honest ladder).
        let exec_id = open_pull_exec(&handle, "ladder-drv").await;
        let rung_mem = expect_drv(&handle, "ladder-drv")
            .await
            .sched
            .last_intent
            .as_ref()
            .map(|i| i.mem_bytes)
            .unwrap_or(0);
        pull_report_exec(
            &handle,
            exec_id,
            "ladder-drv",
            typed_sizing_failure(
                rio_proto::types::FailureClass::CgroupOom,
                "cgroup OOM during build; bumping resource floor",
                None,
                rung_mem,
            ),
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
    pull_complete_failure(
        &handle,
        "ladder-drv",
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
        pull_complete_failure(
            &handle,
            "ladder-drv",
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
///   the eligible-fleet snapshot excludes them → empty fleet →
///   `placeable()` defers → re-queue. The controller spawns fresh `executor_id`s
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.max_retries = 10;
    });
    let _db = db;

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "pt-drv", PriorityClass::Scheduled).await?;

    // Pull-mode delivery: each entry is one bound-node attempt (bind →
    // pull → failure report), so each failure charges a distinct
    // source-node exclusion key — the pull equivalent of N distinct
    // stream workers.
    pull_fail_on_nodes(
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

// r[verify sched.retry.per-executor-budget+4]
/// InfrastructureFailure is a worker-local problem (FUSE EIO, cgroup
/// setup fail, OOM-kill of the build process) — NOT the build's fault.
/// 3× InfrastructureFailure on distinct workers → failed_builders stays
/// EMPTY, derivation NOT poisoned. Contrast with the TransientFailure
/// test above, where 3 distinct failures → poison.
#[tokio::test]
async fn test_infrastructure_failure_does_not_count_toward_poison() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-drv";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× InfrastructureFailure from distinct sources (bound node per
    // attempt). TransientFailure would poison; here it must not
    // (reset_to_ready WITHOUT failed_builders insert / backoff).
    pull_fail_on_nodes(
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
    pull_fail_on_nodes(
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        // 2 retries → walks tiny→small, small→medium, then terminal on 3rd TimedOut.
        c.retry_policy = crate::RetryPolicy {
            max_timeout_retries: 2,
            ..Default::default()
        };
    });

    // D4: bump_floor_or_count reads the MINTED intent's deadline as
    // the doubling base (live_040: each pull mint stamps the solve
    // onto last_intent — the freshness law overwrites any seed, so
    // the ladder is asserted relative to the OBSERVED minted base).
    // Each rung is one pull attempt + a TimedOut report through the
    // report intake.
    let build_id = Uuid::new_v4();
    let drv_hash = "i200-timeout";
    let _ev = merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // ── Retry 1: TimedOut → floor.deadline = 2×minted, status=Ready ──
    let exec = open_pull_exec(&handle, drv_hash).await;
    let d0 = expect_drv(&handle, drv_hash)
        .await
        .sched
        .last_intent
        .as_ref()
        .expect("the mint stamps the solve (live_040)")
        .deadline_secs;
    assert!(
        d0 > 0 && d0 * 4 < 86_400,
        "ladder headroom under the 24h cap"
    );
    // bug_102: the genuine-timeout anchor — the attempt demonstrably
    // ran its assigned deadline (running_since backdated), so the
    // corroboration witness mints and the floor heals as designed.
    assert!(
        handle
            .debug_backdate_running(drv_hash, u64::from(d0))
            .await?
    );
    pull_report_exec(
        &handle,
        exec,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::TimedOut.into(),
            error_msg: "build exceeded daemon_timeout_secs".into(),
            ..Default::default()
        }),
    )
    .await?;

    let info = expect_drv(&handle, drv_hash).await;
    let floor1 = info.sched.resource_floor.deadline_secs;
    assert_eq!(
        floor1,
        d0 * 2,
        "I-200: TimedOut → deadline floor doubled from the minted base ({d0}→{})",
        d0 * 2
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

    // ── Retry 2: TimedOut → floor doubles from max(floor, minted) ──
    let exec = open_pull_exec(&handle, drv_hash).await;
    let d1 = expect_drv(&handle, drv_hash)
        .await
        .sched
        .last_intent
        .as_ref()
        .expect("re-mint re-stamps")
        .deadline_secs;
    assert!(
        handle
            .debug_backdate_running(drv_hash, u64::from(d1))
            .await?
    );
    pull_report_exec(
        &handle,
        exec,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::TimedOut.into(),
            error_msg: "build exceeded daemon_timeout_secs".into(),
            ..Default::default()
        }),
    )
    .await?;
    let info = expect_drv(&handle, drv_hash).await;
    let floor2 = info.sched.resource_floor.deadline_secs;
    assert_eq!(
        floor2,
        floor1.max(d1) * 2,
        "second rung doubles from max(floor, minted base)"
    );
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
    let exec = open_pull_exec(&handle, drv_hash).await;
    let d2 = expect_drv(&handle, drv_hash)
        .await
        .sched
        .last_intent
        .as_ref()
        .expect("re-mint re-stamps")
        .deadline_secs;
    assert!(
        handle
            .debug_backdate_running(drv_hash, u64::from(d2))
            .await?
    );
    pull_report_exec(
        &handle,
        exec,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::TimedOut.into(),
            error_msg: "build exceeded daemon_timeout_secs".into(),
            ..Default::default()
        }),
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
        info.sched.resource_floor.deadline_secs,
        floor2.max(d2) * 2,
        "bump ran on terminal path too (so explicit resubmit starts higher)"
    );
    Ok(())
}

// r[verify sched.retry.store-degraded-uncharged+4]
/// bug_408: an infra failure carrying the builder's `store_degraded`
/// flag is UNCHARGED — driven past `max_infra_retries` (10) it never
/// poisons, never advances `infra_count`, never excludes the node;
/// only the derivation backoff advances (the requeue is paced, not
/// counted: wait out the outage). RED (recorded, pre-fix): the intake
/// ignored the flag and folded the report as `WorkerInfra` —
/// `infra_count` reached 10 by the boundary and the 11th report
/// poisoned, exactly the fleet-amplification this class exists to
/// prevent (`left: Poisoned` / `right: not-poisoned`,
/// `left: 10 / right: 0`).
#[tokio::test]
async fn test_store_degraded_infra_uncharged_waits_out_the_outage() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let drv_hash = "store-degraded-drv";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // merged_bug_032 re-pin: corroborate via the store-health leg so
    // the gate admits the class (11 < STORE_DEGRADED_FREE_RUN = 12 —
    // every original assertion below is intact).
    handle.debug_mark_store_rpc_failure().await?;

    // 11 store-degraded infra failures: one past the infra cap.
    for attempt in 0..11 {
        pull_complete_failure_result(
            &handle,
            drv_hash,
            rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
                error_msg: format!("FUSE EIO: store unreachable (attempt {attempt})"),
                store_degraded: true,
                ..Default::default()
            },
        )
        .await?;
    }

    let s = expect_drv(&handle, drv_hash).await;
    assert_ne!(
        s.status,
        DerivationStatus::Poisoned,
        "store-degraded failures never reach poison (the class is uncharged)"
    );
    assert_eq!(s.retry.infra_count, 0, "no infra budget draw");
    assert_eq!(s.retry.count, 0, "no transient budget draw");
    assert_eq!(s.retry.exempt_infra_count, 0, "not exempt-infra either");
    assert_eq!(s.retry.failure_count, 0, "no flat failure count");
    assert!(
        s.retry.failed_builders.is_empty(),
        "the store's failure mints no node exclusion"
    );
    assert!(
        s.retry.backoff_until.is_some(),
        "the requeue is paced by the derivation backoff"
    );
    Ok(())
}

/// W10-CM (live_057-b, the consumer arm — the cross-crate cell):
/// a worker report whose error_msg carries rio_proto::DISK_FULL_MSG
/// (the builder's quota-attributed DiskFull Display, the contract
/// const both sides reference) doubles the DISK resource floor —
/// the parked EvictedDiskPressure arm's first live producer. The
/// ladder algebra itself is NOT touched (the floor.rs unit pins
/// stand); this drives the production intake path end-to-end:
/// pull → report → floor.
#[tokio::test]
async fn test_disk_full_report_doubles_disk_floor() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let drv = "disk-floor-drv";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;
    // Seed est_disk_bytes — the doubling base (the 25 GiB rung).
    handle
        .debug_seed_sched_hint(drv, None, Some(25 << 30), None, None)
        .await?;

    // bug_090: the typed claim, corroborated at the minted shape.
    let exec_id = open_pull_exec(&handle, drv).await;
    let minted_disk = expect_drv(&handle, drv)
        .await
        .sched
        .last_intent
        .as_ref()
        .map(|i| i.disk_bytes)
        .unwrap_or(0);
    assert!(minted_disk > 0, "the pull mint stamps the disk intent");
    pull_report_exec(
        &handle,
        exec_id,
        drv,
        typed_sizing_failure(
            rio_proto::types::FailureClass::DiskFull,
            "disk full during build (overlay prjquota exhausted); bumping disk floor",
            Some(rio_proto::types::QuotaTelemetry {
                peak_used_bytes: minted_disk - (1 << 20),
                hard_limit_bytes: minted_disk,
                node_free_bytes: 50 << 30,
            }),
            0,
        ),
    )
    .await?;

    let s = expect_drv(&handle, drv).await;
    assert!(
        s.sched.resource_floor.disk_bytes > 0,
        "left: the DiskFull report falls into the non-bump infra arm \
         (retry-poison; the disk recovery ladder never engages, floor \
         stays 0) / right: the floor doubles from the last-dispatched \
         intent's disk (the bump_dim law the :302-region unit pins \
         own); got {}",
        s.sched.resource_floor.disk_bytes
    );
    assert_eq!(
        s.retry.infra_count, 0,
        "promoted=true → exempt from the infra cap (the D4 exemption, \
         the cgroup_oom parity)"
    );
    assert_ne!(s.status, DerivationStatus::Poisoned);
    Ok(())
}

/// W10-CO (the suppression parity product): {oom, disk} ×
/// {believed-store, plain}. bug_408's believed-store gate applies
/// IDENTICALLY to both sizing letters — a store-degraded failure is
/// never a sizing signal, on either axis; an uncorroborated/plain
/// report bumps its own dimension and only its own.
#[rstest]
#[case::oom_plain(true, false)]
#[case::oom_believed(true, true)]
#[case::disk_plain(false, false)]
#[case::disk_believed(false, true)]
#[tokio::test]
async fn test_floor_bump_store_suppression_parity(
    #[case] oom: bool,
    #[case] believed_store: bool,
) -> TestResult {
    let (_db, handle, _task) = setup().await;
    let drv = "parity-drv";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;
    handle
        .debug_seed_sched_hint(drv, Some(2 << 30), Some(25 << 30), None, None)
        .await?;
    if believed_store {
        // merged_bug_032: corroborate via the store-health leg so the
        // degraded flag is BELIEVED (Paced/RunBound), not bare.
        handle.debug_mark_store_rpc_failure().await?;
    }
    // bug_090: even a TYPED, CORROBORATED claim is suppressed by a
    // believed-store attribution — corroboration gates forgery; the
    // bug_408 law gates attribution. Claims corroborate at the
    // minted shape.
    let exec_id = open_pull_exec(&handle, drv).await;
    let intent = expect_drv(&handle, drv)
        .await
        .sched
        .last_intent
        .as_ref()
        .map(|i| (i.mem_bytes, i.disk_bytes))
        .unwrap_or((0, 0));
    let mut payload = if oom {
        typed_sizing_failure(
            rio_proto::types::FailureClass::CgroupOom,
            "cgroup OOM during build; bumping resource floor",
            None,
            intent.0,
        )
    } else {
        typed_sizing_failure(
            rio_proto::types::FailureClass::DiskFull,
            "disk full during build; bumping disk floor",
            Some(rio_proto::types::QuotaTelemetry {
                peak_used_bytes: intent.1.saturating_sub(1 << 20),
                hard_limit_bytes: intent.1,
                node_free_bytes: 50 << 30,
            }),
            0,
        )
    };
    payload.result.store_degraded = believed_store;
    pull_report_exec(&handle, exec_id, drv, payload).await?;

    let s = expect_drv(&handle, drv).await;
    let (mem, disk) = (
        s.sched.resource_floor.mem_bytes,
        s.sched.resource_floor.disk_bytes,
    );
    if believed_store {
        assert_eq!(
            (mem, disk),
            (0, 0),
            "believed-store suppresses the sizing signal on BOTH axes \
             (a store-degraded failure is never a sizing signal)"
        );
    } else if oom {
        assert!(mem > 0, "plain OOM bumps the mem dimension; got {mem}");
        assert_eq!(disk, 0, "…and ONLY the mem dimension");
    } else {
        assert!(
            disk > 0,
            "plain DiskFull bumps the disk dimension; got {disk}"
        );
        assert_eq!(mem, 0, "…and ONLY the disk dimension");
    }
    Ok(())
}

/// W11-W (bug_090) — proposition: NO persisted resource floor moves
/// on unattested worker text. Population: {untyped,
/// typed-uncorroborated, typed-corroborated} × {oom, disk}, plus the
/// paced-forgery cell (N corroboration-free reports, arbitrarily
/// spaced, produce ZERO ladder steps — the cap is
/// population-denominated, never wall-windowed, R29; pacing is
/// irrelevant because the gate is corroboration, not time).
///
/// The attack (the red this test was born failing on, pre-fix):
/// `handle_infrastructure_failure` substring-matched the
/// worker-supplied free-text `error_msg` against
/// CGROUP_OOM_MSG/DISK_FULL_MSG to drive `bump_resource_floor` — a
/// drv_hash-keyed (cross-tenant, M_095), GREATEST()-ratcheted,
/// never-healed-downward persisted sizing decision riding the
/// uncharged ExemptInfra lane. Three forged free-text reports
/// completed the 25→50→100→200 GiB disk ladder; the only gate was
/// `believed_store`, which a hostile builder simply doesn't set.
///
/// Post-fix: the floor bump consumes only the typed
/// `failure_classification` field, corroborated against the
/// scheduler-assigned shape; free text is display/narration.
// r[verify sched.trust.report-corroboration+3]
#[tokio::test]
async fn forged_free_text_never_moves_resource_floors() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let drv = "forge-floor-drv";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;
    // The 25 GiB rung — the ladder base the wave-10 incident rode.
    handle
        .debug_seed_sched_hint(drv, Some(2 << 30), Some(25 << 30), None, None)
        .await?;

    // Three forged FREE-TEXT reports (arbitrary pacing — the
    // paced-forgery cell collapses to this population: zero
    // corroborated incidents, zero steps).
    for _ in 0..3 {
        pull_complete_failure(
            &handle,
            drv,
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            &format!(
                "{} (overlay prjquota exhausted); bumping disk floor",
                rio_proto::DISK_FULL_MSG
            ),
        )
        .await?;
    }
    let s = expect_drv(&handle, drv).await;
    assert_eq!(
        s.sched.resource_floor.disk_bytes, 0,
        "left (pre-fix): three forged free-text reports complete the \
         25→50→100→200 GiB ladder — a persisted cross-tenant sizing \
         decision moved on unattested display text / right: the \
         substring channel is classify-only; the floor never moves \
         without a typed corroborated classification"
    );
    assert_eq!(
        s.sched.resource_floor.mem_bytes, 0,
        "the forged text moves NO axis"
    );
    Ok(())
}

/// W11-W, the typed cells: an UNCORROBORATED typed classification is
/// refused (telemetry absent, or inconsistent with the
/// scheduler-assigned shape); a CORROBORATED one takes exactly ONE
/// doubling per incident. Both axes.
// r[verify sched.trust.report-corroboration+3]
#[tokio::test]
async fn typed_classification_bumps_only_with_corroboration() -> TestResult {
    // Big ceilings: the corroborated cells assert a full doubling —
    // the default test ceilings sit at the probe shape, where the
    // at-cap law (a different cell) would absorb the bump.
    let (_db, handle, _task) = setup_with_big_ceilings().await;

    // ── disk axis ───────────────────────────────────────────────────
    let drv = "typed-disk-drv";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;
    handle
        .debug_seed_sched_hint(drv, Some(2 << 30), Some(25 << 30), None, None)
        .await?;

    // Typed but UNCORROBORATED: no telemetry at all.
    pull_complete_failure_result(
        &handle,
        drv,
        rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
            error_msg: "disk full during build (forged, no telemetry)".into(),
            failure_classification: Some(rio_proto::types::FailureClassification {
                class: rio_proto::types::FailureClass::DiskFull.into(),
                quota: None,
            }),
            ..Default::default()
        },
    )
    .await?;
    let s = expect_drv(&handle, drv).await;
    assert_eq!(
        s.sched.resource_floor.disk_bytes, 0,
        "typed-uncorroborated is REFUSED: no telemetry, no bump"
    );

    // Typed but INCONSISTENT: claims exhaustion of a limit far from
    // the assigned shape (a forged tiny-quota exhaustion must not
    // ladder the floor). 64 MiB is far below any minted disk shape.
    pull_complete_failure_result(
        &handle,
        drv,
        rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
            error_msg: "disk full during build (forged, tiny limit)".into(),
            failure_classification: Some(rio_proto::types::FailureClassification {
                class: rio_proto::types::FailureClass::DiskFull.into(),
                quota: Some(rio_proto::types::QuotaTelemetry {
                    peak_used_bytes: 64 << 20,
                    hard_limit_bytes: 64 << 20,
                    node_free_bytes: 50 << 30,
                }),
            }),
            ..Default::default()
        },
    )
    .await?;
    let s = expect_drv(&handle, drv).await;
    assert_eq!(
        s.sched.resource_floor.disk_bytes, 0,
        "typed-but-inconsistent is REFUSED: the claimed limit does not \
         match the shape the scheduler assigned"
    );

    // Typed + CORROBORATED: exhaustion AT the assigned size (read
    // from the mint — the corroboration anchor the scheduler owns).
    let exec_id = open_pull_exec(&handle, drv).await;
    let minted_disk = expect_drv(&handle, drv)
        .await
        .sched
        .last_intent
        .as_ref()
        .map(|i| i.disk_bytes)
        .unwrap_or(0);
    assert!(minted_disk > 0, "the pull mint stamps the disk intent");
    pull_report_exec(
        &handle,
        exec_id,
        drv,
        typed_sizing_failure(
            rio_proto::types::FailureClass::DiskFull,
            "disk full during build (honest)",
            Some(rio_proto::types::QuotaTelemetry {
                peak_used_bytes: minted_disk - (1 << 20),
                hard_limit_bytes: minted_disk,
                node_free_bytes: 50 << 30,
            }),
            0,
        ),
    )
    .await?;
    let s = expect_drv(&handle, drv).await;
    assert_eq!(
        s.sched.resource_floor.disk_bytes,
        minted_disk * 2,
        "typed + corroborated takes exactly ONE doubling from the \
         minted shape"
    );
    assert_eq!(s.sched.resource_floor.mem_bytes, 0, "only its own axis");

    // ── mem axis (oom) ──────────────────────────────────────────────
    // No seeds: the default probe shape stays under the test
    // ceilings (an at-cap bump is the at-cap law's cell, not ours).
    let drv = "typed-oom-drv";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;

    // Typed oom, UNCORROBORATED: the report's peak_memory is nowhere
    // near the assigned memory shape (the corroboration anchor the
    // scheduler itself minted at dispatch).
    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
        error_msg: "cgroup OOM during build (forged, tiny peak)".into(),
        failure_classification: Some(rio_proto::types::FailureClassification {
            class: rio_proto::types::FailureClass::CgroupOom.into(),
            quota: None,
        }),
        ..Default::default()
    });
    payload.peak_memory_bytes = 1 << 20;
    pull_report(&handle, drv, payload).await?;
    let s = expect_drv(&handle, drv).await;
    assert_eq!(
        s.sched.resource_floor.mem_bytes, 0,
        "typed oom with a peak far below the assigned shape is REFUSED"
    );

    // Typed oom, CORROBORATED: peak at the assigned limit (memory.peak
    // saturates at memory.max under an oom kill). Fresh drv — the
    // refused cell's forged tiny peak feeds the estimator, so cell
    // isolation keeps the corroboration anchor clean.
    let drv = "typed-oom-c-drv";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;
    let exec_id = open_pull_exec(&handle, drv).await;
    let minted_mem = expect_drv(&handle, drv)
        .await
        .sched
        .last_intent
        .as_ref()
        .map(|i| i.mem_bytes)
        .unwrap_or(0);
    assert!(minted_mem > 0, "the pull mint stamps the mem intent");
    pull_report_exec(
        &handle,
        exec_id,
        drv,
        typed_sizing_failure(
            rio_proto::types::FailureClass::CgroupOom,
            "cgroup OOM during build (honest)",
            None,
            minted_mem,
        ),
    )
    .await?;
    let s = expect_drv(&handle, drv).await;
    assert_eq!(
        s.sched.resource_floor.mem_bytes,
        minted_mem * 2,
        "typed + corroborated oom takes one doubling from the minted \
         shape"
    );
    assert_eq!(s.sched.resource_floor.disk_bytes, 0, "only its own axis");
    Ok(())
}

/// InfrastructureFailure hits `max_infra_retries` → poison. The cap
/// exists to convert a misclassified permanent failure (e.g. S3 auth
/// error reported as infra) into a visible poison instead of a hot
/// loop. Observed on EKS: 12 drvs × 146 dispatch cycles in 6 minutes
/// before manual intervention — each cycle re-ran the full build.
///
/// InfrastructureFailure has no backoff and doesn't touch
/// `failed_builders`; each attempt here is a fresh pull on the intent
/// identity followed by an infra-failure report.
#[tokio::test]
async fn test_infrastructure_failure_max_infra_retries_poisons() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-cap-drv";
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
        pull_complete_failure(
            &handle,
            drv_hash,
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
    pull_complete_failure(
        &handle,
        drv_hash,
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
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let drv_hash = "putpath-drv";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Drive WELL past the cap (default 10) — 15 concurrent-PutPath
    // failures. None should count; the drv stays Ready throughout.
    for _attempt in 0..15 {
        pull_complete_failure(
            &handle,
            drv_hash,
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
    pull_complete_failure(
        &handle,
        drv_hash,
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
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let drv_hash = "batch-putpath-drv";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    let batch_msg = format!(
        "output upload failed: output 1: {}; retry",
        rio_proto::CONCURRENT_PUTPATH_MSG
    );
    for _attempt in 0..15 {
        pull_complete_failure(
            &handle,
            drv_hash,
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.max_exempt_infra_retries = 5;
    });
    let _db = db;

    let drv_hash = "leak-drv";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    let leaked_msg = format!("upload failed: {}", rio_proto::CONCURRENT_PUTPATH_MSG);
    for attempt in 0..4 {
        pull_complete_failure(
            &handle,
            drv_hash,
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
    pull_complete_failure(
        &handle,
        drv_hash,
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
/// Flat-counter mode (`require_distinct_workers=false`, the
/// controller-less dev shape): three identity-less failures (no
/// binding → no source attribution → flat counters only, decision
/// P12) poison at the threshold.
#[tokio::test]
async fn test_same_worker_poison_threshold_flat_mode() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.poison = PoisonConfig {
            threshold: 3,
            require_distinct_workers: false,
        };
        c.retry_policy.max_retries = 10;
    });
    let _db = db;

    let drv_hash = "flat-mode-drv";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    // No binding: identity-less attempts charge flat counters only.
    for i in 0..3 {
        pull_complete_failure(
            &handle,
            drv_hash,
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("flat-mode failure {i}"),
        )
        .await?;
    }

    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.retry.failed_builders.len(),
        0,
        "identity-less rows contribute no exclusion keys (P12)"
    );
    assert_eq!(info.retry.failure_count, 3, "flat count always increments");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "flat mode: 3× failures → poisoned at threshold"
    );
    Ok(())
}

/// Distinct mode (`require_distinct_workers=true`): a history of three
/// failures all attributed to ONE source node does NOT poison (one
/// distinct source < threshold 3). Driven through the recovery fold
/// over a seeded ledger: with the 249-rider mint backstop, a live
/// re-pull through a binding on an excluded node answers NotYetReady
/// — the same-source-repetition shape can no longer be produced by
/// live pulls (the controller's drift reap + anti-affinity replace
/// the pod), so the fold semantics are pinned at the recovery
/// boundary, exactly where production re-encounters such a history.
#[tokio::test]
async fn test_same_worker_no_poison_distinct_mode_recovered_history() -> TestResult {
    let drv_hash = "distinct-mode-drv";
    let f = RecoveryFixture::run_configured(
        None,
        |c| {
            c.poison = PoisonConfig {
                threshold: 3,
                require_distinct_workers: true,
            };
            c.retry_policy.max_retries = 10;
        },
        async move |handle, pool| {
            let _ev =
                merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled)
                    .await?;
            barrier(&handle).await;
            let derivation_id: Uuid =
                sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
                    .bind(drv_hash)
                    .fetch_one(&pool)
                    .await?;
            let mut tx = pool.begin().await?;
            for i in 0..3 {
                let mut row = crate::db::attempts::AttemptRow::new(
                    derivation_id,
                    crate::state::OutcomeClass::Transient,
                    crate::state::ReportingParty::Worker,
                    crate::state::AttemptKind::Build,
                );
                row.executor_id = Some(crate::state::ExecutorId::from(
                    format!("same-source-exec-{i}").as_str(),
                ));
                row.source_node = Some("same-source-node".to_string());
                crate::db::SchedulerDb::append_attempt(&mut tx, &row).await?;
            }
            tx.commit().await?;
            Ok(())
        },
    )
    .await?;
    let handle = f.handle;

    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.retry.failed_builders.len(),
        1,
        "HashSet: same source inserted once, stays len()=1"
    );
    assert_eq!(info.retry.failure_count, 3, "flat count always increments");
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "distinct mode: 3× same-source → NOT poisoned (1 distinct < 3)"
    );
    Ok(())
}

/// Completing a child releases its parent to Ready in a dependency chain.
#[tokio::test]
async fn test_dependency_chain_releases_parent() -> TestResult {
    let (_db, handle, _task) = setup().await;

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

    // B is the deliverable leaf. A is Queued waiting for B.
    let info_a = expect_drv(&handle, "chainA").await;
    assert_eq!(info_a.status, DerivationStatus::Queued);

    // The pull for B delivers it.
    let assigned_path = pull_attempt(&handle, "chainB").await.drv_path;
    assert_eq!(assigned_path, p_chain_b);

    // Complete B.
    pull_complete_success_empty(&handle, "chainB").await?;

    // A should now transition Queued -> Ready (released by B's completion).
    let info_a = expect_drv(&handle, "chainA").await;
    assert!(
        matches!(
            info_a.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "A should be Ready or Assigned after B completes, got {:?}",
        info_a.status
    );

    // A is now deliverable through the pull path.
    let assigned_path = pull_attempt(&handle, "chainA").await.drv_path;
    assert_eq!(
        assigned_path, p_chain_a,
        "A should be dispatched after B completes"
    );
    Ok(())
}

/// A duplicate report for the same open attempt is an idempotent no-op.
#[tokio::test]
async fn test_duplicate_completion_idempotent() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let drv_hash = "idem-hash";
    let mut event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Open one pull attempt, then send the same Built report TWICE.
    let exec_id = open_pull_exec(&handle, drv_hash).await;
    for _ in 0..2 {
        pull_report_exec(
            &handle,
            exec_id,
            drv_hash,
            pull_payload(rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            }),
        )
        .await?;
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

/// Unknown BuildResultStatus value (e.g. from a newer worker) → warn,
/// treat as transient failure. Don't panic, don't get stuck.
#[tokio::test]
#[traced_test]
async fn test_unknown_build_status_treated_as_transient() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    merge_single_node(&handle, build_id, "unk-status", PriorityClass::Scheduled).await?;

    // Open a pull attempt and report an invalid status int (9999)
    // through the report intake.
    pull_report(
        &handle,
        "unk-status",
        pull_payload(rio_proto::types::BuildResult {
            status: 9999, // not a valid enum
            error_msg: "mystery".into(),
            ..Default::default()
        }),
    )
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
    let (db, handle, _task) = setup().await;

    // Merge with a distinct pname — make_test_node defaults to
    // "test-pkg", which is fine, but a unique pname makes the
    // SELECT below unambiguous if other tests ever share the pool.
    let build_id = Uuid::new_v4();
    let mut node = make_node("bs-drv");
    node.pname = "sample-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

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
    pull_report(
        &handle,
        "bs-drv",
        PullReportPayload {
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
            final_line_count: 0,
            // Mechanical flag-off default (carve-out 1c).
            materialization_outcome: None,
        },
    )
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
/// (spurious 0.0s samples would poison the SLA estimator's percentiles).
#[tokio::test]
async fn test_completion_no_timestamps_no_sample() -> TestResult {
    let (db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "nt-drv", PriorityClass::Scheduled).await?;

    // pull_complete_success_empty: BuildResult::default() → start_time=None,
    // stop_time=None. The EMA block and build_samples write both gate
    // on these being Some.
    pull_complete_success_empty(&handle, "nt-drv").await?;
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
    let (db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let mut node = make_node("clamp-drv");
    node.pname = "clamp-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    pull_report(
        &handle,
        "clamp-drv",
        PullReportPayload {
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
            final_line_count: 0,
            // Mechanical flag-off default (carve-out 1c).
            materialization_outcome: None,
        },
    )
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

/// Over-filtering guard: legitimate in-domain readings round-trip
/// unfiltered. (The valid-cgroup `cpu_limit_cores` round-trip — both
/// directions of the min() — is already covered by
/// `test_completion_writes_hw_class_and_intent_cores`.)
// r[verify sched.executor.input-bounds+2]
#[tokio::test]
async fn test_completion_valid_final_resources_round_trip() -> TestResult {
    let (db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let mut node = make_node("okres-drv");
    node.pname = "okres-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    pull_report(
        &handle,
        "okres-drv",
        PullReportPayload {
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
            final_line_count: 0,
            // Mechanical flag-off default (carve-out 1c).
            materialization_outcome: None,
        },
    )
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

    let (db, handle, _task) = setup().await;

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
        // bug_138: the lawful out_path rides the dispatch-minted
        // expected set (production-faithful fixture).
        let mut node = make_node(drv_tag);
        node.expected_output_paths = vec![out_path.clone()];
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                req: MergeDagRequest {
                    build_id,
                    tenant_id: Some(tenant),
                    priority_class: PriorityClass::Scheduled,
                    nodes: vec![node],
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

    // ONE open attempt (dedup proof). Second merge saw existing node,
    // just added build_b to interested_builds.
    let assignment = pull_attempt(&handle, drv_tag).await;
    assert_eq!(assignment.drv_path, drv_path, "dedup: one dispatch");

    // Complete with a real output_path. pull_complete_success sends
    // built_outputs[0].output_path → completion.rs:260 stores it on
    // state.output_paths → :365 reads it → :406 upserts.
    pull_complete_success(&handle, drv_tag, &out_path).await?;
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

    // Signed Q2: the direct-call battery exercises the BuiltLocally
    // class (locally produced bytes — the all-attributed cartesian is
    // lawful); witness-gated classes are pinned by
    // walk_success_stamps_only_wire_verified_tenants.
    let prov = crate::db::live_pins::StampProvenance::BuiltLocally;
    let first = sched_db
        .upsert_path_tenants(&paths, &tenants, &prov)
        .await?;
    assert_eq!(first, 6, "3 paths × 2 tenants = 6 rows inserted");

    let second = sched_db
        .upsert_path_tenants(&paths, &tenants, &prov)
        .await?;
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
/// that produced it (durably on the attempt row, surfaced through the
/// ListPoisoned aggregate). Pre-fix, only the success path
/// did the assignment write — every poisoned/cancelled derivation
/// kept a `pending` row, the pruner's `NOT EXISTS assignments` never
/// matched, and `derivations` leaked.
///
/// Part B proves the pruner can now actually delete: drop the
/// `build_derivations` link, run `gc_orphan_terminal_derivations`,
/// assert the row is gone (and the assignment row CASCADEd with it).
#[tokio::test]
async fn permanent_failure_terminals_assignment_and_records_executor() -> TestResult {
    let (db, handle, _task) = setup().await;

    let _ev = merge_single_node(&handle, Uuid::new_v4(), "i209", PriorityClass::Scheduled).await?;
    pull_complete_failure(
        &handle,
        "i209",
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

    // I-209: the executor that produced the permanent failure is
    // recorded durably on the attempt row and surfaced through the
    // operator listing (the attempt-ledger aggregate is the only
    // failure-history surface — migration 075 dropped the legacy
    // `failed_builders` column). On the pull path the
    // attempt's executor identity is the attested intent id.
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());
    let display = sched_db.load_poisoned_display().await?;
    let entry = display
        .iter()
        .find(|(path, _, _)| path == &test_drv_path("i209"))
        .expect("poisoned listing contains the i209 derivation");
    assert_eq!(
        entry.1,
        vec!["i209".to_string()],
        "I-209: ListPoisoned aggregate records the executor"
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
    let (db, handle, _task) = setup().await;

    let mut nodes = vec![make_node("fanin-leaf")];
    let mut edges = Vec::with_capacity(N);
    for i in 0..N {
        let tag = format!("fanin-p{i:03}");
        nodes.push(make_node(&tag));
        edges.push(make_test_edge(&tag, "fanin-leaf"));
    }
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, edges, true).await?;
    pull_complete_success_empty(&handle, "fanin-leaf").await?;
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

// r[verify sched.event.derivation-terminal]
/// bug_080: the terminal-failure epilogue STATES execution backing on
/// the wire — the trigger's event carries the caller's backing (a
/// worker-reported permanent failure => has_execution = true) and
/// EVERY cascaded DependencyFailed event states NoExecution (bystanders
/// swept from non-executing states). Pre-fix red is type-level
/// (disclosed): the field cannot pre-exist; the behavioral half is the
/// value split across trigger/cascade asserted here on the real actor.
#[tokio::test]
async fn cascaded_failed_event_states_no_execution() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let nodes = vec![
        make_node("vb-leaf"),
        make_node("vb-mid"),
        make_node("vb-top"),
    ];
    let edges = vec![
        make_test_edge("vb-mid", "vb-leaf"),
        make_test_edge("vb-top", "vb-mid"),
    ];
    let mut ev = merge_dag(&handle, Uuid::new_v4(), nodes, edges, true).await?;
    pull_complete_failure(
        &handle,
        "vb-leaf",
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "leaf busted",
    )
    .await?;
    barrier(&handle).await;

    let drv_events = drain_derivation_events(&mut ev);
    let failed_kind = rio_proto::types::DerivationEventKind::Failed as i32;
    let perm = rio_proto::types::BuildResultStatus::PermanentFailure as i32;
    let dep = rio_proto::types::BuildResultStatus::DependencyFailed as i32;
    let trigger: Vec<_> = drv_events
        .iter()
        .filter(|d| d.kind == failed_kind && d.failure_status == perm)
        .collect();
    let cascaded: Vec<_> = drv_events
        .iter()
        .filter(|d| d.kind == failed_kind && d.failure_status == dep)
        .collect();
    assert_eq!(trigger.len(), 1, "one trigger Failed event");
    assert!(
        trigger[0].has_execution,
        "the worker-reported trigger states FreshExecution on the wire"
    );
    assert_eq!(cascaded.len(), 2, "both ancestors cascade");
    for d in cascaded {
        assert!(
            !d.has_execution,
            "every cascaded event states NoExecution ({})",
            d.derivation_path
        );
    }
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
    let (db, handle, _task) = setup().await;

    let nodes: Vec<_> = (0..N).map(|i| make_node(&format!("casc{i:02}"))).collect();
    let edges: Vec<_> = (0..N - 1)
        .map(|i| make_test_edge(&format!("casc{:02}", i + 1), &format!("casc{i:02}")))
        .collect();
    let mut ev = merge_dag(&handle, Uuid::new_v4(), nodes, edges, true).await?;
    pull_complete_failure(
        &handle,
        "casc00",
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.max_retries = 0; // first transient → poison
        c.retry_policy.backoff_base_secs = 0.0;
    });
    let _db = db;

    let mut ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "pmax-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;
    pull_complete_failure(
        &handle,
        "pmax-drv",
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
    let (_db, handle, _task) = setup().await;

    let mut ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "prog-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    // Open the pull attempt (drv → Running) before draining, so the
    // failure report below exercises the retry path.
    let _assignment = pull_attempt(&handle, "prog-drv").await;
    barrier(&handle).await;
    // Drain pre-failure events and clear the 250ms emit_progress
    // debounce so the retry-path emit isn't suppressed.
    while ev.try_recv().is_ok() {}
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    pull_complete_failure(
        &handle,
        "prog-drv",
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

    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let mut ev = merge_single_node(&handle, build_id, "ord-drv", PriorityClass::Scheduled).await?;

    // Open the pull attempt, then drain delivery-time events; stop once
    // we see the delivery-phase Progress (DrvStarted precedes it per the
    // assignment-started ordering).
    let _assignment = pull_attempt(&handle, "ord-drv").await;
    loop {
        let e = tokio::time::timeout(Duration::from_secs(5), ev.recv())
            .await
            .expect("dispatch event within 5s")?;
        if matches!(e.event, Some(Event::Progress(_))) {
            break;
        }
    }

    pull_complete_success_empty(&handle, "ord-drv").await?;
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
    let (_db, handle, _task) = setup().await;

    // Two independent derivations under keep_going; fail one,
    // succeed the other.
    let nodes = vec![make_node("kg-fail"), make_node("kg-ok")];
    let mut ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], true).await?;
    barrier(&handle).await;

    pull_complete_failure(
        &handle,
        "kg-fail",
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "boom",
    )
    .await?;
    barrier(&handle).await;
    pull_complete_success_empty(&handle, "kg-ok").await?;
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
            assert_eq!(
                f.status(),
                rio_proto::types::BuildResultStatus::PermanentFailure,
                "BuildFailed carries the worker's classification"
            );
        }
    }
    assert!(saw_failed, "keep_going build must emit BuildFailed");
    Ok(())
}

// r[verify sched.retry.per-executor-budget+4]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy.max_infra_retries = MAX;
    });
    let _db = db;
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

    // MAX failures → NOT poisoned yet (cap-check is BEFORE increment).
    // Each attempt is a fresh pull + infra-failure report.
    for i in 0..MAX {
        pull_complete_failure(
            &handle,
            "uni-drv",
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
    pull_complete_failure(
        &handle,
        "uni-drv",
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
/// exact execution's log instead of falling back to "latest exec for
/// this drv" — which is wrong after a retry or a later build's rebuild
/// of the same drv.
///
/// Uses the `exec_id` from the `WorkAssignment` (the wire carrier) as
/// the expected value — that's the same UUIDv7 minted in
/// `assign_to_worker` and stamped on `DerivationState` and
/// `assignments.exec_id`. The
/// completion handler reads `state.exec_id`; if any carrier disagrees
/// this test would catch it as a mismatch.
///
/// The write is fire-and-forget (`spawn_monitored`), so the test polls
/// PG with the established 10ms × 100 pattern rather than asserting
/// immediately after `barrier()` (which only drains the actor loop, not
/// background tasks).
// r[verify sched.merge.exec-correlation+8]
#[tokio::test]
async fn completion_records_build_exec_correlation() -> TestResult {
    let (db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let node = make_node("ec-drv");
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // The pull mint happened — capture the minted exec_id from the wire
    // carrier. This is the same UUIDv7 stamped on `state.exec_id` that
    // the completion handler will read.
    let assignment = pull_attempt(&handle, "ec-drv").await;
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

    pull_complete_success(&handle, "ec-drv", &test_store_path("ec-out")).await?;
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
/// r[verify sched.merge.exec-correlation+8]
#[tokio::test]
async fn exec_correlation_skips_terminal_builds() -> TestResult {
    use crate::state::{BuildInfo, BuildState};

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

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

    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing::default(),
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
    info1
        .transition_terminal(crate::state::SettledBuild {
            counts: crate::state::SettledCounts {
                total: 1,
                completed: 1,
                cached: 0,
                failed: 0,
            },
            outcome: crate::state::TerminalOutcome::Succeeded {
                output_paths: vec![],
            },
        })
        .unwrap();
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

// ---------------------------------------------------------------------------
// drv_executions lifecycle (harden-logs commit 3)
//
// One row per execution attempt: INSERTed by `record_assignment` at
// dispatch, stamped terminal by `terminal_log_epilogue`. rio-store's
// latest-exec resolution and log-completeness predicate read it; the
// gateway keys its TailLog subscription on the `exec_id` the Started
// event carries. These five tests pin the row's full lifecycle plus
// the two NULL-vs-value subtleties (`status IS NULL` = still running;
// `final_line_count IS NULL` = count not reported).
// ---------------------------------------------------------------------------

/// The `drv_executions` row opening an attempt creates: keyed by the
/// `drv_log_hash()` 32-char form of the *path* (not the DAG key), the
/// assigned executor, a non-NULL `started_at`, and — until the
/// execution terminates — a NULL `status` and NULL `final_line_count`.
#[tokio::test]
async fn dispatch_inserts_drv_executions_row() -> TestResult {
    let (db, handle, _task) = setup().await;
    let drv_hash = "dexe-drv";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;
    // The pull mint writes the fenced assignments + drv_executions rows
    // and transitions the node out of Ready in the same transaction.
    let _assignment = pull_attempt(&handle, drv_hash).await;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, drv_hash).await.status,
        DerivationStatus::Running,
        "the pull mint must open the attempt"
    );
    barrier(&handle).await;

    // The column holds drv_log_hash(<full drv path>) — the same value
    // the logs/{hash}/… S3 keys use — so a store-side reader that
    // normalizes its argument through the same helper finds this row.
    let expected_hash = rio_nix::store_path::drv_log_hash(&test_drv_path(drv_hash));
    let row: Option<(String, String, Option<String>, Option<i64>)> = sqlx::query_as(
        "SELECT drv_hash, executor_id, status, final_line_count \
         FROM drv_executions WHERE drv_hash = $1",
    )
    .bind(&expected_hash)
    .fetch_optional(&db.pool)
    .await?;
    let (got_hash, got_exec, got_status, got_count) =
        row.expect("dispatch must insert a drv_executions row");
    assert_eq!(got_hash, expected_hash);
    assert_eq!(got_exec, drv_hash);
    assert_eq!(
        got_status, None,
        "a still-running execution's status must be NULL (the store's \
         completeness predicate reads NULL as not-terminal)"
    );
    assert_eq!(
        got_count, None,
        "final_line_count is unknown until the CompletionReport arrives"
    );
    Ok(())
}

/// The terminal stamp: `status` becomes the EXEC_STATUS_* vocabulary's
/// "succeeded" (NOT `assignments.status`'s "completed"), `finished_at`
/// is set, and the report's `final_line_count` lands as-is.
#[tokio::test]
async fn terminal_stamps_drv_executions() -> TestResult {
    let (db, handle, _task) = setup().await;
    let drv_hash = "texe-drv";
    let drv_path = test_drv_path(drv_hash);
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    pull_report(
        &handle,
        drv_hash,
        PullReportPayload {
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("texe-out"),
                    output_hash: vec![0u8; 32],
                }],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_resources: None,
            final_line_count: 405,
            // Mechanical flag-off default (carve-out 1c).
            materialization_outcome: None,
        },
    )
    .await?;
    barrier(&handle).await;

    // The stamp is fire-and-forget (spawn_monitored) — poll PG.
    // Established 10ms × 100 pattern.
    let key = rio_nix::store_path::drv_log_hash(&drv_path);
    let mut row: Option<(Option<String>, Option<i64>, Option<i64>)> = None;
    for _ in 0..100 {
        row = sqlx::query_as(
            "SELECT status, final_line_count, \
             EXTRACT(EPOCH FROM finished_at)::bigint \
             FROM drv_executions WHERE drv_hash = $1",
        )
        .bind(&key)
        .fetch_optional(&db.pool)
        .await?;
        // Both columns: the assignment close stamps `status`
        // synchronously; the line count arrives via the equal-verdict
        // epilogue commute (sched.db.exec-stamp-on-close).
        if matches!(&row, Some((Some(_), Some(_), _))) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (status, count, finished_at) = row.expect("the dispatch-time row must exist");
    assert_eq!(
        status.as_deref(),
        Some(rio_migrations::schema::EXEC_STATUS_SUCCEEDED),
        "the terminal stamp must use the EXEC_STATUS_* vocabulary"
    );
    assert_eq!(
        count,
        Some(405),
        "the report's final_line_count lands verbatim"
    );
    assert!(
        finished_at.is_some(),
        "finished_at must be stamped alongside status"
    );
    Ok(())
}

/// An unusable `CompletionReport.final_line_count` must land as SQL
/// NULL ("not reported"), never as a literal value. Two unusable
/// shapes:
///
/// - `0` — the proto's "not reported" sentinel (an old executor, or
///   the count died with the build task). A literal 0 would tell the
///   store's completeness predicate "a zero-line log is complete".
/// - `> i64::MAX` — only a hostile or broken worker sends this. A
///   wrapping `as i64` cast would write a NEGATIVE count, which the
///   store's contiguity fold (`covered` starts at 0; complete ⇔
///   `covered >= count`) reads as vacuously complete with an EMPTY
///   manifest — sealing the log against any further append with zero
///   chunks stored.
#[tokio::test]
async fn terminal_with_zero_line_count_writes_null() -> TestResult {
    let (db, handle, _task) = setup().await;

    for (tag, reported_count) in [("zexe-zero", 0u64), ("zexe-overflow", u64::MAX)] {
        let drv_path = test_drv_path(tag);
        let _ev = merge_single_node(&handle, Uuid::new_v4(), tag, PriorityClass::Scheduled).await?;

        pull_report(
            &handle,
            tag,
            PullReportPayload {
                result: rio_proto::types::BuildResult {
                    status: rio_proto::types::BuildResultStatus::Built.into(),
                    built_outputs: vec![rio_proto::types::BuiltOutput {
                        output_name: "out".into(),
                        output_path: test_store_path(&format!("{tag}-out")),
                        output_hash: vec![0u8; 32],
                    }],
                    ..Default::default()
                },
                peak_memory_bytes: 0,
                peak_cpu_cores: 0.0,
                node_name: None,
                hw_class: None,
                final_resources: None,
                final_line_count: reported_count,
                // Mechanical flag-off default (carve-out 1c).
                materialization_outcome: None,
            },
        )
        .await?;
        barrier(&handle).await;

        let key = rio_nix::store_path::drv_log_hash(&drv_path);
        let mut row: Option<(Option<String>, Option<i64>)> = None;
        for _ in 0..100 {
            row = sqlx::query_as(
                "SELECT status, final_line_count FROM drv_executions WHERE drv_hash = $1",
            )
            .bind(&key)
            .fetch_optional(&db.pool)
            .await?;
            if matches!(&row, Some((Some(_), _))) {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        let (status, count) = row.expect("the dispatch-time row must exist");
        assert_eq!(status.as_deref(), Some("succeeded"), "{tag}");
        assert_eq!(
            count, None,
            "final_line_count = {reported_count} in the report is unusable \
             and must become SQL NULL — never a literal 0 and never a \
             wrapped negative"
        );
    }
    Ok(())
}

/// The terminal stamp is monotone: `AND status IS NULL` means the first
/// verdict to land wins. A second terminal for the same execution (a
/// completion racing a cancellation) must not overwrite the first.
///
/// The two verdicts are sequenced by awaiting each spawned stamp's join
/// handle: the guard's contract is arrival order at PG, not epilogue
/// call order — issuing both fire-and-forget and expecting the first
/// *call* to win was a race the test lost whenever the second UPDATE
/// reached the row first. Awaiting the second handle before the read
/// also makes the no-overwrite assertion a real happens-after check
/// instead of a sleep-bounded one.
#[tokio::test]
async fn second_terminal_does_not_overwrite() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing::default(),
    );

    // A node with a stamped exec_id, exactly as assign_to_worker leaves
    // it. The drv_executions row is seeded directly (the dispatch-time
    // INSERT is dispatch_inserts_drv_executions_row's subject; this
    // test isolates the UPDATE's monotone guard).
    let drv_hash = "mono-drv";
    let drv_path = test_drv_path(drv_hash);
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        drv_path: drv_path.clone(),
        ..crate::db::RecoveryDerivationRow::test_default(drv_hash, "x86_64-linux")
    });
    let exec_id = Uuid::now_v7();
    actor.dag.node_mut(drv_hash).expect("just injected").exec_id = Some(exec_id);
    let key = rio_nix::store_path::drv_log_hash(&drv_path);
    sqlx::query(
        "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at) \
         VALUES ($1, $2, 'mono-w', now())",
    )
    .bind(exec_id)
    .bind(&key)
    .execute(&db.pool)
    .await?;

    // First verdict: succeeded with a real line count. Await the
    // spawned stamp so it has committed before the second verdict is
    // issued — the racing-cancel scenario under test is "second
    // terminal arrives after the first stamped", and the guard must
    // make it a no-op.
    actor
        .terminal_log_epilogue(&DrvHash::from(drv_hash), "succeeded", &[], Some(10))
        .expect("exec_id is set, so the first verdict spawns a stamp")
        .await?;
    let (status, count): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT status, final_line_count FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        status.as_deref(),
        Some("succeeded"),
        "first stamp must land"
    );
    assert_eq!(count, Some(10), "first stamp must carry the line count");

    // Second verdict (a racing cancel): must be a no-op on the row.
    // Await its spawned write too, so the assertions below read the row
    // strictly after the second UPDATE has executed.
    actor
        .terminal_log_epilogue(&DrvHash::from(drv_hash), "cancelled", &[], None)
        .expect("the node still carries the exec_id, so the second verdict spawns too")
        .await?;
    let (status, count): (Option<String>, Option<i64>) =
        sqlx::query_as("SELECT status, final_line_count FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        status.as_deref(),
        Some("succeeded"),
        "the first terminal verdict must win (AND status IS NULL guard)"
    );
    assert_eq!(
        count,
        Some(10),
        "the second verdict must not NULL out the first's final_line_count"
    );
    Ok(())
}

/// The DerivationStarted event carries the execution's exec_id — the
/// value the gateway keys its per-execution TailLog subscription on.
/// It must be the SAME execution the drv_executions row records, or the
/// gateway subscribes to a log no writer is producing.
#[tokio::test]
async fn started_event_carries_exec_id() -> TestResult {
    use rio_proto::types::build_event::Event;

    let (db, handle, _task) = setup().await;
    let drv_hash = "sexe-drv";
    let mut events =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;
    // The pull mint stamps the exec_id and emits the Started event.
    let _assignment = pull_attempt(&handle, drv_hash).await;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, drv_hash).await.status,
        DerivationStatus::Running,
        "the pull mint must open the attempt"
    );

    // Drain until the DrvStarted event.
    let started_exec_id = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event within 5s")?;
        match ev.event {
            Some(Event::Derivation(d))
                if d.kind() == rio_proto::types::DerivationEventKind::Started =>
            {
                break d.exec_id;
            }
            _ => {}
        }
    };
    assert!(
        !started_exec_id.is_empty(),
        "DerivationStarted must carry the execution's exec_id"
    );
    let event_uuid = Uuid::parse_str(&started_exec_id)
        .expect("the Started event's exec_id must be a well-formed UUID");

    // Cross-check: the event names the same execution the lifecycle row
    // records. (UUIDv7 — minted by this dispatch, not a stale one.)
    let key = rio_nix::store_path::drv_log_hash(&test_drv_path(drv_hash));
    let row_exec: Uuid =
        sqlx::query_scalar("SELECT exec_id FROM drv_executions WHERE drv_hash = $1")
            .bind(&key)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        event_uuid, row_exec,
        "the Started event and the drv_executions row must name the same execution"
    );
    Ok(())
}

/// A-1a-2 (the failed-logs defect): a failure-terminal path whose
/// triggering worker report carries a usable `final_line_count` must
/// stamp it on the execution's `drv_executions` row — the store's
/// completeness predicate can then serve the failure log as complete
/// instead of holding it "incomplete" until the 30-day TTL. The
/// client-visible failure event keeps surfacing the worker's
/// `error_msg` (already true for the permanent path — regression pin).
#[tokio::test]
async fn failure_terminal_stamps_report_final_line_count() -> TestResult {
    let (db, handle, _task) = setup().await;
    let drv_hash = "flc-drv";
    let drv_path = test_drv_path(drv_hash);
    let mut ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    // Worker report: permanent failure, 37 log lines emitted.
    pull_report(
        &handle,
        drv_hash,
        PullReportPayload {
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "missing header: zlib.h".into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_resources: None,
            final_line_count: 37,
            // Mechanical flag-off default (carve-out 1c).
            materialization_outcome: None,
        },
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, drv_hash).await.status,
        DerivationStatus::Poisoned,
        "permanent failure must poison"
    );

    // The terminal stamp is fire-and-forget (spawn_monitored) — poll.
    let key = rio_nix::store_path::drv_log_hash(&drv_path);
    let mut row: Option<(Option<String>, Option<i64>)> = None;
    for _ in 0..100 {
        row = sqlx::query_as(
            "SELECT status, final_line_count FROM drv_executions WHERE drv_hash = $1",
        )
        .bind(&key)
        .fetch_optional(&db.pool)
        .await?;
        // Wait for BOTH columns: the assignment-close stamps `status`
        // synchronously with the poison persist, while the line count
        // arrives via the fire-and-forget epilogue (which commutes on
        // the equal verdict — sched.db.exec-stamp-on-close). Breaking
        // on status alone reads the gap between the two.
        if matches!(&row, Some((Some(_), Some(_)))) {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (status, count) = row.expect("the dispatch-time row must exist");
    assert_eq!(
        status.as_deref(),
        Some(rio_migrations::schema::EXEC_STATUS_FAILED),
        "failure terminal must stamp the execution failed"
    );
    assert_eq!(
        count,
        Some(37),
        "the failing report's final_line_count must land on drv_executions \
         (A-1a-2: today the failure epilogue stamps NULL)"
    );

    // The DerivationFailed event carries the worker's error message.
    let events = drain_derivation_events(&mut ev);
    assert!(
        events
            .iter()
            .any(|d| d.error_message.contains("missing header: zlib.h")),
        "the DerivationFailed event must surface the worker's error_msg, got {:?}",
        events
            .iter()
            .map(|d| d.error_message.clone())
            .collect::<Vec<_>>()
    );
    Ok(())
}

/// E1: each transient failure appends exactly one `transient` row — the
/// two retry-arm attempts and the final poison-arm attempt — carrying
/// the reporting worker and its error message.
#[tokio::test]
async fn attempt_ledger_e1_transient_rows() -> TestResult {
    let (db, handle, _task) = setup().await;
    let drv_hash = "ale1-drv";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    // Each attempt is a fresh pull (same intent identity, no binding)
    // followed by a transient-failure report through the report intake.
    for attempt in 0..3 {
        pull_complete_failure(
            &handle,
            drv_hash,
            rio_proto::types::BuildResultStatus::TransientFailure,
            &format!("attempt {attempt} failed"),
        )
        .await?;
    }
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, drv_hash).await.status,
        DerivationStatus::Poisoned,
        "max_retries exhausted → poisoned (decision unchanged by the ledger)"
    );

    let rows = ledger_rows(&db.pool, drv_hash).await;
    assert_eq!(
        rows.len(),
        3,
        "exactly one row per observed transient failure: {rows:?}"
    );
    for (i, r) in rows.iter().enumerate() {
        assert_eq!(r.event_kind, "attempt");
        assert_eq!(r.outcome_class, "transient", "row {i}");
        assert_eq!(r.executor_id.as_deref(), Some(drv_hash));
        assert_eq!(
            r.error_msg.as_deref(),
            Some(format!("attempt {i} failed").as_str()),
            "the worker's error message lands on the row"
        );
        assert_eq!(r.final_line_count, None, "0 = not reported → NULL");
        assert!(!r.exempt && !r.floor_promoted && !r.floor_at_cap);
        assert_eq!(r.resubmit_cycle, 0);
    }
    Ok(())
}

/// E2: a non-exempt infra failure appends an `infra` row; a
/// CONCURRENT_PUTPATH one appends `exempt_infra` with the exemption
/// flag set (and no floor promotion).
#[tokio::test]
async fn attempt_ledger_e2_infra_and_exempt_rows() -> TestResult {
    let (db, handle, _task) = setup().await;
    let drv_hash = "ale2-drv";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    // Non-exempt infra failure (FUSE-style worker-local error).
    pull_complete_failure(
        &handle,
        drv_hash,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        "fuse: transport endpoint is not connected",
    )
    .await?;
    // Exempt infra failure (lost the upload race to a concurrent PutPath).
    pull_complete_failure(
        &handle,
        drv_hash,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        rio_proto::CONCURRENT_PUTPATH_MSG,
    )
    .await?;
    barrier(&handle).await;

    let rows = ledger_rows(&db.pool, drv_hash).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert_eq!(rows[0].outcome_class, "infra");
    assert!(!rows[0].exempt);
    assert_eq!(rows[1].outcome_class, "exempt_infra");
    assert!(rows[1].exempt, "CONCURRENT_PUTPATH is cap-exempt");
    assert!(
        !rows[1].floor_promoted,
        "exempt via the message, not via a floor promotion"
    );
    Ok(())
}

/// E3: a permanent failure appends exactly one `permanent` row carrying
/// the worker's error message, the report's final_line_count, and the
/// execution id of the dispatched attempt.
#[tokio::test]
async fn attempt_ledger_e3_permanent_row() -> TestResult {
    let (db, handle, _task) = setup().await;
    let drv_hash = "ale3-drv";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    pull_report(
        &handle,
        drv_hash,
        PullReportPayload {
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::PermanentFailure.into(),
                error_msg: "missing header: zlib.h".into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_resources: None,
            final_line_count: 21,
            // Mechanical flag-off default (carve-out 1c).
            materialization_outcome: None,
        },
    )
    .await?;
    barrier(&handle).await;

    let rows = ledger_rows(&db.pool, drv_hash).await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    let r = &rows[0];
    assert_eq!(r.outcome_class, "permanent");
    assert_eq!(r.executor_id.as_deref(), Some(drv_hash));
    assert!(
        r.exec_id.is_some(),
        "the report-bearing attempt keeps its execution id"
    );
    assert_eq!(r.error_msg.as_deref(), Some("missing header: zlib.h"));
    assert_eq!(r.final_line_count, Some(21));
    assert_eq!(r.termination_reason, None, "single-installment row");
    Ok(())
}

/// E4: each TimedOut appends one `timeout` row — the under-cap retry
/// and the at-cap terminal Cancelled.
#[tokio::test]
async fn attempt_ledger_e4_timeout_rows() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy = crate::RetryPolicy {
            max_timeout_retries: 1,
            ..Default::default()
        };
    });
    let drv_hash = "ale4-drv";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    // Under cap → retry; cap (1) exhausted on the second → Cancelled.
    // Each attempt is a fresh pull + TimedOut report.
    for _ in 0..2 {
        pull_complete_failure(
            &handle,
            drv_hash,
            rio_proto::types::BuildResultStatus::TimedOut,
            "build exceeded daemon_timeout_secs",
        )
        .await?;
    }
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, drv_hash).await.status,
        DerivationStatus::Cancelled,
        "timeout cap exhausted → terminal Cancelled (decision unchanged)"
    );

    let rows = ledger_rows(&db.pool, drv_hash).await;
    assert_eq!(rows.len(), 2, "{rows:?}");
    assert!(rows.iter().all(|r| r.outcome_class == "timeout"));
    assert!(
        rows.iter()
            .all(|r| r.executor_id.as_deref() == Some(drv_hash))
    );
    Ok(())
}

/// A terminal failure cascades DependencyFailed to its dependent and
/// appends one `cascade` row for the dependent — a row with no
/// execution of its own, written in the same batch as the status
/// persist.
#[tokio::test]
async fn attempt_ledger_cascade_row_for_dependent() -> TestResult {
    let (db, handle, _task) = setup().await;
    let child = "alc-child";
    let parent = "alc-parent";
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node(child), make_node(parent)],
        vec![make_test_edge(parent, child)],
        false,
    )
    .await?;

    pull_complete_failure(
        &handle,
        child,
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "no such file or directory",
    )
    .await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, parent).await.status,
        DerivationStatus::DependencyFailed,
        "the dependent must cascade"
    );
    assert_eq!(
        ledger_classes(&db.pool, child).await,
        vec!["permanent"],
        "the trigger keeps exactly its own row"
    );
    let parent_rows = ledger_rows(&db.pool, parent).await;
    assert_eq!(parent_rows.len(), 1, "{parent_rows:?}");
    assert_eq!(parent_rows[0].outcome_class, "cascade");
    assert!(
        parent_rows[0].exec_id.is_none() && parent_rows[0].executor_id.is_none(),
        "a cascade victim never ran"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 1b (T-1b.2): E3 collapsed onto decide() — the verdict for a
// permanent failure comes from the fold over the appended attempt suffix
// inside the appending transaction. These tests pin equivalence with the
// as-built behavior (no verdict, cascade, or epilogue change).
// ---------------------------------------------------------------------------

/// Each of the seven permanent statuses still poisons the trigger,
/// cascades DependencyFailed to its dependent, emits the trigger's
/// DerivationFailed{PermanentFailure} epilogue event carrying the
/// worker's error message, persists `poisoned` to PG, and appends
/// exactly one `permanent` ledger row — the as-built outcomes,
/// unchanged by routing the verdict through decide().
#[tokio::test]
async fn phase1b_e3_permanent_statuses_poison_identically() -> TestResult {
    use rio_proto::types::BuildResultStatus as S;
    let statuses = [
        S::PermanentFailure,
        S::CachedFailure,
        S::DependencyFailed,
        S::LogLimitExceeded,
        S::OutputRejected,
        S::NotDeterministic,
        S::InputRejected,
    ];
    for (i, status) in statuses.into_iter().enumerate() {
        let (db, handle, _task) = setup().await;
        let child = format!("e3c{i}");
        let parent = format!("e3p{i}");
        let mut ev = merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![make_node(&child), make_node(&parent)],
            vec![make_test_edge(&parent, &child)],
            false,
        )
        .await?;
        let node = format!("e3-node-{i}");
        bind_intent_node(&handle, &child, &node).await?;
        pull_complete_failure(&handle, &child, status, "deterministic boom").await?;
        barrier(&handle).await;

        let trigger = expect_drv(&handle, &child).await;
        assert_eq!(
            trigger.status,
            DerivationStatus::Poisoned,
            "{status:?} must poison the trigger"
        );
        assert!(
            trigger.retry.poisoned_at.is_some(),
            "{status:?}: poisoned_at stamped"
        );
        assert!(
            trigger.retry.failed_builders.contains(node.as_str()),
            "{status:?}: the diagnostics-only exclusion insert keys on the \
             bound source node (rule 1, P12 key shape)"
        );
        assert_eq!(
            expect_drv(&handle, &parent).await.status,
            DerivationStatus::DependencyFailed,
            "{status:?} must cascade to the dependent"
        );
        assert_eq!(
            ledger_classes(&db.pool, &child).await,
            vec!["permanent"],
            "{status:?}: exactly one permanent row for the trigger"
        );
        let pg_status: String =
            sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(&child)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(pg_status, "poisoned", "{status:?}: PG status persisted");

        // Epilogue unchanged: the trigger's DerivationFailed event still
        // reports PermanentFailure and carries the worker's message.
        let drv_events = drain_derivation_events(&mut ev);
        let failed_kind = rio_proto::types::DerivationEventKind::Failed as i32;
        let perm = S::PermanentFailure as i32;
        assert!(
            drv_events.iter().any(|d| d.kind == failed_kind
                && d.failure_status == perm
                && d.error_message.contains("deterministic boom")),
            "{status:?}: trigger epilogue event unchanged ({drv_events:?})"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 1b (T-1b.3): E4 collapsed onto decide() — the under-cap requeue
// vs at-cap Cancelled verdict comes from the fold over the appended
// timeout suffix. Equivalence battery (single-tenure, worker-reported).
// ---------------------------------------------------------------------------

/// Under the cap a TimedOut still requeues with no backoff and no
/// exclusion-set entry; at the cap it still goes terminal Cancelled —
/// never Poisoned — and PG, the ledger, and the legacy RAM counter all
/// agree with the as-built outcomes.
#[tokio::test]
async fn phase1b_e4_timeout_verdicts_unchanged() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.retry_policy = crate::RetryPolicy {
            max_timeout_retries: 1,
            ..Default::default()
        };
    });
    let drv_hash = "e4-timeout";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    // ── Attempt 1: under cap → Requeue ──────────────────────────────
    pull_complete_failure(
        &handle,
        drv_hash,
        rio_proto::types::BuildResultStatus::TimedOut,
        "deadline exceeded",
    )
    .await?;
    barrier(&handle).await;
    let info = expect_drv(&handle, drv_hash).await;
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "under cap: requeued, got {:?}",
        info.status
    );
    assert_eq!(
        info.retry.timeout_count, 1,
        "legacy RAM counter still tracks"
    );
    assert!(
        info.retry.backoff_until.is_none(),
        "timeouts never back off"
    );
    assert!(
        info.retry.failed_builders.is_empty(),
        "timeouts never join the exclusion set"
    );

    // ── Attempt 2: cap exhausted → terminal Cancelled, not Poisoned ─
    pull_complete_failure(
        &handle,
        drv_hash,
        rio_proto::types::BuildResultStatus::TimedOut,
        "deadline exceeded",
    )
    .await?;
    barrier(&handle).await;
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.status,
        DerivationStatus::Cancelled,
        "at the cap: terminal Cancelled (immediately resubmit-retriable)"
    );
    assert!(
        info.retry.poisoned_at.is_none(),
        "Cancelled is not Poisoned — no 24 h lockout"
    );
    let pg_status: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = $1")
            .bind(drv_hash)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(pg_status, "cancelled", "PG persisted the verdict's status");
    assert_eq!(
        ledger_classes(&db.pool, drv_hash).await,
        vec!["timeout", "timeout"],
        "one ledger row per observed timeout"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Phase 1b (T-1b.4): E2 collapsed onto decide()/classify(). Worker-only
// histories are behavior-preserving; the two adjudicated mixed-channel
// deltas (D2, D3) become decision-visible here and are pinned red-first.
// ---------------------------------------------------------------------------

/// Worker-only equivalence battery: a mixed exempt / counted run keeps
/// the as-built outcomes (exempt attempts never consume the counted
/// budget, the legacy RAM counters keep tracking, no poison under the
/// caps) and the ledger carries the per-class rows the fold reads.
#[tokio::test]
async fn phase1b_e2_worker_only_infra_battery_counters_and_rows_agree() -> TestResult {
    let (db, handle, _task) = setup().await;
    let drv = "e2batt";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;
    let putpath = format!("upload failed: {}", rio_proto::CONCURRENT_PUTPATH_MSG);
    let msgs = ["fuse: EIO", putpath.as_str(), "store unreachable"];
    for msg in msgs {
        pull_complete_failure(
            &handle,
            drv,
            rio_proto::types::BuildResultStatus::InfrastructureFailure,
            msg,
        )
        .await?;
        barrier(&handle).await;
    }
    let info = expect_drv(&handle, drv).await;
    assert_ne!(info.status, DerivationStatus::Poisoned, "all under cap");
    assert_eq!(info.retry.infra_count, 2, "two counted infra failures");
    assert_eq!(info.retry.exempt_infra_count, 1, "one exempt attempt");
    assert_eq!(info.retry.count, 0, "infra never eats the transient budget");
    assert!(
        info.retry.failed_builders.is_empty(),
        "infra failures never join the exclusion set"
    );
    assert_eq!(
        ledger_classes(&db.pool, drv).await,
        vec!["infra", "exempt_infra", "infra"],
        "the append-time classification carries the exempt split"
    );
    Ok(())
}

/// Non-distinct threshold mode (`require_distinct_workers = false`):
/// the flat failure count drives the poison threshold, so the same
/// worker failing twice poisons at threshold 2 — with the verdict now
/// computed by the fold over the appended transient rows. The retry arm
/// before the threshold still arms the backoff and the legacy counters.
#[tokio::test]
async fn phase1b_e1_transient_threshold_non_distinct_mode_poisons() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.poison = crate::state::PoisonConfig {
            threshold: 2,
            require_distinct_workers: false,
        };
    });
    let drv = "e1nd";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;

    // Failure 1: under the threshold → retry with backoff. Same intent
    // identity on every attempt (the non-distinct flat count is the
    // subject).
    pull_complete_failure(
        &handle,
        drv,
        rio_proto::types::BuildResultStatus::TransientFailure,
        "flaky once",
    )
    .await?;
    barrier(&handle).await;
    let info = expect_drv(&handle, drv).await;
    assert_ne!(info.status, DerivationStatus::Poisoned);
    assert_eq!(info.retry.count, 1, "legacy RAM retry count still tracks");
    assert_eq!(info.retry.failure_count, 1);
    assert!(
        info.retry.backoff_until.is_some(),
        "the retry arm still arms the backoff at the site"
    );

    // Failure 2: flat count reaches the threshold → poison.
    pull_complete_failure(
        &handle,
        drv,
        rio_proto::types::BuildResultStatus::TransientFailure,
        "flaky twice",
    )
    .await?;
    barrier(&handle).await;
    let info = expect_drv(&handle, drv).await;
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "same-worker repeats reach the flat threshold in non-distinct mode"
    );
    let pg_status: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = $1")
            .bind(drv)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(pg_status, "poisoned");
    assert_eq!(
        ledger_classes(&db.pool, drv).await,
        vec!["transient", "transient"],
        "one ledger row per observed transient failure"
    );
    // (The pre-073 revision of this test additionally asserted the
    // frozen legacy retry_count mirror column stayed at its default;
    // migration 075 dropped the column, so "the per-cycle count lives
    // in the ledger rows alone" is now structural.)
    Ok(())
}

// r[verify sched.build.terminal-status-settled+3]
/// A late shared-node failure must not rewrite a settled build's outcome:
/// `handle_derivation_failure` for a resident terminal build is a no-op.
///
/// Staging: B1 builds X and succeeds. B2 re-merges X while X's output is
/// missing from the store (stale-Completed verify resets X to Ready),
/// then X fails permanently under B2's re-dispatch. The failure fan-out
/// reaches every interested build — including terminal B1, which is
/// still resident in X's interested-build set for the cleanup window.
/// B1's settled outcome (Succeeded, no error summary) must survive; B2
/// owns the failure.
#[tokio::test]
async fn test_terminal_build_outcome_not_rewritten_by_late_shared_node_failure() -> TestResult {
    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;

    let x_out = test_store_path("tow-x-out");

    // B1: single node X → built via pull → B1 Succeeded.
    let b1 = Uuid::new_v4();
    let mut x = make_node("tow-x");
    x.expected_output_paths = vec![x_out.clone()];
    merge_dag(&handle, b1, vec![x.clone()], vec![], false).await?;
    barrier(&handle).await;
    pull_complete_success(&handle, "tow-x", &x_out).await?;
    wait_for_status(&handle, "tow-x", DerivationStatus::Completed).await;
    let settled = query_status(&handle, b1).await?;
    assert_eq!(
        settled.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "precondition: B1 finished"
    );
    assert_eq!(
        settled.error_summary, "",
        "precondition: a succeeded build has no error summary"
    );

    // B2 re-merges X (+ sibling root Z so B2 stays live). The store
    // lacks x_out → the stale-Completed verify resets X to Ready. B1
    // (terminal, resident) keeps its interest in X.
    let b2 = Uuid::new_v4();
    let mut z = make_node("tow-z");
    z.expected_output_paths = vec![test_store_path("tow-z-out")];
    merge_dag(&handle, b2, vec![x, z], vec![], false).await?;
    barrier(&handle).await;
    let xs = expect_drv(&handle, "tow-x").await;
    assert!(
        matches!(
            xs.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "precondition: stale-Completed verify must reset X; got {:?}",
        xs.status
    );

    // X fails permanently under B2's re-dispatch. The per-build failure
    // handler runs for every interested build of X — including B1.
    pull_complete_failure(
        &handle,
        "tow-x",
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "boom",
    )
    .await?;
    barrier(&handle).await;

    // B2 owns the failure.
    let s2 = query_status(&handle, b2).await?;
    assert_eq!(
        s2.state,
        rio_proto::types::BuildState::Failed as i32,
        "the live build that owns the failed re-dispatch fails"
    );

    // B1 keeps its settled outcome: still Succeeded, no error summary,
    // no failed_derivation backfilled into a build that never failed.
    let after = query_status(&handle, b1).await?;
    assert_eq!(
        after.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "a settled build's terminal state is immutable"
    );
    assert_eq!(
        after.error_summary, "",
        "a late shared-node failure must not rewrite a settled \
         (succeeded) build's error summary"
    );
    Ok(())
}

/// bughunt-2 slot 3 (merged_bug_072 / bug_096): the status→ctx
/// classifier's evidence contract — `store_degraded` reaches the
/// failure handlers ONLY from the InfrastructureFailure arm. Every
/// other status (including the whole permanent family, which used to
/// forward the raw wire bit) classifies as no-store-evidence.
#[test]
fn failure_ctx_store_evidence_only_on_infra_arm() {
    use rio_proto::types::BuildResultStatus as S;
    let flagged = crate::domain::BuildResult {
        status: S::PermanentFailure,
        error_msg: "boom".into(),
        start_time: None,
        stop_time: None,
        built_outputs: Vec::new(),
        store_degraded: true,
        failure_classification: None,
    };
    for status in [
        S::TransientFailure,
        S::InfrastructureFailure,
        S::PermanentFailure,
        S::CachedFailure,
        S::DependencyFailed,
        S::LogLimitExceeded,
        S::OutputRejected,
        S::NotDeterministic,
        S::InputRejected,
        S::TimedOut,
        S::Cancelled,
        S::Unspecified,
    ] {
        let ctx = crate::actor::completion::failure_ctx_for(status, &flagged, Some(7), 0);
        assert_eq!(
            ctx.store_degraded(),
            status == S::InfrastructureFailure,
            "status {status:?} must carry store evidence iff infra"
        );
    }
}

/// bughunt-2 slot 3 policy pin: `FailureReportCtx` literal construction
/// is permitted ONLY inside `actor/report_ctx.rs` (the two
/// constructors), and `failure_ctx_for` is the sole non-test producer
/// on the intake path — a future arm constructing the ctx inline (or
/// calling `infra(…, result.store_degraded)` from a permanent arm)
/// shows up here as a count change, CI-red.
#[test]
fn failure_report_ctx_literal_construction_is_constructor_gated() {
    let completion_src = include_str!("../completion.rs");
    let report_ctx_src = include_str!("../report_ctx.rs");
    let literal = "FailureReportCtx {";
    assert_eq!(
        completion_src.matches(literal).count(),
        0,
        "completion.rs must not construct FailureReportCtx literally — \
         route through failure_ctx_for"
    );
    let body_literals =
        report_ctx_src.matches("Self {").count() - report_ctx_src.matches(") -> Self {").count();
    assert_eq!(
        body_literals, 2,
        "report_ctx.rs carries exactly the two constructor bodies"
    );
    assert_eq!(
        report_ctx_src.matches(literal).count(),
        0,
        "even report_ctx.rs constructs via Self, keeping the literal \
         grep unambiguous"
    );
    // The degraded-carrying constructor is invoked exactly once in
    // completion.rs: the InfrastructureFailure classifier row.
    assert_eq!(
        completion_src.matches("FailureReportCtx::infra(").count(),
        1,
        "exactly one infra-evidence production site (the classifier's \
         InfrastructureFailure row)"
    );
    // merged_bug_032: the raw wire bit has exactly ONE read site — the
    // disposition-gate call at the top of
    // handle_infrastructure_failure. Everything downstream consumes
    // the gated disposition.
    assert_eq!(
        completion_src.matches("report.store_degraded()").count(),
        1,
        "single raw-bit read site (the disposition gate)"
    );
}

/// merged_bug_032 (bughunt-2 slot 3 C2, red R3): an UNCORROBORATED
/// store-degraded flag is worker-supplied evidence — one node's word.
/// It charges the counted infra budget like any plain infrastructure
/// failure; no uncharged pacing.
#[tokio::test]
async fn test_store_degraded_uncorroborated_is_charged_infra() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let drv_hash = "sd-uncorroborated-drv";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // ONE flagged report, no second node, no scheduler-side store
    // failure: uncorroborated.
    pull_complete_failure_result(
        &handle,
        drv_hash,
        rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
            error_msg: "FUSE EIO: store unreachable".into(),
            store_degraded: true,
            ..Default::default()
        },
    )
    .await?;

    let s = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        s.retry.infra_count, 1,
        "uncorroborated flag charges the counted infra budget"
    );
    Ok(())
}

/// merged_bug_032 (bughunt-2 slot 3 C1+C2, red R4): even CORROBORATED
/// store-degraded reports are bounded — the 13th consecutive flagged
/// report (kernel free run = 12) falls through CHARGED into the
/// counted infra budget, so a worker stamping every report cannot mint
/// unbounded uncharged requeues.
#[tokio::test]
async fn test_store_degraded_run_bound_charges_thirteenth_report() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let drv_hash = "sd-run-bound-drv";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Corroborate via the scheduler-side store-health leg.
    handle.debug_mark_store_rpc_failure().await?;

    for attempt in 0..13 {
        pull_complete_failure_result(
            &handle,
            drv_hash,
            rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
                error_msg: format!("FUSE EIO: store unreachable (attempt {attempt})"),
                store_degraded: true,
                ..Default::default()
            },
        )
        .await?;
    }

    let s = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        s.retry.infra_count, 1,
        "the 13th consecutive report (run bound 12) is charged fallthrough"
    );
    assert_ne!(
        s.status,
        DerivationStatus::Poisoned,
        "one charge, far from the cap"
    );
    Ok(())
}

/// merged_bug_003 (bughunt-2 slot 3 C4): a failed appending transaction
/// re-delivers the ORIGINAL completion payload — the `store_degraded`
/// flag survives the mailbox roundtrip, so the re-delivered report
/// classifies exactly like the first delivery (uncharged paced, given
/// corroboration). RED (recorded, pre-fix): the requeue rebuilt the
/// result with `..Default::default()`, zeroing the flag (plus node
/// identity and telemetry) — the re-delivery charged the infra budget
/// (`left: 1 / right: 0`).
#[tokio::test]
async fn test_redelivered_completion_keeps_store_degraded_flag() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, p| {
        p.fail_next_attempt_append = true;
    });
    let build_id = Uuid::new_v4();
    let drv_hash = "echo-roundtrip-drv";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    let _ = &db;

    // Corroborate via the store-health leg (outlives the 1s redelivery
    // delay by the full 600s window).
    handle.debug_mark_store_rpc_failure().await?;

    // The plumbing flag fails the FIRST appending transaction →
    // RecordFailed → bounded re-enqueue of the completion event; the
    // re-delivery (flag consumed) succeeds.
    pull_complete_failure_result(
        &handle,
        drv_hash,
        rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::InfrastructureFailure.into(),
            error_msg: "FUSE EIO: store unreachable".into(),
            store_degraded: true,
            ..Default::default()
        },
    )
    .await?;

    // Wait out the 1s ATTEMPT_RECORD_REDELIVERY_DELAY.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    tick(&handle).await?;

    let s = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        s.retry.infra_count, 0,
        "the re-delivered flagged report stays in the uncharged paced class"
    );
    assert!(
        s.retry.backoff_until.is_some(),
        "paced backoff written by the re-delivered report"
    );
    Ok(())
}

/// merged_bug_294 + bug_077: a cancelled build's CompletionReport
/// arrives AFTER the scheduler's durable cancel close, carrying the
/// real post-footer `final_line_count`. The cancel-time stamp passed
/// None (no report existed yet) and the terminal stamp is monotone --
/// without the fill, the execution's fully-stored log reads
/// "incomplete" FOREVER (the store's completeness predicate never
/// passes, the log never seals, the builder's zero-loss CompleteLog
/// disposition cannot fire).
///
/// bug_077 (the witness-provenance rewrite): the previous regression
/// test called `handle_completion` DIRECTLY on a hand-injected world,
/// which hid that the production path never reached the in-body fill:
/// a real cancel closes the assignment durably, so the pod's late
/// report folds AckIgnore at the report intake (`fold_report`,
/// kani-pinned: Process requires an ACTIVE assignment) and the
/// gap-fill arm behind the Process gate was production-unreachable.
/// This test constructs the world through production constructors
/// ONLY -- attempt minted via the pull path, cancel via CancelBuild
/// (the durable close lands), late report via the report intake --
/// and asserts the fill happens on the LATE-REPORT (AckIgnore) lane.
// r[verify sched.executor.report-idempotent]
#[tokio::test]
async fn production_cancel_late_report_fills_the_line_count() -> TestResult {
    let (db, handle, _task) = setup().await;
    let build = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build, "cancel-count", PriorityClass::Scheduled).await?;

    // The pod opens its attempt through the real mint.
    let exec_id = open_pull_exec(&handle, "cancel-count").await;

    // The user cancels; the durable close lands before the reply
    // (assignment rows close in the terminal status persist) and the
    // cancel-time epilogue stamps status='cancelled' with a NULL
    // count.
    cancel_build(&handle, build).await?;

    // The pod's late Cancelled report arrives with the real
    // post-footer count -- after the close, so it folds AckIgnore.
    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::Cancelled.into(),
        ..Default::default()
    });
    payload.final_line_count = 7;
    pull_report_exec(&handle, exec_id, "cancel-count", payload).await?;

    // The epilogue's stamp is a spawned best-effort write: poll.
    let mut count: Option<i64> = None;
    for _ in 0..100 {
        count =
            sqlx::query_scalar("SELECT final_line_count FROM drv_executions WHERE exec_id = $1")
                .bind(exec_id)
                .fetch_one(&db.pool)
                .await?;
        if count.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        count,
        Some(7),
        "left: {count:?} / right: Some(7) (pre-fix: the AckIgnore lane swallowed \
         the count -- the log reads incomplete forever; the fill must ride the \
         late-report lane via the COALESCE equal-status gap-fill)"
    );
    Ok(())
}

/// bug_098 R1: after cancel → resubmit → re-dispatch, the late
/// report's fill lands on the REPORTING exec and the successor's row
/// stays unstamped — both halves of the load-bearing claim (no
/// foreign mint AND the true fill lands), through production
/// constructors only (the bug_077 R13 lane: real merge, real pull
/// mint, real CancelBuild, real report intake; the resubmit is the
/// merge's remove+reinsert via is_retriable_on_resubmit — no
/// Cancelled→Created edge exists).
// r[verify sched.executor.report-idempotent]
#[tokio::test]
async fn late_cancelled_report_fills_its_own_exec_not_the_successor() -> TestResult {
    let (db, handle, _task) = setup().await;
    let b1 = Uuid::new_v4();
    let _ev = merge_single_node(&handle, b1, "late-own-exec", PriorityClass::Scheduled).await?;

    // Attempt A through the real mint; the user cancels (durable
    // close lands; cancel-time epilogue stamps A 'cancelled', NULL
    // count).
    let exec_a = open_pull_exec(&handle, "late-own-exec").await;
    cancel_build(&handle, b1).await?;

    // The production resubmit: a second build re-merges the same drv
    // (remove+reinsert), and re-dispatch mints attempt B — running,
    // drv_executions row status IS NULL.
    let b2 = Uuid::new_v4();
    let _ev = merge_single_node(&handle, b2, "late-own-exec", PriorityClass::Scheduled).await?;
    let exec_b = open_pull_exec(&handle, "late-own-exec").await;
    assert_ne!(exec_a, exec_b, "the resubmit minted a fresh exec");

    // A's late Cancelled report arrives AFTER the resubmit (pod
    // SIGTERM-grace + report retries routinely span one), carrying
    // the real post-footer count.
    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::Cancelled.into(),
        ..Default::default()
    });
    payload.final_line_count = 7;
    pull_report_exec(&handle, exec_a, "late-own-exec", payload).await?;

    // The fill is a spawned best-effort write: poll until it lands on
    // either row (pre-fix it lands on B).
    let row = |exec: Uuid| {
        let pool = db.pool.clone();
        async move {
            let r: (Option<String>, Option<i64>) = sqlx::query_as(
                "SELECT status, final_line_count FROM drv_executions WHERE exec_id = $1",
            )
            .bind(exec)
            .fetch_one(&pool)
            .await
            .expect("exec row exists");
            r
        }
    };
    for _ in 0..100 {
        let (_, a_count) = row(exec_a).await;
        let (_, b_count) = row(exec_b).await;
        if a_count.is_some() || b_count.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    let (a_status, a_count) = row(exec_a).await;
    let (b_status, b_count) = row(exec_b).await;
    assert_eq!(
        (
            (a_status.as_deref(), a_count),
            (b_status.as_deref(), b_count)
        ),
        ((Some("cancelled"), Some(7)), (None, None)),
        "left: the fill re-resolved the CURRENT exec — successor B minted \
         'cancelled' with A's count and a fabricated finished_at via the \
         status-IS-NULL arm, A's count stayed NULL, and B's real verdict \
         now matches zero rows forever / right: A fills its OWN row; \
         B stays unstamped and running"
    );
    Ok(())
}

/// bug_098 R2: per-attempt count attribution — COALESCE first-wins
/// can no longer cross attempts. Both attempts cancelled; each late
/// report fills ITS OWN row (pre-fix: A's count landed on B through
/// the node's mutable carrier, and first-wins then blocked B's true
/// count forever). Production constructors only.
// r[verify sched.executor.report-idempotent]
#[tokio::test]
async fn cross_attempt_count_never_migrates() -> TestResult {
    let (db, handle, _task) = setup().await;
    let b1 = Uuid::new_v4();
    let _ev = merge_single_node(&handle, b1, "count-no-migrate", PriorityClass::Scheduled).await?;
    let exec_a = open_pull_exec(&handle, "count-no-migrate").await;
    cancel_build(&handle, b1).await?;

    let b2 = Uuid::new_v4();
    let _ev = merge_single_node(&handle, b2, "count-no-migrate", PriorityClass::Scheduled).await?;
    let exec_b = open_pull_exec(&handle, "count-no-migrate").await;
    cancel_build(&handle, b2).await?;

    // A's late report first (count 7), then B's own (count 9).
    let late = |count: u64| {
        let mut payload = pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::Cancelled.into(),
            ..Default::default()
        });
        payload.final_line_count = count;
        payload
    };
    pull_report_exec(&handle, exec_a, "count-no-migrate", late(7)).await?;
    pull_report_exec(&handle, exec_b, "count-no-migrate", late(9)).await?;

    let row = |exec: Uuid| {
        let pool = db.pool.clone();
        async move {
            let r: (Option<i64>,) =
                sqlx::query_as("SELECT final_line_count FROM drv_executions WHERE exec_id = $1")
                    .bind(exec)
                    .fetch_one(&pool)
                    .await
                    .expect("exec row exists");
            r.0
        }
    };
    // Poll until BOTH fills land (spawned best-effort writes).
    let mut pair = (None, None);
    for _ in 0..100 {
        pair = (row(exec_a).await, row(exec_b).await);
        if pair.0.is_some() && pair.1.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        pair,
        (Some(7), Some(9)),
        "left: B.final_line_count = 7 — A's count migrated to the successor \
         and first-wins blocked B's true count (A stayed NULL) / right: each \
         report fills its own attempt's row"
    );
    Ok(())
}

/// merged_bug_200: rio_scheduler_store_degraded_requeues_total ticked
/// at CLASSIFICATION time -- before the claims-floor fence, the
/// appending transaction, and the dag presence guard -- so the counter
/// counted outcomes that never settled: a fenced drop ticked "paced"
/// with no requeue, a failed appending tx ticked once per re-delivery
/// (N ticks, one committed row -- exactly during PG brownouts), and a
/// DAG-absent report ticked then early-returned. Same
/// per-delivery-vs-settled shape as the bug_086 width-event close: the
/// disposition now returns the label and the single POST-COMMIT site
/// emits it.
#[tokio::test]
async fn store_degraded_counter_ticks_only_on_commit() -> TestResult {
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'assigned') \
         RETURNING derivation_id",
    )
    .bind("settle-tick-drv")
    .bind(test_drv_path("settle-tick-drv"))
    .fetch_one(&db.pool)
    .await?;

    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id,
        ..crate::db::RecoveryDerivationRow::test_default("settle-tick-drv", "x86_64-linux")
    });
    actor
        .dag
        .node_mut("settle-tick-drv")
        .expect("just injected")
        .set_status_for_test(DerivationStatus::Assigned);
    // Corroborate via the store-health leg.
    actor.note_issued_store_rpc_failure("test-seed");

    // Delivery 1: the appending transaction FAILS (RecordFailed, the
    // report will be re-delivered). Nothing settled -- nothing ticks.
    actor.fail_next_attempt_append = true;
    let outcome = actor
        .handle_infrastructure_failure(
            &DrvHash::from("settle-tick-drv"),
            &crate::state::ExecutorId::from("w-st"),
            crate::actor::report_ctx::FailureReportCtx::infra(
                None,
                "FUSE EIO: store unreachable",
                true,
                None,
                0,
            ),
        )
        .await;
    assert!(
        matches!(
            outcome,
            crate::actor::completion::FailureHandling::RecordFailed
        ),
        "injection must fail the appending tx, got {outcome:?}"
    );
    let after_failed =
        recorder.get("rio_scheduler_store_degraded_requeues_total{disposition=paced}");
    assert_eq!(
        after_failed, 0,
        "left: {after_failed} / right: 0 (a failed appending tx settles \
         nothing -- the disposition counter must not tick per delivery)"
    );

    // Delivery 2 (the re-delivery): commits -- exactly one tick.
    let outcome = actor
        .handle_infrastructure_failure(
            &DrvHash::from("settle-tick-drv"),
            &crate::state::ExecutorId::from("w-st"),
            crate::actor::report_ctx::FailureReportCtx::infra(
                None,
                "FUSE EIO: store unreachable",
                true,
                None,
                0,
            ),
        )
        .await;
    assert!(
        matches!(outcome, crate::actor::completion::FailureHandling::Handled),
        "re-delivery must commit, got {outcome:?}"
    );
    assert_eq!(
        recorder.get("rio_scheduler_store_degraded_requeues_total{disposition=paced}"),
        1,
        "the committed re-delivery ticks exactly once"
    );
    Ok(())
}

// r[verify sched.sla.reactive-floor+4]
/// bug_027 companion red — the carried-at-ceiling cell of `bump_dim`'s
/// at_cap law: when the pod was DISPATCHED at the deadline cap (the
/// carried `BoundIntent` rendered 86400s) and the mint-time solve
/// resolved below it, a DeadlineExceeded must take the COUNTED at-cap
/// arm (`at_cap=true`, floor catches up to the cap, the retry budget
/// bounds it) — not the promotion-exempt path doubling from the
/// smaller mint value (an UNCOUNTED guaranteed-futile full-length
/// retry at a limit that provably failed; the exact uncounted
/// at-ceiling burn the base-not-floor fix existed to kill).
/// floor.rs/bump_dim is UNTOUCHED — the law's INPUT becomes honest
/// (the stamp carries the reconciled shape), and the hole closes
/// through the stamp alone. Pre-fix, verbatim: floor doubled to 7200
/// with `floor_promoted=true ∧ floor_at_cap=false` (exempt); post-fix:
/// floor == DEADLINE_CAP_SECS with `floor_at_cap=true ∧
/// floor_promoted=false` (counted).
#[tokio::test]
async fn carried_at_cap_deadline_exceeded_is_counted_not_exempt() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor(db.pool.clone());
    let drv_hash = "atcap-carried";
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;

    // The controller rendered the pod at the 24h deadline cap.
    handle
        .send_unchecked(ActorCommand::AckSpawnedIntents {
            rejected: vec![],
            reply: tokio::sync::oneshot::channel().0,
            spawned: vec![],
            unfulfillable_cells: vec![],
            registered_cells: vec![],
            observed_instance_types: vec![],
            bound_intents: vec![],
            binding_snapshot: Some(vec![rio_proto::types::BoundIntent {
                intent_id: drv_hash.into(),
                node_name: "node-7".into(),
                deadline_secs: crate::actor::floor::DEADLINE_CAP_SECS,
            }]),
        })
        .await?;
    barrier(&handle).await;

    let exec = open_pull_exec(&handle, drv_hash).await;
    let stamped = expect_drv(&handle, drv_hash)
        .await
        .sched
        .last_intent
        .as_ref()
        .expect("the mint stamps the dispatch shape")
        .deadline_secs;
    // bug_102: the genuine anchor — the attempt ran its full carried
    // 24h deadline before the controller's kill (backdated).
    assert!(
        handle
            .debug_backdate_running(drv_hash, u64::from(crate::actor::floor::DEADLINE_CAP_SECS))
            .await?
    );
    pull_report_exec(
        &handle,
        exec,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::TimedOut.into(),
            error_msg: "build exceeded activeDeadlineSeconds at the cap".into(),
            ..Default::default()
        }),
    )
    .await?;

    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.sched.resource_floor.deadline_secs,
        crate::actor::floor::DEADLINE_CAP_SECS,
        "dispatched at the cap ⇒ no growth possible ⇒ the floor catches \
         up to the cap (pre-fix: doubled from the {stamped}s mint value \
         to a floor still at/below the limit that provably failed)"
    );
    let rows = ledger_rows(&db.pool, drv_hash).await;
    let row = rows.last().expect("the TimedOut attempt row");
    assert!(
        row.floor_at_cap && !row.floor_promoted,
        "the carried-at-ceiling DeadlineExceeded must charge the COUNTED \
         at-cap arm, not ride promotion-exempt (got promoted={} at_cap={})",
        row.floor_promoted,
        row.floor_at_cap
    );
    assert_eq!(
        info.retry.timeout_count, 1,
        "the counted charge is what bounds the at-cap case"
    );
    Ok(())
}

// =======================================================================
// Round-9 WO-S1-1 — the signed registration invariant (§5-S Q1):
// "completed uploads survive cancellation as registered evidence."
// A late BUILT report on a cancelled derivation carries everything
// registration needs (paths, hashes, tenant via the durable interest
// rows, drv identity); the pre-fix lane discarded the bookkeeping
// while the bytes stayed durable — 1,735/4,529 (38.3%) of run-1's
// uploads lost their registration to the cancel-intake no-op arm.
// =======================================================================

/// W9-A face (a), red 1 — the WITHIN-GRACE cell (the measured incident
/// shape: p50 139ms / p99 19.3s after cancel; the cancelled node is
/// still DAG-resident). Production constructors only (the bug_077 R13
/// lane): real merge (tenanted build → durable builds/bd rows), real
/// pull mint, real CancelBuild (durable close + interest stripped),
/// late BUILT report through the production report intake
/// (`ReportPullOutcome` → the pull.rs AckIgnore lane).
///
/// Asserts the REGISTRATION half structurally on rows: the
/// `path_tenants` stamp for (output path × historically-interested
/// tenant) EXISTS post-report. The tenant-scoped FindMissingPaths
/// verdict rides this row by the pinned store/kernel laws (the I-217
/// table: owned ⇒ Visible regardless of signatures —
/// `rio_evidence_kernel::visibility::visibility_verdict`; the store's
/// `own_built_projection` consumes exactly these rows): the row IS the
/// FMP input, so "missing" flips to "present" for this tenant when the
/// row exists. Composition recorded per W9-A (rows + the cited pinned
/// laws; no live store RPC in this crate — PD-13).
// r[verify store.registration.cancel-survives]
#[tokio::test]
async fn late_built_report_after_cancel_registers_path_tenants() -> TestResult {
    use sha2::Digest;
    let (db, handle, _task) = setup().await;
    let build = Uuid::new_v4();
    // bug_138: the lawful late output must be a member of the durable
    // expected set (W10-O — the membership law must NOT regress the
    // lawful late registration this test exists for).
    let out_path = test_store_path("cancel-reg-out");
    let mut node = make_node("cancel-reg");
    node.expected_output_paths = vec![out_path.clone()];
    let _ev = merge_dag(&handle, build, vec![node], vec![], false).await?;

    // The pod opens its attempt through the real mint.
    let exec_id = open_pull_exec(&handle, "cancel-reg").await;

    // The user cancels: the durable close lands before the reply and
    // the build interest is stripped from the DAG (cold tenant
    // resolution is the ONLY lawful attribution from here on).
    cancel_build(&handle, build).await?;

    // The pod finished the build + upload during the SIGTERM grace;
    // its late BUILT report arrives after the durable close, so it
    // folds AckIgnore at the production report intake.
    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::Built.into(),
        built_outputs: vec![rio_proto::types::BuiltOutput {
            output_name: "out".into(),
            output_path: out_path.clone(),
            output_hash: vec![0u8; 32],
        }],
        ..Default::default()
    });
    payload.final_line_count = 3;
    pull_report_exec(&handle, exec_id, "cancel-reg", payload).await?;
    barrier(&handle).await;

    // The registration stamp: poll (the apply is best-effort PG work
    // behind the actor channel; barrier flushes the command, the row
    // write is awaited inline — one poll loop absorbs scheduling).
    let hash = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
    let mut tenants: Vec<Uuid> = vec![];
    for _ in 0..100 {
        tenants =
            sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_all(&db.pool)
                .await?;
        if !tenants.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        tenants,
        vec![DEFAULT_TEST_TENANT],
        "left: the late BUILT report's outputs carry NO path_tenants stamp \
         (the cancel-intake no-op arm discarded the registration; the bytes \
         are durable but tenant-invisible — the I-217 own-tenant-Hidden \
         channel) / right: the Register letter stamps the historically-\
         interested tenant through the censused writer"
    );
    Ok(())
}

/// W9-A face (b), red 1b — the BEYOND-GRACE/EVICTED cell: the same
/// report after cancel-context is GONE, driven STRUCTURALLY by the
/// production eviction path (`CleanupTerminalBuild` →
/// `handle_cleanup_terminal_build` reaps the orphaned terminal node),
/// never by clock advance (the RC-2 paused-clock ban). Identity is
/// cold-resolved from the persisted rows (derivations + bd + builds —
/// the attempt row names the drv; the durable interest names the
/// tenant). Pre-fix this report dies at the unknown-derivation
/// early-return — zero stamps (the face-(b) shipped truth).
// r[verify store.registration.cancel-survives]
#[tokio::test]
async fn late_built_report_after_eviction_registers_path_tenants() -> TestResult {
    use sha2::Digest;
    let (db, handle, _task) = setup().await;
    let build = Uuid::new_v4();
    // bug_138 / W10-O: lawful late output ∈ the durable expected set.
    let out_path = test_store_path("cancel-evict-out");
    let mut node = make_node("cancel-evict");
    node.expected_output_paths = vec![out_path.clone()];
    let _ev = merge_dag(&handle, build, vec![node], vec![], false).await?;
    let exec_id = open_pull_exec(&handle, "cancel-evict").await;
    cancel_build(&handle, build).await?;

    // The production eviction, driven structurally (no clock advance):
    // the delayed-cleanup command the terminal timer would post.
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: build })
        .await?;
    barrier(&handle).await;

    // The late BUILT report lands AFTER the reap: the node and the
    // build are gone from memory; only the durable rows remain.
    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::Built.into(),
        built_outputs: vec![rio_proto::types::BuiltOutput {
            output_name: "out".into(),
            output_path: out_path.clone(),
            output_hash: vec![0u8; 32],
        }],
        ..Default::default()
    });
    payload.final_line_count = 5;
    pull_report_exec(&handle, exec_id, "cancel-evict", payload).await?;
    barrier(&handle).await;

    let hash = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
    let mut tenants: Vec<Uuid> = vec![];
    for _ in 0..100 {
        tenants =
            sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_all(&db.pool)
                .await?;
        if !tenants.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        tenants,
        vec![DEFAULT_TEST_TENANT],
        "left: the evicted-node report died at the pre-chokepoint \
         unknown-derivation discard — zero stamps (the invariant is \
         unconditional; eviction must not un-register completed work) \
         / right: cold-resolved identity stamps the historically-\
         interested tenant"
    );
    Ok(())
}

/// W9-A face (b), the ProcessCompletion-shim sibling: the un-admitted
/// intake (`handle_completion`) has its own pre-chokepoint
/// unknown-derivation early-return — W9-C folds it into the alphabet's
/// sight (a Register-or-censused-sibling classification, never a
/// pre-classifier discard). Same eviction setup, report via the shim.
// r[verify store.registration.cancel-survives]
#[tokio::test]
async fn evicted_shim_report_registers_path_tenants() -> TestResult {
    use sha2::Digest;
    let (db, handle, _task) = setup().await;
    let build = Uuid::new_v4();
    // bug_138 / W10-O: lawful late output ∈ the durable expected set.
    let out_path = test_store_path("cancel-shim-out");
    let mut node = make_node("cancel-shim");
    node.expected_output_paths = vec![out_path.clone()];
    let _ev = merge_dag(&handle, build, vec![node], vec![], false).await?;
    let _exec_id = open_pull_exec(&handle, "cancel-shim").await;
    cancel_build(&handle, build).await?;
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: build })
        .await?;
    barrier(&handle).await;

    // The shim path: ProcessCompletion (the append-failure redelivery
    // echo's production lane) for the evicted drv.
    complete_success(&handle, "shim-w", "cancel-shim", &out_path).await?;
    barrier(&handle).await;

    let hash = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
    let mut tenants: Vec<Uuid> = vec![];
    for _ in 0..100 {
        tenants =
            sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
                .bind(&hash)
                .fetch_all(&db.pool)
                .await?;
        if !tenants.is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        tenants,
        vec![DEFAULT_TEST_TENANT],
        "left: the shim's unknown-derivation early-return discarded a \
         registrable BUILT report before classification / right: the \
         shim folds through the late-report chokepoint like every lane"
    );
    Ok(())
}

// =======================================================================
// Round-9 WO-S1-3 — the IDENTITY half of the signed Q1 invariant:
// registered evidence carries identity (deriver linkage / CA
// realisations) so resubmission re-associates. Forensics: realisations
// 0-written for the IA incident population; the control finding
// (consecutive resubmits shared 956/~2000 drvs with visibility intact,
// reuse capped at 0.62% structurally) proves visibility alone does not
// re-associate.
// =======================================================================

/// W9-F′ — the composed chain the full signed invariant quantifies
/// over, driven end-to-end by ONE witness: cancel → late Built report
/// → Register letter (stamps + identity) → resubmit the identical
/// un-salted drv → WARM solve (the merge's cache-hit lane completes
/// the node with zero dispatched attempts). Production constructors
/// throughout (real merge, real pull mint, real CancelBuild, the
/// production report intake, real re-merge).
///
/// The cross-crate seam, disclosed: the node's outputs are made
/// byte-complete by seeding the store tables the upload would have
/// written (narinfo + complete manifest — the scheduler test cannot
/// run the store's gRPC ingest, PD-13), and the mock store's FMP
/// answers "present" for them — exactly the verdict the REAL store's
/// pinned visibility law gives a stamped path (W9-D(b), commit 2).
///
/// The identity asserts (the commit-3 red): post-Register the
/// (path ↔ deriver) linkage EXISTS — narinfo.deriver carries the
/// drv_path for an upload whose uploader did NOT declare it (the
/// dev-mode/legacy-builder face; wire-declared deriver is never
/// overwritten by the monotone fill).
// r[verify store.registration.cancel-survives]
#[tokio::test]
async fn cancel_late_report_resubmit_solves_warm_with_identity() -> TestResult {
    use sha2::Digest;
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let drv = "ident-resub";
    let out_path = test_store_path("ident-resub-out");

    // The un-salted node: stable drv hash, declared output, KNOWN
    // expected output path (the IA cache-hit lane probes these).
    let mut node = make_node(drv);
    node.expected_output_paths = vec![out_path.clone()];

    let b1 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b1, vec![node.clone()], vec![], false).await?;
    let exec_id = open_pull_exec(&handle, drv).await;
    cancel_build(&handle, b1).await?;

    // The upload happened before the report (builder uploads, then
    // reports): seed the store rows the ingest would have written —
    // WITHOUT a declared deriver (the identity-gap face).
    let out_hash = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
         VALUES ($1, $2, $3, 0)",
    )
    .bind(&out_hash)
    .bind(&out_path)
    .bind(vec![0u8; 32])
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO manifests (store_path_hash, status, inline_blob) VALUES ($1, 'complete', '')",
    )
    .bind(&out_hash)
    .execute(&db.pool)
    .await?;

    // The late BUILT report → the Register letter.
    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::Built.into(),
        built_outputs: vec![rio_proto::types::BuiltOutput {
            output_name: "out".into(),
            output_path: out_path.clone(),
            output_hash: vec![0u8; 32],
        }],
        ..Default::default()
    });
    payload.final_line_count = 2;
    pull_report_exec(&handle, exec_id, drv, payload).await?;
    barrier(&handle).await;

    // Identity assert 1 (the red): the deriver linkage EXISTS.
    let deriver: Option<String> =
        sqlx::query_scalar("SELECT deriver FROM narinfo WHERE store_path_hash = $1")
            .bind(&out_hash)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        deriver,
        Some(test_drv_path(drv)),
        "left: the registration carried no identity (narinfo.deriver \
         absent — re-association has no (path ↔ deriver) record) / \
         right: the registration writer fills the deriver linkage"
    );

    // Registration assert (carried from W9-A): the stamp exists.
    let tenants: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&out_hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(tenants, vec![DEFAULT_TEST_TENANT]);

    // The store now answers "present" for the registered path — the
    // real store's pinned verdict for a stamped path (W9-D(b)).
    store.seed_with_content(&out_path, b"registered bytes");

    // The RESUBMIT: identical un-salted drv, fresh build. The merge's
    // cache-hit lane must complete it WARM — no dispatch.
    let b2 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b2, vec![node], vec![], false).await?;

    // Warm solve: the build reaches Succeeded with ZERO dispatched
    // attempts (structural: a pull finds nothing deliverable).
    let mut status = query_status(&handle, b2).await?;
    for _ in 0..100 {
        if status.state == rio_proto::types::BuildState::Succeeded as i32 {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        status = query_status(&handle, b2).await?;
    }
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "left: the resubmit re-built from scratch (no re-association; \
         the merge saw nothing reusable) / right: the registered + \
         identified outputs solve the resubmit WARM at merge"
    );
    let pull = try_pull_attempt(&handle, drv).await;
    assert!(
        !matches!(pull, Ok(PullOutcome::Deliver(_))),
        "warm solve must leave nothing dispatchable; got {pull:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// bug_138: worker-supplied output paths verify membership against the
// dispatch-minted expected set before any path_tenants stamp (W10-M)
// ---------------------------------------------------------------------------

/// W10-M — proposition: NO worker-supplied output path outside the
/// scheduler-authoritative expected set
/// (`state.expected_output_paths`, the same set the AssignmentClaims
/// mint signs at dispatch) reaches `path_tenants`, on ANY report lane.
/// The quantifier is both-lanes; this test drives BOTH: the admitted
/// success epilogue AND the late-report Register lane's evicted face
/// (the weaker-checked wave-9 replica).
///
/// The attack (the red this test was born failing on, pre-fix): a
/// compromised worker assigned drv X (tenant A's build) reports tenant
/// B's EXISTING store path as its own output. No upload occurs, so the
/// store's PutPath `path ∈ claims.expected_outputs` check
/// (put_path/common.rs — the enforcement this lane shadows) never
/// runs. The stamp lands `(B's path, tenant A)` with BuiltLocally
/// provenance; the store's `own_built_projection`
/// (`bool_or(tenant_id = $tid)` over exactly these rows) then yields
/// `owned=true` for A, and the I-217 law
/// (`rio_evidence_kernel::visibility::visibility_verdict`) flips B's
/// path Hidden → Visible for tenant A. This test asserts the verdict
/// cell itself through the projection's own SQL — the full I-217
/// assertion, not a weaker row-count sibling.
///
/// Post-fix: membership refusal — a TYPED, counted, attributed,
/// non-poisoning letter (`rio_scheduler_unexpected_built_output_total`
/// + the structured WARN; the report's lawful effects are unaffected
/// — the drv still completes on the admitted lane).
// r[verify sched.trust.report-membership+3]
#[tokio::test]
async fn forged_output_path_never_reaches_path_tenants_on_any_lane() -> TestResult {
    use sha2::Digest;
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (db, handle, _task) = setup().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    // Tenant B (the victim) owns two pre-existing paths, Hidden from
    // every other tenant under I-217 (built-by-another-tenant-only).
    let tenant_b = rio_store::test_helpers::seed_tenant(&db.pool, "forge-victim").await;
    let victim_adm = test_store_path("forge-victim-secret-adm");
    let victim_reg = test_store_path("forge-victim-secret-reg");
    let prov = crate::db::live_pins::StampProvenance::BuiltLocally;
    sched_db
        .upsert_path_tenants(
            &[victim_adm.clone(), victim_reg.clone()],
            &[tenant_b],
            &prov,
        )
        .await?;

    // The I-217 projection cell for tenant A over a path: EXACTLY the
    // store's `own_built_projection` semantics (visibility.rs —
    // CITE-ONLY surface: `bool_or(pt.tenant_id = $2) AS owned`,
    // any_built = group-exists), folded through the kernel's verdict.
    let verdict_for_a = |path: String, pool: sqlx::PgPool| async move {
        let hash = sha2::Sha256::digest(path.as_bytes()).to_vec();
        let owned: Option<bool> = sqlx::query_scalar(
            "SELECT bool_or(tenant_id = $2) FROM path_tenants WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .bind(DEFAULT_TEST_TENANT)
        .fetch_one(&pool)
        .await?;
        let (owned, any_built) = (owned.unwrap_or(false), owned.is_some());
        anyhow::Ok(rio_evidence_kernel::visibility::visibility_verdict(
            owned, any_built, false,
        ))
    };

    // Baseline: both victim paths Hidden from tenant A.
    for p in [&victim_adm, &victim_reg] {
        assert_eq!(
            verdict_for_a(p.clone(), db.pool.clone()).await?,
            rio_evidence_kernel::visibility::VisibilityVerdict::Hidden,
            "baseline: tenant B's path must be Hidden from tenant A (I-217)"
        );
    }

    // ── Lane 1: the ADMITTED success epilogue ──────────────────────
    // Tenant A's build, drv with a dispatch-minted expected set. The
    // worker reports B's path as its only output.
    let lawful_adm = test_store_path("forge-adm-out");
    let mut node = make_node("forge-adm");
    node.expected_output_paths = vec![lawful_adm.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let _assignment = pull_attempt(&handle, "forge-adm").await;
    pull_complete_success(&handle, "forge-adm", &victim_adm).await?;
    barrier(&handle).await;

    assert_eq!(
        verdict_for_a(victim_adm.clone(), db.pool.clone()).await?,
        rio_evidence_kernel::visibility::VisibilityVerdict::Hidden,
        "left (pre-fix): a worker-supplied output_path FLIPPED tenant \
         B's path Hidden → Visible for tenant A (owned=true via the \
         forged BuiltLocally row; no upload occurred — the full I-217 \
         visibility consequence on the admitted lane) / right: the \
         membership check refuses the non-member path; the verdict \
         stays Hidden"
    );
    let adm_hash = sha2::Sha256::digest(victim_adm.as_bytes()).to_vec();
    let stamped: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&adm_hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        stamped,
        vec![tenant_b],
        "left (pre-fix): the admitted epilogue stamped the forged path \
         for tenant A — path_tenants gained (B's path, A) with \
         BuiltLocally provenance, no upload required / right: the \
         membership check refuses the non-member path at the boundary; \
         only B's own row exists"
    );
    // Non-poisoning: the report's lawful effects are unaffected — the
    // drv still completes (the refusal letter is per-output, not a
    // report failure).
    let info = expect_drv(&handle, "forge-adm").await;
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "the membership refusal must not poison the completion itself"
    );

    // ── Lane 2: the late-report Register lane, EVICTED face ────────
    // (declared=None — the wave-9 lane with no membership check at
    // all pre-fix.) Cancel + evict, then the late forged report.
    let lawful_reg = test_store_path("forge-reg-out");
    let mut node = make_node("forge-reg");
    node.expected_output_paths = vec![lawful_reg.clone()];
    let build = Uuid::new_v4();
    let _ev = merge_dag(&handle, build, vec![node], vec![], false).await?;
    let exec_id = open_pull_exec(&handle, "forge-reg").await;
    cancel_build(&handle, build).await?;
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: build })
        .await?;
    barrier(&handle).await;

    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::Built.into(),
        built_outputs: vec![rio_proto::types::BuiltOutput {
            output_name: "out".into(),
            output_path: victim_reg.clone(),
            output_hash: vec![0u8; 32],
        }],
        ..Default::default()
    });
    payload.final_line_count = 3;
    pull_report_exec(&handle, exec_id, "forge-reg", payload).await?;
    barrier(&handle).await;
    // The Register apply awaits inline behind the actor channel;
    // barrier flushed it. A short settle absorbs runtime scheduling
    // before the ABSENCE assertion (presence-polling would be wrong:
    // post-fix nothing must appear).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let reg_hash = sha2::Sha256::digest(victim_reg.as_bytes()).to_vec();
    let stamped: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&reg_hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        stamped,
        vec![tenant_b],
        "left (pre-fix): the Register lane's evicted face stamped the \
         forged path for tenant A — the weaker-checked wave-9 replica \
         of the admitted hole / right: the cold membership check \
         against the durable expected set refuses it; only B's row \
         exists"
    );
    assert_eq!(
        verdict_for_a(victim_reg.clone(), db.pool.clone()).await?,
        rio_evidence_kernel::visibility::VisibilityVerdict::Hidden,
        "left (pre-fix): I-217 verdict FLIPPED for tenant A on the \
         late lane / right: Hidden holds"
    );

    // The typed refusal letter: counted once per refused output, one
    // per lane here.
    assert_eq!(
        recorder.get("rio_scheduler_unexpected_built_output_total{}"),
        2,
        "the membership refusal is a COUNTED letter (one per refused \
         output: admitted lane + Register lane), not a silent drop"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// bug_132: the CA-exempt face joins the membership law — no tenant-
// visibility stamp without store-recorded production evidence (W11-Q)
// ---------------------------------------------------------------------------

/// W11-Q — proposition: NO path becomes tenant-visible without
/// store-recorded production evidence, on the floating-CA face of
/// EITHER stamp lane. Population: both stamp lanes (admitted success
/// epilogue + late-report Register) × {resident, non-resident} — the
/// adversarial cells; the resident-late cell rides the same Register
/// applier as the evicted one (cold durable rows), driven here via
/// the evicted face exactly like W10-M's lane 2.
///
/// The attack (the red this test was born failing on, pre-fix): the
/// bug_138 membership law is De-Morgan-skipped for floating-CA
/// (`!is_ca || is_fixed_output` gates `retain_expected_members` on
/// BOTH lanes) on the censused claim that `verify_ca_store_path`
/// covers that face "exactly as on upload" — but the attack uploads
/// NOTHING (the victim path already exists; the store's content
/// recompute only runs inside PutPath), so a tenant submitting a
/// trivial `__contentAddressed` drv from a compromised builder stamps
/// any existing victim path into `path_tenants` (BuiltLocally), and
/// `own_built_projection` + `visibility_verdict(owned=true)` flips it
/// Visible cross-tenant — the I-217 flip one face over.
///
/// Post-fix: the CA face consults the store-recorded registration
/// evidence (the same `path_tenants` rows the visibility verdict
/// reads — the ingest-lane stamp the store mints for the uploading
/// claims-tenant, `r[store.registration.ingest-stamps]`): no
/// evidence for the reporting build's attributed cohort ⇒ a typed,
/// counted, non-poisoning refusal of the STAMP (the report's other
/// effects are unaffected; the drv still completes).
// r[verify sched.trust.report-corroboration+3]
#[tokio::test]
async fn ca_no_upload_report_never_flips_visibility_on_any_lane() -> TestResult {
    use sha2::Digest;
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (db, handle, _task) = setup().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    // Tenant B (the victim) owns two pre-existing paths — the store's
    // ingest-lane registration stamped them at B's upload (seeded via
    // the censused scheduler writer as the ingest-stamp stand-in, the
    // W10-M precedent: never a raw INSERT).
    let tenant_b = rio_store::test_helpers::seed_tenant(&db.pool, "ca-forge-victim").await;
    let victim_adm = test_store_path("ca-forge-victim-secret-adm");
    let victim_reg = test_store_path("ca-forge-victim-secret-reg");
    let prov = crate::db::live_pins::StampProvenance::BuiltLocally;
    sched_db
        .upsert_path_tenants(
            &[victim_adm.clone(), victim_reg.clone()],
            &[tenant_b],
            &prov,
        )
        .await?;

    // The I-217 projection cell for tenant A over a path: EXACTLY the
    // store's `own_built_projection` semantics (visibility.rs —
    // cite-only surface), folded through the kernel's verdict.
    let verdict_for_a = |path: String, pool: sqlx::PgPool| async move {
        let hash = sha2::Sha256::digest(path.as_bytes()).to_vec();
        let owned: Option<bool> = sqlx::query_scalar(
            "SELECT bool_or(tenant_id = $2) FROM path_tenants WHERE store_path_hash = $1",
        )
        .bind(&hash)
        .bind(DEFAULT_TEST_TENANT)
        .fetch_one(&pool)
        .await?;
        let (owned, any_built) = (owned.unwrap_or(false), owned.is_some());
        anyhow::Ok(rio_evidence_kernel::visibility::visibility_verdict(
            owned, any_built, false,
        ))
    };

    for p in [&victim_adm, &victim_reg] {
        assert_eq!(
            verdict_for_a(p.clone(), db.pool.clone()).await?,
            rio_evidence_kernel::visibility::VisibilityVerdict::Hidden,
            "baseline: tenant B's path must be Hidden from tenant A (I-217)"
        );
    }

    // A floating-CA node: `is_content_addressed=true`,
    // `is_fixed_output=false` — EXACTLY the claims-mint predicate the
    // CA exemption keys on. No dispatch-minted expected set exists
    // (floating-CA paths are computed post-build), which is the
    // attack surface: pre-fix NOTHING bounds the reported path value.
    let mk_ca_node = |tag: &str| {
        let mut node = make_node(tag);
        node.is_content_addressed = true;
        node.ca_modular_hash = {
            use sha2::{Digest, Sha256};
            Sha256::digest(format!("ca-forge:{tag}").as_bytes()).to_vec()
        };
        node
    };

    // ── Lane 1: the ADMITTED success epilogue, CA face ─────────────
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![mk_ca_node("ca-forge-adm")],
        vec![],
        false,
    )
    .await?;
    let _assignment = pull_attempt(&handle, "ca-forge-adm").await;
    pull_complete_success(&handle, "ca-forge-adm", &victim_adm).await?;
    barrier(&handle).await;

    let adm_hash = sha2::Sha256::digest(victim_adm.as_bytes()).to_vec();
    let stamped: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&adm_hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        stamped,
        vec![tenant_b],
        "left (pre-fix): the admitted epilogue's CA-exempt arm stamped \
         the forged path for tenant A — path_tenants gained (B's path, \
         A) with BuiltLocally provenance, no upload required / right: \
         the production-evidence consult finds no store-recorded \
         registration for A's cohort and withholds the stamp; only B's \
         own row exists"
    );
    assert_eq!(
        verdict_for_a(victim_adm.clone(), db.pool.clone()).await?,
        rio_evidence_kernel::visibility::VisibilityVerdict::Hidden,
        "left (pre-fix): a no-upload CA report FLIPPED tenant B's path \
         Hidden → Visible for tenant A (the full I-217 consequence on \
         the admitted lane's CA face) / right: Hidden holds"
    );
    // Non-poisoning: the refusal withholds the STAMP, never the
    // completion — the report is recorded and the drv completes.
    let info = expect_drv(&handle, "ca-forge-adm").await;
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "the evidence refusal must not poison the completion itself"
    );

    // ── Lane 2: the late-report Register lane, CA face (evicted) ───
    let build = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build,
        vec![mk_ca_node("ca-forge-reg")],
        vec![],
        false,
    )
    .await?;
    let exec_id = open_pull_exec(&handle, "ca-forge-reg").await;
    cancel_build(&handle, build).await?;
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: build })
        .await?;
    barrier(&handle).await;

    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::Built.into(),
        built_outputs: vec![rio_proto::types::BuiltOutput {
            output_name: "out".into(),
            output_path: victim_reg.clone(),
            output_hash: vec![0u8; 32],
        }],
        ..Default::default()
    });
    payload.final_line_count = 3;
    pull_report_exec(&handle, exec_id, "ca-forge-reg", payload).await?;
    barrier(&handle).await;
    // Settle before the ABSENCE assertion (post-fix nothing appears).
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let reg_hash = sha2::Sha256::digest(victim_reg.as_bytes()).to_vec();
    let stamped: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&reg_hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        stamped,
        vec![tenant_b],
        "left (pre-fix): the Register lane's CA-exempt arm stamped the \
         forged path for tenant A (the wave-9 weaker-checked replica, \
         CA face) / right: the evidence consult against the durable \
         cohort refuses it; only B's row exists"
    );
    assert_eq!(
        verdict_for_a(victim_reg.clone(), db.pool.clone()).await?,
        rio_evidence_kernel::visibility::VisibilityVerdict::Hidden,
        "left (pre-fix): I-217 verdict FLIPPED for tenant A on the \
         late lane's CA face / right: Hidden holds"
    );

    // The realisation plane takes the same bound: a worker-supplied
    // (modular_hash → victim_path) mapping must NOT become durable
    // truth — the CA-cutoff cascade later stamps skipped nodes' paths
    // FROM `realisations` under plain BuiltLocally, so an unbounded
    // insert here resurrects the flip one lane over.
    let realisation_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM realisations WHERE output_path = $1")
            .bind(&victim_adm)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        realisation_rows, 0,
        "left (pre-fix): the forged CA report planted a realisation row \
         mapping the attacker's modular hash to the victim path / right: \
         the evidence bound refuses the mapping at the insert"
    );

    // The typed refusal letter: one per refused output per lane —
    // counted on the CA-evidence counter, NOT the expected-set one
    // (different law, different letter).
    assert_eq!(
        recorder.get("rio_scheduler_unevidenced_ca_output_total{}"),
        2,
        "the CA evidence refusal is a COUNTED letter (one per refused \
         output: admitted lane + Register lane), not a silent drop"
    );
    assert_eq!(
        recorder.get("rio_scheduler_unexpected_built_output_total{}"),
        0,
        "the CA face refuses on the EVIDENCE law, not the expected-set \
         law (the dispatch-minted set does not exist for floating-CA)"
    );
    Ok(())
}

/// W11-R — the honest CA flow is unbroken: upload-then-report stamps
/// exactly as today. The builder uploaded its real CA output before
/// reporting, so the store's ingest-lane registration stamp exists
/// for the build's attributed tenant; the evidence consult passes and
/// the completion stamp widens to all interested tenants under the
/// signed Q2 BuiltLocally law (locally produced bytes — all
/// interested tenants lawful).
// r[verify sched.trust.report-corroboration+3]
#[tokio::test]
async fn ca_honest_upload_then_report_stamps_as_today() -> TestResult {
    use sha2::Digest;
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (db, handle, _task) = setup().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    let honest_out = test_store_path("ca-honest-out");

    // The store's ingest-lane stamp: the builder's PutPath ran under
    // the exec's AssignmentClaims (tenant = the attributed tenant),
    // and the store stamped (path, claims-tenant) after the content
    // recompute passed. Seeded via the censused writer as the
    // ingest-stamp stand-in (the W10-M precedent).
    let prov = crate::db::live_pins::StampProvenance::BuiltLocally;
    sched_db
        .upsert_path_tenants(
            std::slice::from_ref(&honest_out),
            &[DEFAULT_TEST_TENANT],
            &prov,
        )
        .await?;

    let mut node = make_node("ca-honest");
    node.is_content_addressed = true;
    node.ca_modular_hash = sha2::Sha256::digest(b"ca-honest").to_vec();
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let _assignment = pull_attempt(&handle, "ca-honest").await;
    pull_complete_success(&handle, "ca-honest", &honest_out).await?;
    barrier(&handle).await;

    let hash = sha2::Sha256::digest(honest_out.as_bytes()).to_vec();
    let stamped: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        stamped,
        vec![DEFAULT_TEST_TENANT],
        "honest CA flow: the evidence consult passes on the ingest \
         stamp and the completion stamp lands for the interested \
         tenant — the upload-then-report path is byte-identical to \
         the pre-fix behavior"
    );
    let info = expect_drv(&handle, "ca-honest").await;
    assert_eq!(info.status, DerivationStatus::Completed);
    assert_eq!(
        recorder.get("rio_scheduler_unevidenced_ca_output_total{}"),
        0,
        "the honest flow takes zero refusal letters"
    );
    Ok(())
}

/// **W12-J (bug_155, the round-12 HIGH)** — *proposition: no
/// `realisations` row exists without store production evidence, at
/// the GLOBAL table's own scope — quantified over EVERY floating-CA
/// face INCLUDING the untenanted one (NULL-tenant anon/dev build);
/// the negation is the live forgery: a token-holding worker on an
/// untenanted build durably mints a forged modular_hash→victim_path
/// mapping with zero corroboration, first-writer-wins blocks the
/// later honest row, and the GLOBAL read path serves the flip to the
/// other tenant's resolve.*
///
/// The bug_132 close's own untenanted face: both lanes' empty-cohort
/// `ca_evidence = None` arm skipped the membership law ENTIRELY at
/// `ca_insert_realisations` — the "no boundary to guard" vacuity
/// rationale is true ONLY for the tenant-keyed `path_tenants` stamp
/// reader; `realisations` is globally keyed (PK (drv_hash =
/// modular_hash, output_name); `query_batch` /
/// `query_prior_realisation` are tenant-unscoped), so the empty
/// cohort is exactly where the insert must REFUSE ALL, never stand
/// down. Post-fix the evidence requirement derives from the CONSUMER
/// table's scope: empty cohort ⇒ empty evidence set ⇒ every path
/// refuses (fail-closed structural).
///
/// The forged half rides `r13-allow(forged-report)` (the forgery IS
/// the test); the honest half is production constructors end-to-end
/// (censused stamp writer + the pull report path). The read leg
/// consults `rio_store::realisations::query_batch` — the EXACT
/// consumer fn the scheduler resolve/merge path consults
/// (ca/resolve.rs `resolve_ca_inputs`; actor/merge.rs) — and
/// `query_prior_realisation` (the cutoff-compare consumer), both
/// tenant-unscoped: the W11-Q end-to-end precedent's global-read
/// manifestation.
// r[verify sched.trust.report-corroboration+3]
// r[verify sched.trust.evidence-scope]
#[tokio::test]
async fn untenanted_floating_ca_report_never_mints_global_realisations() -> TestResult {
    use sha2::Digest;
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (db, handle, _task) = setup().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    // Tenant B (the victim) owns a real path — stamped at upload via
    // the censused writer (the W10-M ingest-stamp stand-in).
    let tenant_b = rio_store::test_helpers::seed_tenant(&db.pool, "untenanted-ca-victim").await;
    let victim = test_store_path("untenanted-ca-victim-secret");
    let prov = crate::db::live_pins::StampProvenance::BuiltLocally;
    sched_db
        .upsert_path_tenants(std::slice::from_ref(&victim), &[tenant_b], &prov)
        .await?;

    // ONE modular hash, two drvs: the realisations PK is
    // (modular_hash, output_name), so the attacker's row and the
    // honest row collide exactly when both builds realise the same
    // modular hash — the first-writer-wins face under test.
    let mh = sha2::Sha256::digest(b"untenanted-ca-forge").to_vec();
    let mh32: [u8; 32] = mh.as_slice().try_into().unwrap();
    let mk_ca = |tag: &str| {
        let mut n = make_node(tag);
        n.is_content_addressed = true;
        n.ca_modular_hash = mh.clone();
        n
    };

    // ── The forged half (r13-allow(forged-report)) ─────────────────
    // The UNTENANTED build: `tenant_id: None` — the anon/dev face the
    // cold/warm cohort resolution returns empty for.
    let _ev = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: Uuid::new_v4(),
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![mk_ca("untenanted-ca-forge")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    let _assignment = pull_attempt(&handle, "untenanted-ca-forge").await;
    // The forged report: claims tenant B's path as this build's output.
    pull_complete_success(&handle, "untenanted-ca-forge", &victim).await?;
    barrier(&handle).await;

    // THE GLOBAL INSERT — the coupled second reader the cohort-keyed
    // guard never covered.
    let forged: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM realisations WHERE drv_hash = $1 AND output_path = $2",
    )
    .bind(mh.as_slice())
    .bind(&victim)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        forged, 0,
        "left (pre-fix): the untenanted lane's ca_evidence=None arm \
         skipped the membership law and the forged \
         modular_hash→victim_path row LANDED in the GLOBAL \
         realisations table / right: empty cohort ⇒ empty evidence \
         set ⇒ the insert refuses every path"
    );
    // The read leg, resolve-path consumer (tenant-unscoped).
    let served =
        rio_store::realisations::query_batch(&db.pool, &[(mh32, "out".to_string())]).await?;
    assert!(
        served.is_empty(),
        "left (pre-fix): query_batch — the resolve/merge consult — \
         served the FORGED victim mapping to any tenant's resolve / \
         right: no row exists, nothing is served"
    );
    // The read leg, cutoff-compare consumer: a DIFFERENT build asking
    // "did any prior build realise this path?" must not see a forged
    // prior (pre-fix it minted cross-tenant cutoff grants from it).
    let other_mh = sha2::Sha256::digest(b"untenanted-ca-other").to_vec();
    let other32: [u8; 32] = other_mh.as_slice().try_into().unwrap();
    let prior = crate::ca::query_prior_realisation(&db.pool, &victim, &other32).await?;
    assert!(
        prior.is_none(),
        "left (pre-fix): query_prior_realisation served the forged row \
         as a PRIOR REALISATION of the victim path (the cutoff-compare \
         flip) / right: none exists"
    );
    // The refusal is a TYPED letter (counted at the consult), and it
    // is non-poisoning: the untenanted completion itself succeeds.
    assert_eq!(
        recorder.get("rio_scheduler_unevidenced_ca_output_total{}"),
        1,
        "the untenanted refusal is COUNTED (one per refused output), \
         never a silent drop"
    );
    let info = expect_drv(&handle, "untenanted-ca-forge").await;
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "the evidence refusal withholds the realisation, never the \
         completion (non-poisoning; bytes durable; heal = lawful \
         registration/re-stamp — the disclosed degraded posture)"
    );

    // ── The honest half (production constructors; no r13-allow) ────
    // A DIFFERENT, TENANTED build realises the SAME modular hash with
    // its REAL output: uploaded first (the ingest stamp IS the
    // production evidence), then reported.
    let honest_out = test_store_path("untenanted-ca-honest-out");
    sched_db
        .upsert_path_tenants(
            std::slice::from_ref(&honest_out),
            &[DEFAULT_TEST_TENANT],
            &prov,
        )
        .await?;
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![mk_ca("untenanted-ca-honest")],
        vec![],
        false,
    )
    .await?;
    let _assignment = pull_attempt(&handle, "untenanted-ca-honest").await;
    pull_complete_success(&handle, "untenanted-ca-honest", &honest_out).await?;
    barrier(&handle).await;

    // First-writer-wins adjudicates: pre-fix the forged row blocked
    // this insert (ON CONFLICT DO NOTHING) and the resolve consult
    // kept serving the victim path; post-fix the honest row is the
    // only writer and the same consumer serves it.
    let served =
        rio_store::realisations::query_batch(&db.pool, &[(mh32, "out".to_string())]).await?;
    assert_eq!(
        served.len(),
        1,
        "the honest evidence-backed row must exist for the resolve consult"
    );
    assert_eq!(
        served[0].output_path, honest_out,
        "left (pre-fix): first-writer-wins served the FORGED victim \
         path to the honest tenant's own resolve (the cross-tenant \
         flip through the global read path) / right: the honest row \
         landed and is served"
    );
    Ok(())
}

/// **W12-J2 (bug_155, the aged-out late-lane orbit)** — *proposition:
/// the refusal quantifier covers the LATE Register lane whose tenant
/// cohort aged out — the cold resolve (`b.tenant_id IS NOT NULL`)
/// returns an empty cohort for a drv whose build rows the retention
/// sweep removed, and the empty cohort refuses the realisation insert
/// exactly as the anon face does.* The late HONEST re-report refusing
/// here is part of the DISCLOSED behavior delta (wave-log disclosure
/// at the owning commit): bytes stay durable, the row is absent, the
/// heal lane is lawful re-registration/re-stamp.
///
/// Aging stand-in: the build/build_derivations rows are deleted
/// directly — modeling the builds-retention sweep's row removal (the
/// producer of the aged-out state; the drv row itself survives, which
/// is exactly what makes the cold resolve succeed with zero tenants).
// r[verify sched.trust.report-corroboration+3]
#[tokio::test]
async fn aged_out_late_ca_report_refuses_realisations() -> TestResult {
    use sha2::Digest;
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (db, handle, _task) = setup().await;

    let mh = sha2::Sha256::digest(b"aged-late-ca").to_vec();
    let mut node = make_node("aged-late-ca");
    node.is_content_addressed = true;
    node.ca_modular_hash = mh.clone();

    let build = Uuid::new_v4();
    let _ev = merge_dag(&handle, build, vec![node], vec![], false).await?;
    let exec_id = open_pull_exec(&handle, "aged-late-ca").await;
    // Cancel WITHOUT cleanup: the node stays RESIDENT (status
    // Cancelled) so the Register lane's realisation insert has a
    // resolvable modular hash — the exact face where the unguarded
    // arm minted durable rows. (A cleaned/evicted node's insert
    // no-ops on residency — the bug_132 close's priced residual —
    // which would mask the row-landing red this orbit exists to pin.)
    cancel_build(&handle, build).await?;
    barrier(&handle).await;

    // The aging: retention removes the build rows; the derivation row
    // survives. The cold resolve's LEFT JOIN then yields the drv with
    // ZERO tenanted builds — the empty cohort.
    sqlx::query("DELETE FROM build_derivations WHERE build_id = $1")
        .bind(build)
        .execute(&db.pool)
        .await?;
    sqlx::query("DELETE FROM builds WHERE build_id = $1")
        .bind(build)
        .execute(&db.pool)
        .await?;

    // The late report (honest-shaped: the build's own output path —
    // the refusal below is the DISCLOSED delta, not an attack claim).
    let late_out = test_store_path("aged-late-ca-out");
    let mut payload = pull_payload(rio_proto::types::BuildResult {
        status: rio_proto::types::BuildResultStatus::Built.into(),
        built_outputs: vec![rio_proto::types::BuiltOutput {
            output_name: "out".into(),
            output_path: late_out.clone(),
            output_hash: vec![0u8; 32],
        }],
        ..Default::default()
    });
    payload.final_line_count = 1;
    pull_report_exec(&handle, exec_id, "aged-late-ca", payload).await?;
    barrier(&handle).await;
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM realisations WHERE drv_hash = $1")
        .bind(mh.as_slice())
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        rows, 0,
        "left (pre-fix): the aged-out late lane's empty cohort took \
         the ca_evidence=None arm and the realisation landed \
         unguarded (the same face a forger reaches by waiting out \
         retention) / right: empty cohort ⇒ refuse — the late honest \
         re-report's refusal is the disclosed delta, healed by lawful \
         re-registration"
    );
    assert_eq!(
        recorder.get("rio_scheduler_unevidenced_ca_output_total{}"),
        1,
        "the aged-out refusal is the same COUNTED letter"
    );
    Ok(())
}

/// **W12-K (bug_155, the satisfiable honest faces)** — *proposition:
/// the law's honest faces are PRESERVED at the law's own scope — a
/// tenanted, evidence-backed floating-CA report (the single- and
/// multi-tenant deployments' face: non-empty cohort, real upload)
/// inserts its realisation EXACTLY as today and the global consumers
/// serve it; on the untenanted face the witness is the REFUSAL plus
/// the degraded-posture disclosure (asserted in W12-J/W12-J2 — the
/// reds are this witness's teeth), because evidence is structurally
/// unrepresentable there: `path_tenants` is tenant-keyed, so an empty
/// cohort has no consultable evidence row by construction.*
// r[verify sched.trust.report-corroboration+3]
#[tokio::test]
async fn tenanted_evidence_backed_ca_realisation_lands_exactly_as_today() -> TestResult {
    use sha2::Digest;
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (db, handle, _task) = setup().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    let honest_out = test_store_path("ca-evidence-realisation-out");
    let prov = crate::db::live_pins::StampProvenance::BuiltLocally;
    sched_db
        .upsert_path_tenants(
            std::slice::from_ref(&honest_out),
            &[DEFAULT_TEST_TENANT],
            &prov,
        )
        .await?;

    let mh = sha2::Sha256::digest(b"ca-evidence-realisation").to_vec();
    let mh32: [u8; 32] = mh.as_slice().try_into().unwrap();
    let mut node = make_node("ca-evidence-realisation");
    node.is_content_addressed = true;
    node.ca_modular_hash = mh.clone();
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let _assignment = pull_attempt(&handle, "ca-evidence-realisation").await;
    pull_complete_success(&handle, "ca-evidence-realisation", &honest_out).await?;
    barrier(&handle).await;

    let served =
        rio_store::realisations::query_batch(&db.pool, &[(mh32, "out".to_string())]).await?;
    assert_eq!(
        served.len(),
        1,
        "tenanted + evidence-backed: the realisation row lands exactly \
         as today (the honest path is byte-identical pre/post fix)"
    );
    assert_eq!(served[0].output_path, honest_out);
    assert_eq!(
        recorder.get("rio_scheduler_unevidenced_ca_output_total{}"),
        0,
        "zero refusal letters on the evidence-backed face"
    );
    let info = expect_drv(&handle, "ca-evidence-realisation").await;
    assert_eq!(info.status, DerivationStatus::Completed);
    Ok(())
}

/// **W12-L (bug_102)** — *proposition: no worker-supplied signal
/// moves a persisted cross-tenant floor without a scheduler-anchored
/// corroboration witness, quantified over floor MUTATIONS — the
/// status-borne `TimedOut` lane included; the negation is the live
/// ratchet: a hostile builder's zero-age TimedOut reports double
/// `resource_floor.deadline_secs` per report toward the 24h cap into
/// the GREATEST() ratchet that never heals downward.*
///
/// The wave-11 corroboration gate covered only CgroupOom/DiskFull
/// riding `failure_classification`; TimedOut rides STATUS, bypassing
/// SizingClaim entirely — no attempt-age-vs-assigned-deadline anchor
/// consulted, and the standing census quantified over FailureClass
/// CARRIERS, leaving the status lane structurally outside its corpus.
/// Post-fix `bump_resource_floor` itself demands the typed
/// `CorroborationWitness`; the timeout axis corroborates on
/// attempt-open-duration >= assigned_deadline/2 (the scheduler's own
/// `running_since` stamp — an anchor the worker cannot mint).
///
/// Forged half via `r13-allow(forged-report)` (zero-age TimedOut
/// reports through the production pull path); the verdict flow is
/// asserted UNTOUCHED (timeouts still charge `timeout_count` — the
/// close is classify-only on the floor axis, never a retry change).
// r[verify sched.trust.report-corroboration+3]
#[tokio::test]
async fn forged_timeout_reports_never_move_the_deadline_floor() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (_db, handle, _task) = setup().await;

    let drv = "forged-timeout-drv";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;

    // Five forged TimedOut reports (max_timeout_retries=4 requeues,
    // the fifth takes the terminal Cancel verdict) with ~zero attempt
    // age — dispatch then report immediately, the ratchet shape a
    // hostile builder can drive for free.
    for i in 0..5 {
        let exec = open_pull_exec(&handle, drv).await;
        pull_report_exec(
            &handle,
            exec,
            drv,
            pull_payload(rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::TimedOut.into(),
                error_msg: format!("forged timeout {i} (zero attempt age)"),
                ..Default::default()
            }),
        )
        .await?;
        barrier(&handle).await;
    }

    let s = expect_drv(&handle, drv).await;
    assert_eq!(
        s.sched.resource_floor.deadline_secs, 0,
        "left (pre-fix): five zero-age TimedOut reports RATCHETED the \
         deadline floor (doubling per report toward DEADLINE_CAP_SECS \
         into the GREATEST() ratchet) with no corroboration anchor \
         consulted / right: uncorroborated timeout claims are \
         classify-only — the floor never moves"
    );
    assert_eq!(
        recorder.get("rio_scheduler_uncorroborated_sizing_claim_total{class=timed_out}"),
        5,
        "every refused timeout claim is a COUNTED letter on the \
         standing refusal counter (the typed-claim siblings' alphabet \
         gains the timed_out label)"
    );
    // The verdict flow is untouched: four timeouts charged the
    // timeout budget (max_timeout_retries=4) and the fifth took the
    // terminal Cancelled verdict — classify-only means the FLOOR is
    // inert, never the retry accounting.
    assert_eq!(
        s.status,
        DerivationStatus::Cancelled,
        "the timeout budget verdict is unchanged by the floor refusal"
    );
    Ok(())
}

/// **W12-L2 (bug_102, the true-positive arm)** — *proposition: the
/// genuine-timeout path still heals capacity — an attempt that
/// demonstrably ran at least half its assigned deadline (the
/// scheduler's own `running_since` clock vs the reconciled
/// `last_intent.deadline_secs`) corroborates, and the floor doubles
/// exactly as the wave-11 behavior intended for honest slow builds.*
// r[verify sched.trust.report-corroboration+3]
#[tokio::test]
async fn corroborated_slow_build_timeout_still_heals_the_deadline_floor() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (_db, handle, _task) = setup().await;

    let drv = "honest-slow-drv";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;
    let exec = open_pull_exec(&handle, drv).await;
    // The scheduler-side anchor: this attempt RAN for 1800s of its
    // 3600s assigned deadline (running_since backdated through the
    // debug seam — the same stamp the Running transition mints;
    // production constructors end-to-end otherwise).
    assert!(handle.debug_backdate_running(drv, 1800).await?);
    handle
        .debug_seed_sched_hint(drv, None, None, Some(3600), None)
        .await?;
    pull_report_exec(
        &handle,
        exec,
        drv,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::TimedOut.into(),
            error_msg: "honest timeout after a real slow run".into(),
            ..Default::default()
        }),
    )
    .await?;
    barrier(&handle).await;

    let s = expect_drv(&handle, drv).await;
    assert_eq!(
        s.sched.resource_floor.deadline_secs, 7200,
        "corroborated (open 1800s >= 3600/2): the floor doubles from \
         the assigned deadline — honest slow builds still heal"
    );
    assert_eq!(
        recorder.get("rio_scheduler_uncorroborated_sizing_claim_total{class=timed_out}"),
        0,
        "zero refusal letters on the corroborated path"
    );
    Ok(())
}
