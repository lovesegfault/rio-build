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
/// CA-compare fires before the test body arms fault flags), the
/// tests that arm `content_lookup_hang` / `fail_content_lookup`
/// AFTER the fixture would silently become vacuous.
///
/// Proof: the `rio_scheduler_ca_hash_compares_total` counter stays at
/// 0 after the fixture returns (no CA-compare ran), then increments
/// after `complete_ca` (the compare fires NOW, not earlier). Additionally
/// arms `fail_content_lookup` post-fixture and observes the Err path
/// take effect — direct proof that post-fixture flag arming still
/// reaches the first compare.
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
         the callsite; post-fixture fault-flag arming is now vacuous"
    );
    assert_eq!(
        recorder.get(match_key),
        0,
        "CA-compare fired during setup_ca_fixture"
    );

    // Arm a fault flag AFTER setup. If the fixture raced past, this
    // would be too late to affect anything.
    f.store
        .fail_content_lookup
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // Drive to completion — the CA-compare fires NOW.
    complete_ca(
        &f.actor,
        &f.worker_id,
        &f.drv_path,
        &[("out", &test_store_path("ca-race-out"), vec![0x42; 32])],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-race-guard")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);

    // The flag armed AFTER setup DID take effect: fail_content_lookup
    // → Err(Unavailable) → miss counter increments. If the compare
    // had already fired pre-arm, we'd see 0 here (or a match if the
    // store happened to have an entry, but it doesn't).
    assert_eq!(
        recorder.get(miss_key),
        1,
        "fail_content_lookup armed AFTER fixture did not take effect — \
         fixture raced past CA-compare"
    );
    assert!(
        !info.ca_output_unchanged,
        "RPC Err → matched=false → ca_output_unchanged=false"
    );

    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// CA early-cutoff compare: on successful completion of an `is_ca`
/// derivation, `handle_success_completion` looks up each output's
/// nar_hash in the content index. All-match → `ca_output_unchanged =
/// true`. The metric is labeled `{outcome=match|miss}`.
///
/// Three scenarios in one test (shared MockStore + actor setup, which
/// is the expensive part):
///   1. Output hash matches a seeded content-index entry → `true`,
///      counter{match} += 1.
///   2. Output hash doesn't match anything → `false`, counter{miss}
///      += 1.
///   3. Non-CA derivation → hook skipped entirely, counter untouched,
///      flag stays `false` regardless of whether the hash would've
///      matched. Proves the `is_ca` guard.
///
/// AND-fold correctness (multi-output with [match, miss] → `false`)
/// is covered by sending two BuiltOutputs in scenario 2.
#[tokio::test]
async fn ca_completion_hash_compare_sets_unchanged_and_counts() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    // Three scenarios sharing one actor/store/worker — setup_ca_fixture
    // merges the first scenario's single CA node; scenarios 2-3 do
    // manual merges against the SAME actor (additional builds).
    let f = setup_ca_fixture("ca-match").await?;

    // Seed a content-index entry: MockStore's content_lookup scans
    // stored paths for nar_hash == content_hash. The nar_hash here is
    // what the worker reports as `output_hash` on completion.
    //
    // This simulates a PRIOR build having uploaded identical content
    // at `prior_out`. Scenarios below report a DIFFERENT output_path
    // (the "new build" path) so the self-exclusion filter doesn't
    // hide this prior entry (store.content.self-exclude).
    let prior_out = test_store_path("ca-prior-out");
    let (_nar, seeded_hash) = f.store.seed_with_content(&prior_out, b"reproducible");

    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";
    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";

    // ─── Scenario 1: CA + matching hash → unchanged=true ───────────
    // (node already merged by setup_ca_fixture)

    // Precondition: merged as is_ca.
    let pre = f
        .actor
        .debug_query_derivation("ca-match")
        .await?
        .expect("exists");
    assert!(pre.is_ca, "precondition: merged with is_ca=true");
    assert!(!pre.ca_output_unchanged, "default false before completion");

    let match_before = recorder.get(match_key);
    // DIFFERENT path from prior_out — simulates a rebuild producing
    // the same content at a new input-addressed name.
    // ContentLookup(H, exclude=this-path) finds prior_out → match.
    // output_hash = seeded nar_hash → ContentLookup finds it.
    complete_ca(
        &f.actor,
        &f.worker_id,
        &f.drv_path,
        &[(
            "out",
            &test_store_path("ca-match-out"),
            seeded_hash.to_vec(),
        )],
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
        "CA + all-outputs-matched → ca_output_unchanged=true"
    );
    assert_eq!(
        recorder.get(match_key) - match_before,
        1,
        "one matched output → counter{{outcome=match}} +1.\nCounters: {:#?}",
        recorder.all_keys()
    );

    // ─── Scenario 2: CA + mixed [match, miss] → AND-fold → false ───
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-mixed");
    let mut node = make_test_node("ca-mixed", "x86_64-linux");
    node.is_content_addressed = true;
    node.output_names = vec!["out".into(), "dev".into()];
    let _ev = merge_dag(&f.actor, build_id, vec![node], vec![], false).await?;

    let miss_before = recorder.get(miss_key);
    let match_before2 = recorder.get(match_key);
    // First output matches (same seeded hash, DIFFERENT path than
    // prior_out → exclude doesn't hide the prior entry). Second
    // output: unknown hash → miss.
    complete_ca(
        &f.actor,
        &f.worker_id,
        &drv_path,
        &[
            (
                "out",
                &test_store_path("ca-mixed-out"),
                seeded_hash.to_vec(),
            ),
            ("dev", &test_store_path("ca-mixed-dev"), vec![0xabu8; 32]),
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
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ia-skip");
    let node = make_test_node("ia-skip", "x86_64-linux"); // is_content_addressed=false
    let _ev = merge_dag(&f.actor, build_id, vec![node], vec![], false).await?;

    let match_before3 = recorder.get(match_key);
    let miss_before3 = recorder.get(miss_key);
    // Hash WOULD match if the hook ran — proves the is_ca guard,
    // not a coincidental miss.
    complete_ca(
        &f.actor,
        &f.worker_id,
        &drv_path,
        &[("out", &test_store_path("ia-skip-out"), seeded_hash.to_vec())],
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
// r[verify store.content.self-exclude]
/// First-build self-exclusion (bughunt-mc196 regression).
///
/// MockStore seeded with ONE content_index row `(H, P)` — simulating
/// the worker's own PutPath having inserted the row BEFORE
/// BuildComplete fires (which the real worker always does; PutPath
/// completes before the build result is sent).
///
/// CA completion for a derivation with `built_outputs = [{output_path=P,
/// output_hash=H}]`. The `exclude_store_path = P` wired at
/// `completion.rs:T4` makes the lookup ask "seen H under a DIFFERENT
/// path?" → no → miss → `ca_output_unchanged = false`.
///
/// **BEFORE the fix:** `ContentLookup(H)` (no exclude) found P →
/// matched → `ca_output_unchanged = true` → P0252 would skip
/// downstream. WRONG: this is the FIRST build; nothing should skip.
///
/// Mutation check (plan exit criterion): remove the
/// `exclude_store_path: output.output_path.clone()` line at T4 and
/// this test FAILS with `ca_output_unchanged == true`.
#[tokio::test]
async fn ca_compare_first_build_excludes_own_upload() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let f = setup_ca_fixture("ca-first").await?;

    // THE SELF-MATCH SETUP: the only content_index row for H is
    // (H, own_path) — exactly what PutPath inserts for this build.
    // Seeded AFTER fixture setup is fine: the CA-compare reads the
    // store at ProcessCompletion time, not at merge time.
    let own_path = test_store_path("ca-first-out");
    let (_nar, own_hash) = f.store.seed_with_content(&own_path, b"first-ever");

    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";
    let miss_before = recorder.get(miss_key);
    let match_before = recorder.get(match_key);

    // output_path == the ONLY seeded path → exclude_store_path hides it
    // → no sibling → miss.
    complete_ca(
        &f.actor,
        &f.worker_id,
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
        "FIRST build: ContentLookup(H, exclude=own_path) → None → \
         ca_output_unchanged=false. (Before fix: matched own PutPath row → true.)"
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
/// Slow-store regression guard: `content_lookup` that NEVER responds
/// must NOT block completion past `CONTENT_LOOKUP_TIMEOUT`. The
/// timeout wrapper at `completion.rs` is what keeps the scheduler
/// responsive — if a refactor removes it, every CA build's
/// `handle_completed` awaits the store forever.
///
/// MockStore `content_lookup_hang=true` → `content_lookup` awaits
/// `pending()` (never resolves). Outer test timeout: 10s bounds the
/// test itself; the CA-compare hook uses a fixed 2s
/// `CONTENT_LOOKUP_TIMEOUT` constant (NOT `grpc_timeout` — see the
/// module comment at the constant for why) so the wrapper fires at
/// ~2s, well inside 10s. If the wrapper is removed, the test hits
/// the 10s outer bound and fails with "timeout".
///
/// Doesn't use `start_paused`: the real TCP between actor and
/// MockStore means the scheduler's `tokio::time::timeout` races a
/// real socket read; pausing time wouldn't help (and would break
/// the PG pool acquisition per lang-gotchas). Pays ~2s wall-clock.
#[tokio::test]
async fn ca_cutoff_compare_slow_store_doesnt_block_completion() -> TestResult {
    // 3s instead of the 30s production default — same wrapper-exists
    // proof at 10× less wall-clock.
    let f = setup_ca_fixture_configured("ca-slow", |a| a.with_grpc_timeout(Duration::from_secs(3)))
        .await?;

    // Arm the hang BEFORE any CA completion fires. Fixture returns
    // with the node Ready/Assigned — CA-compare hasn't run yet
    // (proved by setup_ca_fixture_does_not_race_past_ca_compare).
    f.store
        .content_lookup_hang
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // Send completion with a valid 32-byte output_hash. The CA
    // compare hook WILL call content_lookup → hang.
    complete_ca(
        &f.actor,
        &f.worker_id,
        &f.drv_path,
        &[("out", &test_store_path("ca-slow-out"), vec![0xAB; 32])],
    )
    .await?;

    // Outer guard: 10s. The hook's CONTENT_LOOKUP_TIMEOUT (2s) fires
    // at ~2s, letting the debug_query_derivation barrier return well
    // before 10s. If the timeout wrapper is removed, this hits the
    // 10s bound → timeout Err → test fail.
    let info = tokio::time::timeout(
        Duration::from_secs(10),
        f.actor.debug_query_derivation("ca-slow"),
    )
    .await
    .expect("actor blocked past 10s — grpc_timeout wrapper removed?")?
    .expect("derivation exists");

    // Completion PROCEEDED despite content_lookup never returning.
    // Timeout → matched=false → ca_output_unchanged=false → no
    // cascade, but the state DID advance to Completed.
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "completion must proceed past a hung content_lookup"
    );
    assert!(
        !info.ca_output_unchanged,
        "timed-out lookup → counted as miss → ca_output_unchanged=false"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// Zero-outputs edge: `built_outputs=[]` → `all_matched` starts
/// `false` (the `!result.built_outputs.is_empty()` at :397) →
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
    complete_ca(&f.actor, &f.worker_id, &f.drv_path, &[]).await?;

    let info = f
        .actor
        .debug_query_derivation("ca-zero")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "zero outputs → all_matched starts false (!is_empty() guard at :397); \
         NEVER flip true on no data"
    );
    // Loop body doesn't execute → no counter increment either way.
    assert_eq!(recorder.get(match_key), match_before, "no match recorded");
    assert_eq!(recorder.get(miss_key), miss_before, "no miss recorded");
    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// Malformed-hash edge: `output_hash.len() != 32` → debug +
/// `all_matched=false` + `continue` (no ContentLookup). Proves the
/// 32-byte guard at :399 short-circuits before the RPC — a worker
/// sending a truncated/wrong-length hash (parser bug, proto change)
/// doesn't get silently dropped by the RPC's INVALID_ARGUMENT; it's
/// counted as a miss explicitly.
#[tokio::test]
async fn ca_compare_malformed_hash_counts_as_miss() -> TestResult {
    let f = setup_ca_fixture("ca-malform").await?;

    // 16 bytes — not 32. Malformed.
    complete_ca(
        &f.actor,
        &f.worker_id,
        &f.drv_path,
        &[("out", &test_store_path("ca-malform-out"), vec![0xCD; 16])],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-malform")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "malformed hash → all_matched=false (the len!=32 guard), no RPC fired"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// RPC-error edge: `content_lookup` returns `Err(Unavailable)` →
/// the `Ok(Err(e))` arm at :429 → debug + `matched=false`. Proves a
/// transient store outage degrades to "no cutoff" (safe — downstream
/// rebuilds) rather than blocking or crashing.
#[tokio::test]
async fn ca_compare_rpc_err_counts_as_miss() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let f = setup_ca_fixture("ca-err").await?;
    // Arm the RPC-err flag. The CA-compare content_lookup fires at
    // ProcessCompletion time (not merge time) so arming after the
    // fixture returns still takes effect.
    f.store
        .fail_content_lookup
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let miss_before = recorder.get(miss_key);

    // Two outputs → two content_lookup calls → two Err(Unavailable)
    // → two miss increments (if P0393's short-circuit hasn't
    // landed; with short-circuit, 1 miss then break). Either way
    // ca_output_unchanged stays false.
    complete_ca(
        &f.actor,
        &f.worker_id,
        &f.drv_path,
        &[
            ("out", &test_store_path("ca-err-out1"), vec![0x01; 32]),
            ("dev", &test_store_path("ca-err-out2"), vec![0x02; 32]),
        ],
    )
    .await?;

    let info = f
        .actor
        .debug_query_derivation("ca-err")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "RPC Err → matched=false → ca_output_unchanged stays false \
         (store outage degrades to no-cutoff, not crash)"
    );
    // ≥1 miss recorded (2 without short-circuit, 1 with). The exact
    // count depends on P0393; either proves the Err arm fires the
    // counter and doesn't crash.
    assert!(
        recorder.get(miss_key) > miss_before,
        "RPC Err → at least one miss counter increment"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-propagate]
/// `verify_cutoff_candidates` wiring: the `|h| verified.contains(h)`
/// closure at `completion.rs:476` is LOAD-BEARING. A mutant that
/// replaces it with `|_| true` (skip every candidate unconditionally)
/// would let never-built nodes go `Skipped` on first-build.
///
/// Setup: A(CA)→B→C chain. Seed MockStore with B's expected output
/// ONLY (not C's). Complete A with `ca_output_unchanged=true` (seed
/// a matching prior content-index entry for A's output).
///
/// Expected:
///   - B goes Skipped (verify: B's output present in store → verified).
///   - C does NOT go Skipped (verify: C's output absent → rejected).
///   - [P0399] C goes Ready instead (all_deps_completed accepts Skipped).
///
/// Mutation check: `|_| true` → C goes Skipped too → the `assert_ne`
/// on C fails.
#[tokio::test]
async fn cascade_only_skips_verified_candidates() -> TestResult {
    // Multi-node chain — setup_ca_fixture merges a single node, so
    // this test keeps the bare setup + custom merge. Still uses
    // complete_ca for the ProcessCompletion.
    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));
    let _db = test_db;

    // Chain A→B→C. Each node has expected_output_paths so
    // verify_cutoff_candidates has something to check.
    let b_out = test_store_path("verify-b-out");
    let c_out = test_store_path("verify-c-out");
    let mut node_a = make_test_node("verify-a", "x86_64-linux");
    node_a.is_content_addressed = true;
    let mut node_b = make_test_node("verify-b", "x86_64-linux");
    node_b.expected_output_paths = vec![b_out.clone()];
    let mut node_c = make_test_node("verify-c", "x86_64-linux");
    node_c.expected_output_paths = vec![c_out.clone()];

    // A's content-index: seed a prior-build entry at a DIFFERENT
    // path so ContentLookup(A_hash, exclude=A_new) → match.
    // Seeded BEFORE merge is fine — A has no expected_output_paths
    // so cache-check doesn't short-circuit it.
    let a_new_out = test_store_path("verify-a-new");
    let a_prior_out = test_store_path("verify-a-prior");
    let (_nar_prior, a_hash) = store.seed_with_content(&a_prior_out, b"ca-stable-content");
    store.seed_with_content(&a_new_out, b"ca-stable-content");

    let _rx = connect_worker(&handle, "verify-worker", "x86_64-linux", 4).await?;

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

    // Preconditions: B and C Queued (their outputs aren't in the
    // store yet, so merge-time cache-check doesn't hit).
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

    // NOW seed B's output — AFTER merge (so cache-check didn't see
    // it) but BEFORE A completes (so verify_cutoff_candidates WILL
    // see it). C's output stays absent → verify rejects C.
    store.seed_with_content(&b_out, b"b-content");

    // Complete A with ca_output_unchanged=true (matches prior).
    complete_ca(
        &handle,
        "verify-worker",
        &test_drv_path("verify-a"),
        &[("out", &a_new_out, a_hash.to_vec())],
    )
    .await?;
    barrier(&handle).await;

    // A: Completed + ca_output_unchanged=true (matched prior).
    let info_a = handle
        .debug_query_derivation("verify-a")
        .await?
        .expect("verify-a exists");
    assert!(
        info_a.ca_output_unchanged,
        "precondition: A matched prior → ca_output_unchanged=true"
    );

    // B: Skipped (output present in store → verified).
    let info_b = handle
        .debug_query_derivation("verify-b")
        .await?
        .expect("verify-b exists");
    assert_eq!(
        info_b.status,
        DerivationStatus::Skipped,
        "B's output present → verified → Skipped via cascade"
    );

    // C: NOT Skipped (output absent → verify rejects). The mutation
    // `|_| true` would make this Skipped — the test catches it.
    let info_c = handle
        .debug_query_derivation("verify-c")
        .await?
        .expect("verify-c exists");
    assert_ne!(
        info_c.status,
        DerivationStatus::Skipped,
        "C's output absent → verify rejects → NOT Skipped \
         (mutation |_| true would fail here — this is the load-bearing assert)"
    );
    // P0399: C goes Ready (B's Skipped satisfies all_deps_completed).
    // The assert_ne above is the MUTATION-KILL; this is the
    // behavioral follow-through.
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

/// Second-build positive case: a PRIOR build's content_index row
/// (`(H, P_prior)`) exists ALONGSIDE this build's own upload
/// (`(H, P_new)`). `ContentLookup(H, exclude=P_new)` finds `P_prior`
/// → match → `ca_output_unchanged = true`.
///
/// Paired with `ca_compare_first_build_excludes_own_upload` this
/// proves "hide self, find sibling" — the exclusion isn't "any match
/// → None" (over-correction).
#[tokio::test]
async fn ca_compare_second_build_matches_prior() -> TestResult {
    let f = setup_ca_fixture("ca-second").await?;

    // TWO rows for the same hash: P_prior (a previous build) and
    // P_new (THIS build's own upload). Same bytes → same nar_hash.
    // Seeding after the fixture merge is fine — the CA-compare
    // content_lookup fires at ProcessCompletion, not merge.
    let p_prior = test_store_path("ca-sibling-prior");
    let p_new = test_store_path("ca-sibling-new");
    let (_n1, hash) = f.store.seed_with_content(&p_prior, b"identical-bytes");
    let (_n2, hash2) = f.store.seed_with_content(&p_new, b"identical-bytes");
    assert_eq!(hash, hash2, "same content → same nar_hash");

    // Report P_new as the just-built path. Exclude hides P_new;
    // P_prior remains findable → match.
    complete_ca(
        &f.actor,
        &f.worker_id,
        &f.drv_path,
        &[("out", &p_new, hash.to_vec())],
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
        "SECOND build: ContentLookup(H, exclude=P_new) → finds P_prior → \
         ca_output_unchanged=true (intended positive: downstream CAN skip)"
    );

    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// Short-circuit on first miss: a 4-output CA completion where
/// output[0] misses should call `content_lookup` exactly ONCE, not
/// four times. The AND-fold result is already `false` after the first
/// miss; the remaining lookups can't flip it. The `break` at the
/// short-circuit saves up to (N-1)×CONTENT_LOOKUP_TIMEOUT worst case.
///
/// Mutation check (plan exit criterion): remove the `break;` in the
/// `if !matched { ... }` block → `content_lookup_calls == 4` instead
/// of `1` → test fails on the call-count assert.
///
/// Uses `MockStore.content_lookup_calls` as the direct proof — the
/// counter-delta (`1×miss + 3×skipped_after_miss`) is the secondary
/// assert that proves the metric labelling is wired.
#[tokio::test]
async fn ca_compare_short_circuits_on_first_miss() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));
    let _db = test_db;

    let _rx = connect_worker(&handle, "sc-worker", "x86_64-linux", 4).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-shortcircuit");
    let mut node = make_test_node("ca-shortcircuit", "x86_64-linux");
    node.is_content_addressed = true;
    node.output_names = vec!["out".into(), "dev".into(), "doc".into(), "man".into()];
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let skip_key = "rio_scheduler_ca_hash_compares_total{outcome=skipped_after_miss}";
    let miss_before = recorder.get(miss_key);
    let skip_before = recorder.get(skip_key);
    let calls_before = store.content_lookup_calls.load(Ordering::SeqCst);

    // Four outputs. None of these hashes are seeded in MockStore →
    // output[0] → miss → break. outputs[1..3] would ALSO miss but
    // should never be queried.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "sc-worker".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![
                    rio_proto::types::BuiltOutput {
                        output_name: "out".into(),
                        output_path: test_store_path("ca-sc-out"),
                        output_hash: vec![0x10; 32],
                    },
                    rio_proto::types::BuiltOutput {
                        output_name: "dev".into(),
                        output_path: test_store_path("ca-sc-dev"),
                        output_hash: vec![0x11; 32],
                    },
                    rio_proto::types::BuiltOutput {
                        output_name: "doc".into(),
                        output_path: test_store_path("ca-sc-doc"),
                        output_hash: vec![0x12; 32],
                    },
                    rio_proto::types::BuiltOutput {
                        output_name: "man".into(),
                        output_path: test_store_path("ca-sc-man"),
                        output_hash: vec![0x13; 32],
                    },
                ],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;

    // Barrier: send_unchecked returns once the actor has PROCESSED
    // the completion (debug_query_derivation serializes behind it).
    let info = handle
        .debug_query_derivation("ca-shortcircuit")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "first-output miss → AND-fold false"
    );

    // LOAD-BEARING ASSERT: exactly 1 call, not 4. If the break is
    // removed, this becomes 4.
    let calls_after = store.content_lookup_calls.load(Ordering::SeqCst);
    assert_eq!(
        calls_after - calls_before,
        1,
        "short-circuit: 4 outputs, first miss → ONE content_lookup call. \
         Got {} (4 = break removed / short-circuit defeated)",
        calls_after - calls_before
    );

    // Metric labelling: 1 miss + 3 skipped_after_miss.
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

// r[verify sched.ca.cutoff-compare]
/// Breaker-open gate: if the cache-check circuit breaker is open
/// BEFORE the CA completion fires, the compare loop is skipped
/// entirely. Zero `content_lookup` calls, zero match/miss
/// increments, `ca_output_unchanged` stays `false` (safe fallback).
///
/// Uses `DebugTripBreaker{n=5}` to open the breaker directly — no
/// need for the N-failing-SubmitBuild dance. The breaker's
/// `OPEN_THRESHOLD` is 5; the handler returns `is_open()` so the
/// precondition is explicit.
#[tokio::test]
async fn ca_compare_skipped_when_breaker_open() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));
    let _db = test_db;

    // Trip the breaker open (OPEN_THRESHOLD = 5 per breaker.rs).
    let opened = handle.debug_trip_breaker(5).await?;
    assert!(
        opened,
        "precondition: 5 record_failure() calls open the breaker"
    );

    let _rx = connect_worker(&handle, "bo-worker", "x86_64-linux", 4).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-breaker-open");
    let mut node = make_test_node("ca-breaker-open", "x86_64-linux");
    node.is_content_addressed = true;
    node.output_names = vec!["out".into(), "dev".into(), "doc".into()];
    // No expected_output_paths — so merge-time check_cached_outputs
    // skips the store call (check_paths empty → early return Ok).
    // This keeps the breaker open through the merge; the CA-compare
    // is_open() gate is what we're testing, not merge-time rejection.
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";
    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";
    let match_before = recorder.get(match_key);
    let miss_before = recorder.get(miss_key);
    let calls_before = store.content_lookup_calls.load(Ordering::SeqCst);

    // Three outputs with valid 32-byte hashes. If the breaker gate
    // DIDN'T exist, the loop would run → ≥1 content_lookup call.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "bo-worker".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![
                    rio_proto::types::BuiltOutput {
                        output_name: "out".into(),
                        output_path: test_store_path("ca-bo-out"),
                        output_hash: vec![0x20; 32],
                    },
                    rio_proto::types::BuiltOutput {
                        output_name: "dev".into(),
                        output_path: test_store_path("ca-bo-dev"),
                        output_hash: vec![0x21; 32],
                    },
                    rio_proto::types::BuiltOutput {
                        output_name: "doc".into(),
                        output_path: test_store_path("ca-bo-doc"),
                        output_hash: vec![0x22; 32],
                    },
                ],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;

    let info = handle
        .debug_query_derivation("ca-breaker-open")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);

    // LOAD-BEARING: zero ContentLookup calls. Breaker-open →
    // is_open() gate fires before the loop.
    let calls_after = store.content_lookup_calls.load(Ordering::SeqCst);
    assert_eq!(
        calls_after, calls_before,
        "breaker open → compare loop skipped → ZERO content_lookup calls"
    );

    // Safe fallback: ca_output_unchanged stays false (downstream
    // runs normally, no false-positive cutoff).
    assert!(
        !info.ca_output_unchanged,
        "breaker open → no compare → ca_output_unchanged stays false (safe fallback)"
    );

    // Loop never ran → no match/miss counter increments.
    assert_eq!(recorder.get(match_key), match_before, "no match recorded");
    assert_eq!(recorder.get(miss_key), miss_before, "no miss recorded");

    Ok(())
}

// r[verify sched.ca.cutoff-compare]
/// Breaker feedback: ContentLookup failures feed the SAME circuit
/// breaker FindMissingPaths feeds. Five consecutive failing CA
/// completions (MockStore `fail_content_lookup=true`) → 5×
/// `record_failure()` → breaker trips open. The open-transition
/// metric (`rio_scheduler_cache_check_circuit_open_total`) increments
/// once on the trip.
///
/// Each completion is 1-output so the short-circuit doesn't reduce
/// the per-completion failure count below 1 (the plan's threshold is
/// 5 = `OPEN_THRESHOLD`; a 5-output single completion would trip on
/// the FIRST iteration then short-circuit — that's a different test).
///
/// Post-trip probe: `debug_trip_breaker(0)` calls `record_failure` 0
/// times and returns `is_open()` — a clean read of the breaker state
/// without mutating it.
#[tokio::test]
async fn ca_compare_failures_feed_breaker() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    store.fail_content_lookup.store(true, Ordering::SeqCst);
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));
    let _db = test_db;

    let _rx = connect_worker(&handle, "fb-worker", "x86_64-linux", 8).await?;

    let open_key = "rio_scheduler_cache_check_circuit_open_total{}";
    let open_before = recorder.get(open_key);

    // Precondition: breaker closed (0 record_failure calls, read-only probe).
    assert!(
        !handle.debug_trip_breaker(0).await?,
        "precondition: breaker starts closed"
    );

    // Five completions × 1-output each. Each ContentLookup → Err →
    // record_failure(). The fifth trips the breaker.
    // No expected_output_paths on the nodes → merge-time cache-check
    // skips the store (check_paths empty) so the breaker isn't
    // perturbed by FindMissingPaths before CA-compare runs.
    for i in 0..5 {
        let tag = format!("ca-feed-{i}");
        let build_id = Uuid::new_v4();
        let drv_path = test_drv_path(&tag);
        let mut node = make_test_node(&tag, "x86_64-linux");
        node.is_content_addressed = true;
        let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

        handle
            .send_unchecked(ActorCommand::ProcessCompletion {
                worker_id: "fb-worker".into(),
                drv_key: drv_path,
                result: rio_proto::build_types::BuildResult {
                    status: rio_proto::build_types::BuildResultStatus::Built.into(),
                    built_outputs: vec![rio_proto::types::BuiltOutput {
                        output_name: "out".into(),
                        output_path: test_store_path(&format!("ca-feed-{i}-out")),
                        output_hash: vec![0x30 + i as u8; 32],
                    }],
                    ..Default::default()
                },
                peak_memory_bytes: 0,
                output_size_bytes: 0,
                peak_cpu_cores: 0.0,
            })
            .await?;
    }

    // Barrier: ensure all five completions processed. Querying the
    // last derivation serializes behind all prior ProcessCompletion.
    let _ = handle
        .debug_query_derivation("ca-feed-4")
        .await?
        .expect("last CA derivation exists");

    // LOAD-BEARING: breaker is now open. ContentLookup failures fed
    // it the same way FindMissingPaths failures would.
    assert!(
        handle.debug_trip_breaker(0).await?,
        "5 ContentLookup failures → breaker open (OPEN_THRESHOLD=5)"
    );

    // Open-transition metric fired once (at the threshold crossing,
    // not per-failure).
    assert_eq!(
        recorder.get(open_key) - open_before,
        1,
        "breaker tripped once → circuit_open_total +1.\nCounters: {:#?}",
        recorder.all_keys()
    );

    // Followup confirmation: a SIXTH CA completion with the breaker
    // open → is_open() gate fires → zero ContentLookup calls for it.
    let calls_before_6th = store.content_lookup_calls.load(Ordering::SeqCst);
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("ca-feed-gated", "x86_64-linux");
    node.is_content_addressed = true;
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "fb-worker".into(),
            drv_key: test_drv_path("ca-feed-gated"),
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("ca-feed-gated-out"),
                    output_hash: vec![0x40; 32],
                }],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    let _ = handle
        .debug_query_derivation("ca-feed-gated")
        .await?
        .expect("exists");
    assert_eq!(
        store.content_lookup_calls.load(Ordering::SeqCst),
        calls_before_6th,
        "6th completion: breaker still open → is_open() gate → zero new calls"
    );

    Ok(())
}

/// TransientFailure: retry on a different worker up to max_retries (default 2).
#[tokio::test]
async fn test_transient_retry_different_worker() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Register two workers
    let _rx1 = connect_worker(&handle, "worker-a", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "worker-b", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let p_retry = test_drv_path("retry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "retry-hash", PriorityClass::Scheduled).await?;

    // Get initial worker assignment
    let info1 = handle
        .debug_query_derivation("retry-hash")
        .await?
        .expect("exists");
    let first_worker = info1.assigned_worker.clone().expect("assigned to a worker");
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
    // failed_workers contains first_worker so when backoff elapses,
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
    // the test, the worker must be different (failed_workers
    // exclusion).
    match info2.status {
        DerivationStatus::Ready => {
            // Backoff active: assigned_worker cleared.
            assert!(
                info2.assigned_worker.is_none(),
                "Ready after failure → assigned_worker cleared"
            );
        }
        DerivationStatus::Assigned => {
            // Raced: dispatch happened. Worker MUST be different
            // (failed_workers exclusion).
            let retry_worker = info2.assigned_worker.expect("Assigned → worker set");
            assert_ne!(
                retry_worker, first_worker,
                "failed_workers exclusion: retry must go to a DIFFERENT worker"
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
/// and failed_workers exclusion (which correctly prevent immediate
/// same-worker retry). The test drives the state machine directly to
/// test the completion handler's max_retries logic, not dispatch.
#[tokio::test]
async fn test_transient_failure_max_retries_poisons() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("flaky-worker", "x86_64-linux", 1).await?;

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
            // dispatch, and failed_workers now excludes flaky-worker.
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

/// Derivation poisoned after POISON_THRESHOLD (3) distinct worker failures.
#[tokio::test]
async fn test_poison_threshold_after_distinct_workers() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Register 4 workers so the derivation can be re-dispatched after each failure.
    let _rx1 = connect_worker(&handle, "poison-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "poison-w2", "x86_64-linux", 1).await?;
    let _rx3 = connect_worker(&handle, "poison-w3", "x86_64-linux", 1).await?;
    let _rx4 = connect_worker(&handle, "poison-w4", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "poison-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Send TransientFailure from 3 DISTINCT workers. After the 3rd, poison.
    //
    // debug_force_assign between failures: backoff_until prevents
    // immediate re-dispatch after each failure. The test drives
    // the assignment directly — we're testing the poison-threshold
    // logic (3 distinct failed_workers → poisoned), not dispatch
    // timing.
    for (i, worker) in ["poison-w1", "poison-w2", "poison-w3"].iter().enumerate() {
        if i > 0 {
            // After the first failure, dispatch won't re-assign
            // until backoff elapses. Force it. The worker is
            // distinct each time so failed_workers exclusion
            // wouldn't block (if backoff weren't in play).
            let ok = handle.debug_force_assign(drv_hash, worker).await?;
            assert!(ok, "force-assign should succeed");
        }
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

// r[verify sched.retry.per-worker-budget]
/// InfrastructureFailure is a worker-local problem (FUSE EIO, cgroup
/// setup fail, OOM-kill of the build process) — NOT the build's fault.
/// 3× InfrastructureFailure on distinct workers → failed_workers stays
/// EMPTY, derivation NOT poisoned. Contrast with the TransientFailure
/// test above, where 3 distinct failures → poison.
#[tokio::test]
async fn test_infrastructure_failure_does_not_count_toward_poison() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // 4 workers so re-dispatch always has a candidate.
    let _rx1 = connect_worker(&handle, "infra-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "infra-w2", "x86_64-linux", 1).await?;
    let _rx3 = connect_worker(&handle, "infra-w3", "x86_64-linux", 1).await?;
    let _rx4 = connect_worker(&handle, "infra-w4", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× InfrastructureFailure from distinct workers. In the
    // TransientFailure case this would poison; here it must not.
    // handle_infrastructure_failure does reset_to_ready WITHOUT
    // failed_workers insert and WITHOUT backoff — so immediate
    // re-dispatch to whatever worker wins. We drive via
    // debug_force_assign to make the per-worker assertion
    // deterministic (avoid racing dispatch).
    for (i, worker) in ["infra-w1", "infra-w2", "infra-w3"].iter().enumerate() {
        if i > 0 {
            let ok = handle.debug_force_assign(drv_hash, worker).await?;
            assert!(ok, "force-assign should succeed (no backoff, no exclusion)");
        }
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
    // Exit criterion: 3× InfrastructureFailure → failed_workers.is_empty()
    assert!(
        info.failed_workers.is_empty(),
        "InfrastructureFailure must NOT insert into failed_workers, got {:?}",
        info.failed_workers
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
        info.failed_workers.len(),
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

    let _rx = connect_worker(&handle, "solo-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "nondistinct-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× TransientFailure on the SAME worker. In default distinct
    // mode: failed_workers={solo-worker} stays at len()=1 < 3 → not
    // poisoned (blocked by max_retries instead). In non-distinct mode:
    // failure_count goes 1→2→3, and 3 >= threshold → poisoned.
    for i in 0..3 {
        if i > 0 {
            // Backoff + failed_workers exclusion block real dispatch
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
        // failed_workers (HashSet) stays at 1 — same worker every time.
        assert_eq!(
            info.failed_workers.len(),
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
/// does NOT poison via the threshold path — failed_workers.len()
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

    let _rx = connect_worker(&handle, "ctrl-worker", "x86_64-linux", 1).await?;

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
    assert_eq!(info.failed_workers.len(), 1, "HashSet: same worker = 1");
    assert_eq!(info.failure_count, 3, "flat count still increments");
    // NOT poisoned: distinct mode uses failed_workers.len()=1 < 3.
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "distinct mode (default): 3× same-worker → NOT poisoned via threshold \
         (failed_workers.len()=1 < 3); would need 3 DISTINCT workers"
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

    // Complete B.
    complete_success_empty(&handle, "chain-worker", &p_chain_b).await?;

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

    // Worker should receive A's assignment.
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
            worker_id: "ghost-worker".into(),
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
            worker_id: "unk-w".into(),
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
            worker_id: "cancel-w".into(),
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

    // The state check at handle_completion's top catches this —
    // drv is already Cancelled (not Assigned/Running), so it hits
    // the "not in assigned/running state, ignoring" early-return.
    // This IS the no-op path for acknowledging a cancel.
    assert!(
        logs_contain("not in assigned/running state"),
        "expected early-return for already-Cancelled state"
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
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "w-small".into(),
            stream_tx: tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            resources: None,
            bloom: None,
            size_class: Some("small".into()),
            worker_id: "w-small".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 4,
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
            worker_id: "w-small".into(),
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
            worker_id: "bs-worker".into(),
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
