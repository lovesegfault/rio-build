//! Dispatch: skip-ineligible, options propagation, interactive priority.

use super::*;

/// Per-build BuildOptions (max_silent_time, build_timeout) must propagate
/// to the worker via WorkAssignment. Regression guard: without
/// propagation, all-zeros defaults would be sent.
#[tokio::test]
async fn test_build_options_propagated_to_worker() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Submit with build_timeout=300, max_silent_time=60.
    let build_id = Uuid::new_v4();
    let _rx = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("opts-hash")],
            edges: vec![],
            options: BuildOptions {
                max_silent_time: 60,
                build_timeout: 300,
                build_cores: 4,
            },
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    // The pull payload carries the build's options.
    let assignment = pull_attempt(&handle, "opts-hash").await;
    let opts = assignment.build_options.expect("options should be set");
    assert_eq!(
        opts.build_timeout, 300,
        "build_timeout should propagate from build to worker"
    );
    assert_eq!(
        opts.max_silent_time, 60,
        "max_silent_time should propagate from build to worker"
    );
    assert_eq!(opts.build_cores, 4);
    Ok(())
}

/// The submitter's W3C traceparent is carried through `MergeDagRequest` →
/// stored on `DerivationState` → embedded in `WorkAssignment.traceparent`
/// at dispatch. This gives gateway→scheduler→worker trace continuity
/// despite the span context NOT crossing the mpsc channel to the actor.
///
/// Regression: before this fix, `dispatch.rs` called
/// `current_traceparent()` which read the actor's ORPHAN span (a fresh
/// root), so the worker's span belonged to a disjoint trace.
// r[verify obs.trace.w3c-traceparent]
// r[verify sched.trace.assignment-traceparent]
#[tokio::test]
async fn test_dispatch_carries_submitter_traceparent() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Known traceparent (W3C format: version-trace_id-span_id-flags).
    // The exact bytes don't matter for this test — we just verify it
    // flows through unchanged.
    let known_tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    let build_id = Uuid::new_v4();
    let _rx = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("trace-hash")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: known_tp.to_string(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    let assignment = pull_attempt(&handle, "trace-hash").await;
    assert_eq!(
        assignment.traceparent, known_tp,
        "WorkAssignment.traceparent must match the submitter's traceparent, \
         not the actor's orphan span"
    );
    Ok(())
}

/// Dedup: if a second submitter merges an already-present derivation,
/// the FIRST submitter's traceparent is preserved on the state. The
/// worker's span should chain back to whichever build first introduced
/// the derivation (operationally: the trace that will have waited longest).
#[tokio::test]
async fn test_dispatch_traceparent_first_submitter_wins_on_dedup() -> TestResult {
    let (db, handle, _task) = setup().await;

    let tp_first = "00-11111111111111111111111111111111-1111111111111111-01";
    let tp_second = "00-22222222222222222222222222222222-2222222222222222-01";

    // Helper: merge dedup-hash with a given traceparent (defaults otherwise).
    let merge_with_tp = |tp: &str| MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: None,
        priority_class: PriorityClass::Scheduled,
        nodes: vec![make_node("dedup-hash")],
        edges: vec![],
        options: BuildOptions::default(),
        keep_going: false,
        traceparent: tp.to_string(),
        jti: None,
        jwt_token: None,
    };

    // First submit with tp_first.
    let _ = merge_dag_req(&handle, merge_with_tp(tp_first)).await?;
    // Second submit: SAME derivation, DIFFERENT traceparent (dedup hit).
    let _ = merge_dag_req(&handle, merge_with_tp(tp_second)).await?;

    // Pull the deduped node: the payload carries the stored traceparent.
    let assignment = pull_attempt(&handle, "dedup-hash").await;
    assert_eq!(
        assignment.traceparent, tp_first,
        "first submitter's traceparent should win on dedup (existing state not overwritten)"
    );
    drop(db);
    Ok(())
}

/// Dedup upgrade: if the existing node has traceparent="" (from
/// recovery or poison-reset), a live submitter's traceparent REPLACES
/// it. Recovery isn't a "submitter" — without this, a user's
/// STDERR_NEXT trace_id after failover never finds the worker span.
#[tokio::test]
async fn test_dedup_upgrades_empty_traceparent_from_recovery() -> TestResult {
    let (db, handle, _task) = setup().await;

    let merge_with_tp = |tp: &str| MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: None,
        priority_class: PriorityClass::Scheduled,
        nodes: vec![make_node("upgrade-hash")],
        edges: vec![],
        options: BuildOptions::default(),
        keep_going: false,
        traceparent: tp.to_string(),
        jti: None,
        jwt_token: None,
    };

    // First merge with EMPTY traceparent (simulates recovery:
    // from_recovery_row/from_poisoned_row set traceparent="").
    let _ = merge_dag_req(&handle, merge_with_tp("")).await?;

    // Second merge with a REAL traceparent — dedup hit, should upgrade.
    let live_tp = "00-33333333333333333333333333333333-3333333333333333-01";
    let _ = merge_dag_req(&handle, merge_with_tp(live_tp)).await?;

    let assignment = pull_attempt(&handle, "upgrade-hash").await;
    assert_eq!(
        assignment.traceparent, live_tp,
        "empty traceparent (recovery) should be upgraded by first live submitter"
    );
    drop(db);
    Ok(())
}

// -----------------------------------------------------------------------------
// C1: Leader generation — Arc<AtomicU64>, single-load consistency
// -----------------------------------------------------------------------------

/// The generation starts at 1, not 0. Proto-default is 0; a worker
/// receiving `generation=0` should interpret it as "field unset (old
/// scheduler version)" not "first generation."
///
/// Catches the off-by-one if someone changes `AtomicU64::new(1)` → `new(0)`
/// during a refactor. Without this, the bug would only surface as workers
/// treating EVERY first-leadership assignment as unset/stale.
#[tokio::test]
async fn test_generation_starts_at_one_not_zero() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Not dispatching anything — just reading the reader directly.
    // leader_generation() is the raw (not recovery-gated) value; the
    // heartbeat carries advertised_generation(), which equals it here
    // because the always-leader fixture constructs
    // recovery_complete = true.
    assert_eq!(
        handle.leader_generation(),
        1,
        "gen=0 is proto-default (unset); gen=1 is the real first generation"
    );
    assert_eq!(
        handle.advertised_generation(),
        1,
        "always-leader fixture has recovery complete, so the heartbeat advertises the real \
         first generation, not the 0 unset sentinel"
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// PrefetchHint before WorkAssignment
// -----------------------------------------------------------------------------

/// Dispatch pins input-closure paths; terminal unpins.
/// Verifies the end-to-end pin → unpin lifecycle via scheduler_
/// live_pins row count.
// r[verify sched.gc.live-pins]
#[tokio::test]
async fn test_pin_unpin_live_inputs_lifecycle() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Two-node chain: child (leaf, no inputs) + parent (depends
    // on child). Parent's approx_input_closure = child's
    // expected_output_paths. The pull mint of PARENT should pin those.
    //
    // make_test_node defaults expected_output_paths=vec![]; set
    // explicitly so approx_input_closure has something to collect.
    let build_id = Uuid::new_v4();
    let child_out = test_store_path("x9-child-out");
    let mut child = make_node("x9-child");
    child.expected_output_paths = vec![child_out.clone()];
    let parent = make_node("x9-parent");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![child, parent],
        vec![make_test_edge("x9-parent", "x9-child")],
        false,
    )
    .await?;

    // Child is pullable first (leaf → Ready immediately).
    let assignment_child = pull_attempt(&handle, "x9-child").await;
    assert!(assignment_child.drv_path.contains("x9-child"));

    // Child is leaf → approx_input_closure empty → no pin.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-child'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 0, "leaf drv (no inputs) should not pin anything");

    // Complete child → parent becomes Ready → its pull mint pins.
    pull_complete_success_empty(&handle, "x9-child").await?;
    let assignment_parent = pull_attempt(&handle, "x9-parent").await;
    assert!(assignment_parent.drv_path.contains("x9-parent"));
    barrier(&handle).await;

    // Parent's input-closure = child's expected_output_paths
    // (1 path via make_test_node). Pin should be present.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 1, "parent pull mint should pin its 1 input path");

    // Complete parent → unpin.
    pull_complete_success_empty(&handle, "x9-parent").await?;
    barrier(&handle).await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 0, "completion should unpin");

    Ok(())
}

// -----------------------------------------------------------------------------
// CA recovery-resolve: fetch ATerm from store when drv_content empty
// -----------------------------------------------------------------------------

/// Build a CA-on-CA fixture: (child_node, parent_node, parent_aterm,
/// placeholder, child_modular_hash, realized_path).
///
/// Parent is floating-CA with one inputDrv = child. Child is
/// floating-CA with `ca_modular_hash` set (so `collect_ca_inputs`
/// picks it up). The placeholder is what `resolve_ca_inputs` will
/// replace with `realized_path` once the child's realisation is in PG.
fn ca_on_ca_fixture() -> (
    rio_proto::types::DerivationNode,
    rio_proto::types::DerivationNode,
    String,
    String,
    [u8; 32],
    String,
) {
    use crate::ca::resolve::downstream_placeholder;
    use rio_nix::store_path::StorePath;

    let child_path = test_drv_path("ca-child");
    let child_modular: [u8; 32] = [0xCA; 32];
    let realized_path = test_store_path("ca-child-realized-out");

    let placeholder = downstream_placeholder(&StorePath::parse(&child_path).unwrap(), "out");

    // Parent's ATerm: floating-CA output ("sha256" algo, empty hash,
    // empty path), one inputDrv = child, placeholder in env.DEP.
    let parent_aterm = format!(
        r#"Derive([("out","","sha256","")],[("{child_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","build"],[("DEP","{placeholder}"),("out",""),("system","x86_64-linux")])"#
    );

    let mut child = make_node("ca-child");
    child.is_content_addressed = true;
    child.needs_resolve = true;
    child.ca_modular_hash = child_modular.to_vec();
    // expected_output_paths can stay empty — parent's PrefetchHint
    // will be empty and skipped (leaf child → no hint anyway).

    let mut parent = make_node("ca-parent");
    parent.is_content_addressed = true;
    parent.needs_resolve = true;
    parent.drv_content = parent_aterm.clone().into_bytes();

    (
        child,
        parent,
        parent_aterm,
        placeholder,
        child_modular,
        realized_path,
    )
}

// -----------------------------------------------------------------------------
// maybe_resolve_ca gate-path passthrough coverage
// -----------------------------------------------------------------------------

// r[verify sched.ca.resolve+3]
/// IA passthrough: `state.ca.needs_resolve = false` → gate at
/// dispatch.rs:681 fails → `drv_content` returned unchanged. No
/// resolve fires, no ContentLookup, no PG query. The cheapest path —
/// every IA-with-IA-inputs dispatch takes it.
#[tokio::test]
async fn maybe_resolve_ca_ia_derivation_passthrough() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let original_content = b"dummy-ia-aterm-content".to_vec();
    let mut node = make_node("ia-drv");
    node.is_content_addressed = false; // explicit: IA
    node.needs_resolve = false; // explicit: no CA inputs either
    node.drv_content = original_content.clone();

    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let asgn = pull_attempt(&handle, "ia-drv").await;

    assert_eq!(
        asgn.drv_content, original_content,
        "IA derivation → maybe_resolve_ca passthrough; drv_content unchanged"
    );
    Ok(())
}

// D3-retarget (flipped with the walk spawner's deletion): dispatch-time
// classification survives; the routed mechanism is now a materialization
// job — the store replica executes the fetch and the consumption path
// completes the node.
// r[verify sched.dispatch.fod-substitute+3]
/// Dispatch-time substitution routing: a Ready IA derivation (FOD or
/// non-FOD) whose output becomes substitutable AFTER merge (so
/// merge-time `check_cached_outputs` missed it) is routed to a
/// materialization job (origin=cache_opportunity) by
/// `batch_probe_cached_ready` — never dispatched to a builder, never
/// walked scheduler-side.
///
/// Pre-fix lineage: the batch was FOD-only AND read only
/// `missing_paths` (no service-token, ignored `substitutable_paths`)
/// → non-FODs relied on merge-time `check_available` which truncates
/// at 4096 → an 18k-drv build's IA cache-hits dispatched to builders.
#[rstest::rstest]
#[case::fod(true)]
#[case::non_fod(false)]
#[tokio::test]
async fn dispatch_time_substitutable_routes_to_job(#[case] is_fod: bool) -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    // merged_bug_003 (Q3): the cache-opportunity lane exists only for
    // TENANTED builds — upstreams are per-tenant (`tenant_upstreams`),
    // the dispatch probe asks per live tenant under service auth, and
    // the store reports substitutable paths only to a verified scope.
    // (The pre-Q3 mock answered an anonymous probe with substitutable
    // paths — a wire state the real store never produces; this test
    // passed against that fiction with a tenant-less build.)
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "dispatch-sub-tenant").await;
    let out = test_store_path("dispatch-sub-out");
    let mut n = make_node("dispatch-sub-drv");
    n.is_fixed_output = is_fod;
    n.system = "aarch64-linux".into();
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![n],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    // Merge-time saw nothing (substitutable not yet seeded) → node
    // stays Ready, stamped probed_generation=1. Seed; the next Tick
    // advances probe_generation and re-runs the ready-set sweep → the
    // batch probe sees it → creates the job.
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;
    barrier(&handle).await;

    // The probe partition routed the node to a job, not a walk and not
    // a builder dispatch.
    let (origin, job_state): (String, String) = sqlx::query_as(
        "SELECT origin, state FROM materialization_jobs WHERE drv_hash = 'dispatch-sub-drv'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        origin, "cache_opportunity",
        "is_fod={is_fod}: the dispatch-probe creation site's origin"
    );
    assert_eq!(job_state, "pending", "claimable by a store replica");
    let d = expect_drv(&handle, "dispatch-sub-drv").await;
    assert_eq!(
        d.status,
        DerivationStatus::Ready,
        "is_fod={is_fod}: the node stays Ready (job is the in-flight marker)"
    );
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            !qpi.contains(&out),
            "no scheduler-side walk fetch may run; qpi_calls={qpi:?}"
        );
    }
    Ok(())
}

// r[verify sched.materialize.routing+7]
/// merged_bug_028 (028a, dispatch leg): presence and substitutability
/// are PER-TENANT facts — the batch probe asks once per live tenant
/// and folds per candidate. A path visible under tenant A but
/// missing-and-unsubstitutable under tenant B (sig-visibility split)
/// must NOT inline-complete (the every-tenant conjunction fails) and
/// MUST route to a materialization job (the some-tenant existential
/// holds via A — owner Q2). RED (pre-fix): one find_map-picked tenant
/// answered for both — picking A inline-completed (laundering B's
/// visibility), picking B left the node Ready from-source; NEITHER
/// created the job.
#[tokio::test]
async fn probe_batch_partitions_by_tenant() -> TestResult {
    use rio_auth::hmac::HmacSigner;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-028a-dispatch-service-key-32".to_vec();
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(std::sync::Arc::new(HmacSigner::from_key(service_key)));
        });
    let _tasks = (store_task, actor_task);
    let tenant_a = rio_store::test_helpers::seed_tenant(&db.pool, "028a-tenant-a").await;
    let tenant_b = rio_store::test_helpers::seed_tenant(&db.pool, "028a-tenant-b").await;

    let out = test_store_path("028a-split-out");
    // Two builds, one per tenant, both interested in the same node.
    // (The path is seeded AFTER merge so the merge-time presence check
    // cannot settle the node before the dispatch batch runs.)
    let mut n = make_node("028a-split-drv");
    n.expected_output_paths = vec![out.clone()];
    n.wanted_output_names = vec!["out".into()];
    for (build_tenant, n) in [(tenant_a, n.clone()), (tenant_b, n)] {
        merge_dag_req(
            &handle,
            MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: Some(build_tenant),
                priority_class: PriorityClass::Scheduled,
                nodes: vec![n],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
                jti: None,
                jwt_token: None,
            },
        )
        .await?;
    }
    barrier(&handle).await;

    // NOW the path appears in the store (global state) but stays
    // invisible under tenant B (the per-tenant unobtainable script —
    // the mock twin of the sig-visibility gate).
    store.seed_with_content(&out, b"028a-split");
    store
        .state
        .per_tenant_unobtainable
        .write()
        .unwrap()
        .insert(tenant_b.to_string(), vec![out.clone()]);
    // Advance the probe generation so the batch re-probes the node.
    tick(&handle).await?;
    barrier(&handle).await;

    // The batch asked once per tenant (the partition's structural pin).
    {
        let tenants_probed = store.calls.find_missing_tenants.read().unwrap();
        let mut seen: Vec<&str> = tenants_probed
            .iter()
            .flatten()
            .map(|s| s.as_str())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        let ta = tenant_a.to_string();
        let tb = tenant_b.to_string();
        assert!(
            seen.contains(&ta.as_str()) && seen.contains(&tb.as_str()),
            "the probe must ask under EVERY live tenant, probed={tenants_probed:?}"
        );
    }

    // Routed to a job (some-tenant obtainable), NOT inline-completed
    // (every-tenant visibility failed under B).
    let d = expect_drv(&handle, "028a-split-drv").await;
    assert_eq!(
        d.status,
        DerivationStatus::Ready,
        "the visibility split must not inline-complete the node"
    );
    let jobs: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM materialization_jobs WHERE drv_hash = '028a-split-drv'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        jobs, 1,
        "the some-tenant existential (owner Q2) routes the node to a materialization job"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+3]
/// `batch_probe_cached_ready` × wanted outputs: a Ready node whose only
/// missing output is one nothing wants must be completed inline by the
/// dispatch-time batch probe instead of staying Ready forever / being
/// dispatched to a builder. The node is aarch64 with only an x86_64
/// worker connected so it can never dispatch — with the all-declared
/// criterion it would sit Ready until the heat death of the universe
/// because P_debug is missing and unsubstitutable.
#[tokio::test]
async fn batch_probe_completes_on_missing_unwanted_output() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("bpw-out");
    let dbg = test_store_path("bpw-debug");
    let mut n = make_node("bpw-drv");
    n.system = "aarch64-linux".into();
    n.output_names = vec!["out".into(), "debug".into()];
    n.expected_output_paths = vec![out.clone(), dbg.clone()];
    n.wanted_output_names = vec!["out".into()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "bpw-drv").await.status,
        DerivationStatus::Ready,
        "precondition: nothing in store at merge time → Ready"
    );

    // P_out appears in the store AFTER merge (another build uploaded
    // it). P_debug stays missing and unsubstitutable. The next
    // The next Tick's ready-set sweep batch-probes the Ready node.
    store.seed_with_content(&out, b"out");
    tick(&handle).await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "bpw-drv").await.status,
        DerivationStatus::Completed,
        "all WANTED outputs present → completed inline by the batch \
         probe; the missing unwanted P_debug must not keep it Ready"
    );
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Succeeded as i32);
    Ok(())
}

/// I-163 Fix 3: `cluster_snapshot_cached()` reads the watch-channel
/// value the actor publishes on `Tick` — no mailbox round-trip. The
/// `fn` (not `async fn`) signature is the structural proof; this test
/// verifies the value is wired (Tick → publish → handle reads it) and
/// is stale-until-Tick (merge alone doesn't update it).
// r[verify sched.admin.snapshot-cached]
#[tokio::test]
async fn cluster_snapshot_cached_reflects_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "i163-snap",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;

    // No Tick yet → watch holds the Default snapshot (all zeros).
    // The merged (Ready) node exists in actor state, but the cached
    // snapshot doesn't see it until Tick publishes.
    let pre = handle.cluster_snapshot_cached();
    assert_eq!(
        pre.queued_derivations, 0,
        "cached snapshot is Tick-published, not live; pre-Tick must be Default"
    );

    // Open the node's pull attempt so the post-Tick snapshot has a
    // running derivation (= one busy executor) to reflect.
    let _assignment = pull_attempt(&handle, "i163-snap").await;
    tick(&handle).await?;

    let post = handle.cluster_snapshot_cached();
    assert_eq!(
        post.total_executors, 1,
        "one open attempt = one busy executor"
    );
    assert_eq!(post.active_executors, 1);
    assert_eq!(post.draining_executors, 0);
    assert_eq!(post.running_derivations, 1);
    assert_eq!(
        post.queued_derivations, 0,
        "the pulled drv is no longer Ready"
    );
    Ok(())
}

// r[verify sched.sla.reactive-floor+4]
/// D4: `InfrastructureFailure(CgroupOom)` doubles
/// `resource_floor.mem_bytes` for both FOD and non-FOD (D2: same
/// reactive path). The floor feeds `solve_intent_for`'s mem clamp →
/// next SpawnIntent is at least the doubled value.
///
/// (TransientFailure does NOT bump — that's a build-determinism
/// signal. CgroupOom is the worker-reported sizing signal.)
#[rstest::rstest]
#[case::fod(rio_proto::types::ExecutorKind::Fetcher, true, "oom-fod")]
#[case::builder(rio_proto::types::ExecutorKind::Builder, false, "glibc-177")]
#[tokio::test]
async fn cgroup_oom_doubles_mem_floor(
    #[case] kind: rio_proto::types::ExecutorKind,
    #[case] is_fod: bool,
    #[case] tag: &str,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        // Zero backoff so the retry redispatches on the next Tick.
        c.retry_policy = crate::RetryPolicy {
            backoff_base_secs: 0.0,
            ..Default::default()
        };
        // 256 GiB ceiling so the 2GiB→4GiB double isn't clamped.
        c.sla = test_sla_config();
    });

    // Delivery is pull; the executor kind no longer routes anything.
    let _ = kind;

    let mut node = make_node(tag);
    node.is_fixed_output = is_fod;
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    barrier(&handle).await;
    let first_asgn = pull_attempt(&handle, tag).await;
    assert!(first_asgn.drv_path.contains(tag));

    // Seed est_memory_bytes so the doubling has a base.
    handle
        .debug_seed_sched_hint(tag, Some(2 << 30), None, None, None)
        .await?;

    // Worker-reported CgroupOom → floor.mem doubled.
    pull_complete_failure(
        &handle,
        tag,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        &format!("{}; bumping resource floor", rio_proto::CGROUP_OOM_MSG),
    )
    .await?;
    tick(&handle).await?;

    assert_eq!(
        expect_drv(&handle, tag)
            .await
            .sched
            .resource_floor
            .mem_bytes,
        4 << 30,
        "CgroupOom → mem floor doubled (2GiB→4GiB)"
    );
    Ok(())
}

// ─── ADR-023 SlaEstimator → SpawnIntent ────────────────────────────────

/// `compute_spawn_intents` skips Ready nodes with `probed_generation==0`.
///
/// Pre-fix: `handle_substitute_complete{ok=true}` promotes dependents
/// Queued→Ready then defers their substitute-probe to the next Tick
/// (`dispatch_dirty=true`). A `GetSpawnIntents` poll in that ≤1s window
/// emitted intents → controller spawned Jobs → next Tick probed →
/// dependents went Substituting→Completed → `reap_stale_for_intents`
/// deleted the Pending Jobs 10s later. Fresh-cluster substitution
/// cascades hit this once per DAG layer.
///
/// `queued_by_system` is intentionally NOT gated — it must match
/// `ClusterSnapshot.queued_by_system` (snapshot.rs).
// r[verify sched.admin.spawn-intents.probed-gate+3]
#[tokio::test]
async fn spawn_intents_excludes_unprobed_ready() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (_store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig {
            sla: test_sla_config(),
            ..Default::default()
        },
        DagActorPlumbing {
            store_client: Some(store_client),
            ..Default::default()
        },
    );

    // `probeable`: has expected_output_paths → gate applies.
    // `unprobeable`: empty expected_output_paths (floating-CA / VM-test
    // SubmitBuild shape) → batch_probe_cached_ready never stamps it →
    // gate must NOT apply or it dead-locks at zero intents.
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        expected_output_paths: vec![test_store_path("probeable-out")],
        ..crate::db::RecoveryDerivationRow::test_default("probeable", "x86_64-linux")
    });
    actor.test_inject_ready("unprobeable", None, "x86_64-linux", false);
    actor.test_set_probed_generation("probeable", 0);
    actor.test_set_probed_generation("unprobeable", 0);

    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(
        snap.intents.len(),
        1,
        "probeable+gen=0 gated; unprobeable+gen=0 emits (gate exempt)"
    );
    assert_eq!(snap.intents[0].intent_id, "unprobeable");
    assert_eq!(
        snap.queued_by_system.get("x86_64-linux"),
        Some(&2),
        "queued_by_system counts both (matches ClusterSnapshot, gate-independent)"
    );

    actor.test_set_probed_generation("probeable", 1);
    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(snap.intents.len(), 2, "post-probe: both emit SpawnIntent");

    Ok(())
}

// r[verify sched.materialize.job+2]
/// PD-7 (Phase B, design §2.3): a node with an unresolved
/// materialization job is never a spawn-intent candidate — the
/// controller must not spawn builder pods for work that will be
/// materialized — and it is excluded from the per-system counts (the
/// §2.6 bucket exclusion's controller-facing twin, keeping
/// `GetSpawnIntents.queued_by_system` coherent with
/// `ClusterSnapshot.queued_by_system`). Resolving the job restores
/// candidacy.
#[tokio::test]
async fn spawn_intents_excludes_job_pending_nodes() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: test_sla_config(),
            ..Default::default()
        },
    );
    actor.test_inject_ready("pd7-job-pending", None, "x86_64-linux", false);
    actor.test_inject_ready("pd7-job-free", None, "x86_64-linux", false);
    // The unresolved (pending, unclaimed) job view entry.
    actor.materialization_jobs.insert(
        DrvHash::from("pd7-job-pending"),
        crate::actor::materialize::JobViewEntry::new_unclaimed(Uuid::new_v4(), None),
    );

    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(
        snap.intents.len(),
        1,
        "the job-pending node is never a spawn-intent candidate (PD-7); got {:?}",
        snap.intents
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(snap.intents[0].intent_id, "pd7-job-free");
    assert_eq!(
        snap.queued_by_system.get("x86_64-linux"),
        Some(&1),
        "job-pending nodes are excluded from queued_by_system \
         (coherent with the ClusterSnapshot bucket exclusion)"
    );

    // The job resolves (a SETTLED durable disposition removes the
    // view entry — the only removal path) → candidacy restored.
    assert!(
        actor.materialization_jobs.remove_settled(
            "pd7-job-pending",
            crate::actor::materialize::WriteDisposition::Applied,
        ),
        "settled removal of an existing entry"
    );
    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(
        snap.intents.len(),
        2,
        "resolving the job restores spawn-intent candidacy"
    );

    // (The flag-off invariance half died with the flag — the PD-7
    // filter is unconditional now.)
    Ok(())
}

/// `compute_spawn_intents` emits one SpawnIntent per Ready derivation,
/// with `intent_id == drv_hash` and `cores ≈ solve_tier(c_star)` for a
/// fitted key. Unfitted keys (no SlaEstimator entry) get probe defaults.
// r[verify sched.sla.intent-from-solve]
#[tokio::test]
async fn spawn_intent_from_sla_estimator() {
    use crate::sla::{solve, types::*};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    // Seed a single tier so the test exercises the Feasible path the way
    // a configured deploy would (empty ladder → solve_tier BestEffort at
    // p̄ capped at max_cores).
    actor.sla_tiers = vec![solve::Tier {
        name: "normal".into(),
        p50: None,
        p90: Some(1200.0),
        p99: None,
    }];

    // Seed a fit for ("test-pkg", x86_64-linux, ""): Amdahl s=30 p=2000.
    // Against p90=1200, β=30-e^{-0.128}·1200≈-1026 → c*=2000/1026≈1.95
    // → ceil → 2 cores.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "test-pkg".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(6 << 30),
        },
        disk_p90: Some(DiskBytes(10 << 30)),
        sigma_resid: 0.1,
        log_residuals: Vec::new(),
        // Asymptotic-n so z_q ≈ Φ⁻¹(0.9)=1.2816 and the closed-form
        // β math in the comment above holds; this test is about
        // SpawnIntent wiring, not small-n widening.
        n_eff_ring: RingNEff(1e6),
        fit_df: FitDf(1e6),
        n_distinct_c: 1_000_000,
        sum_w: 1e6,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(32.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });

    // "fitted" matches the seeded key; "cold" has no fit (different
    // pname). Both Ready, both non-FOD. test_inject_ready uses the
    // first arg verbatim as drv_hash.
    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux", false);
    actor.test_inject_ready("cold", Some("never-seen"), "x86_64-linux", false);

    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(
        snap.intents.len(),
        2,
        "one SpawnIntent per Ready derivation"
    );
    assert_eq!(snap.queued_by_system.get("x86_64-linux"), Some(&2));

    let fitted = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .expect("intent_id == drv_hash");
    assert_eq!(
        fitted.cores, 2,
        "solve_tier c_star ≈ 1.95 → ceil 2 (got {})",
        fitted.cores
    );
    assert_eq!(fitted.disk_bytes, 10 << 30, "disk_p90 from fit");
    assert!(fitted.mem_bytes >= 6 << 30, "mem ≥ p90 (× headroom)");

    let cold = snap.intents.iter().find(|i| i.intent_id == "cold").unwrap();
    // No SlaEstimator entry + no [sla] config → fallback probe (4c, 8Gi).
    assert_eq!(cold.cores, 4, "no fit → fallback probe cores");
    assert_eq!(cold.mem_bytes, 8 << 30);
}

/// `r[sched.sla.hw-class.ice-mask]`: `unfulfillable_cells` from the
/// controller marks the cell ICE; the read-time mask drops it from
/// `node_affinity` WITHOUT re-solving (memo unchanged); a successful
/// spawn ack clears the cell and the next dispatch sees it again.
///
/// R5B2: `_hw_ladder_exhausted_total` is gated on the ICE-edge
/// (`MemoEntry.ice_exhausted` rising), NOT on `was_miss`. ICE state is
/// read-time — explicitly NOT in `inputs_gen` — so `was_miss` is the
/// wrong granularity. Masking happens AFTER poll 1 here so `was_miss`
/// (true on poll 1) and all-masked (poll 4) are decoupled; the metric
/// must fire exactly once, on poll 4, and NOT on poll 5 (still
/// exhausted, no edge).
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn ice_mask_is_read_time() {
    use crate::sla::config::CapacityType;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    const LADDER: &str = "rio_scheduler_sla_hw_ladder_exhausted_total";
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.test_inject_ready("d", Some("test-pkg"), "x86_64-linux", false);

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);
    let ladder = |s: &metrics_util::debugging::Snapshotter| -> u64 {
        s.snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| ck.key().name() == LADDER)
            .map(|(.., v)| match v {
                DebugValue::Counter(c) => c,
                _ => 0,
            })
            .sum()
    };

    // ── poll 1: was_miss=true, ICE clear → no ladder emit ──────────────
    // Precondition: solve_full fires, affinity over A' populated.
    let (hw, cost, g0) = actor.solve_inputs();
    let intent = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert!(
        !intent.node_affinity.is_empty(),
        "precondition: solve_full path active"
    );
    let n0 = intent.node_affinity.len();
    assert_eq!(
        ladder(&snap),
        0,
        "poll 1: was_miss=true but ICE clear → ladder MUST NOT fire \
         (was_miss is the wrong gate)"
    );

    // Mask one cell from A' via the controller's unfulfillable report.
    let masked: crate::sla::config::Cell = ("intel-6".into(), CapacityType::Spot);
    actor
        .handle_ack_spawned_intents(&[], &["intel-6:spot".into()], &[], &[], &[], None)
        .expect("applied under leadership");
    assert!(actor.ice.is_masked(&masked));

    // ── poll 2: read-time mask, A\{masked}, not exhausted ──────────────
    let intent2 = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert_eq!(actor.solve_inputs().2, g0, "ICE state is NOT in inputs_gen");
    let intel6_spot = |t: &rio_proto::types::NodeSelectorTerm| {
        t.match_expressions
            .iter()
            .any(|r| r.key == "rio.build/hw-class" && r.values == ["intel-6"])
            && t.match_expressions
                .iter()
                .any(|r| r.key == "karpenter.sh/capacity-type" && r.values == ["spot"])
    };
    if intent.node_affinity.iter().any(intel6_spot) {
        assert!(
            !intent2.node_affinity.iter().any(intel6_spot),
            "masked cell dropped from affinity"
        );
        assert_eq!(intent2.node_affinity.len(), n0 - 1);
    }
    assert_eq!(ladder(&snap), 0, "poll 2: partial mask → no ladder");

    // ── poll 3: clear → full A' returns from memo ──────────────────────
    actor.ice.clear(&masked);
    let intent3 = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert_eq!(
        intent3.node_affinity.len(),
        n0,
        "memo never overwritten — unmask restores full A'"
    );
    assert_eq!(ladder(&snap), 0, "poll 3: clear → no ladder");

    // ── poll 4: all-of-H masked → §Capacity backoff exit (b): falls
    // back to A (not empty affinity) and emits ladder_exhausted on the
    // RISING edge. `was_miss=false` here (memo hit at g0); the
    // `was_miss && exhausted` gate would have been silent.
    for h in actor.sla_config.hw_classes.keys() {
        for cap in CapacityType::ALL {
            actor.ice.mark(&(h.clone(), cap));
        }
    }
    let intent4 = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert!(
        !intent4.node_affinity.is_empty(),
        "all-masked → still emit A (best-effort reserved for envelope-infeasibility)"
    );
    assert_eq!(
        ladder(&snap),
        1,
        "poll 4: ICE-edge `false→true` → ladder fires exactly once \
         (bug_006: `was_miss`-gated would be 0 here since miss was poll 1)"
    );

    // ── poll 5: still all-masked → NO re-emit (no edge) ────────────────
    let _ = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert_eq!(
        ladder(&snap),
        0,
        "poll 5: still exhausted, prev=true → no edge → no re-emit"
    );
}

/// Regression: `ice.clear()` was wired to the Pending ack (`spawned`),
/// not the success edge (`registered_cells`). The all-masked fallback
/// re-emits the masked cell at `node_affinity[0]`, so each tick did
/// `clear(C)` then `mark(C)` — `step` never climbed past 0, defeating
/// backoff doubling. Now `spawned` is informational-only; only
/// `registered_cells` (or first heartbeat) clears.
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn ice_step_doubles_across_mark_without_clear() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let cell: crate::sla::config::Cell = ("intel-6".into(), CapacityType::Spot);

    // The controller's ack echoes back the intent it spawned a Job
    // for. Under the all-masked fallback, `node_affinity[0]` IS the
    // masked cell — exactly what the old `term_to_cell` clear-loop
    // would have parsed back out and cleared.
    let spawned = SpawnIntent {
        node_affinity: vec![NodeSelectorTerm {
            match_expressions: vec![
                NodeSelectorRequirement {
                    key: "rio.build/hw-class".into(),
                    operator: "In".into(),
                    values: vec!["intel-6".into()],
                },
                NodeSelectorRequirement {
                    key: "karpenter.sh/capacity-type".into(),
                    operator: "In".into(),
                    values: vec!["spot".into()],
                },
            ],
        }],
        ..Default::default()
    };

    // Three DISTINCT failures of {spawned for cell, unfulfillable=cell,
    // no Registered signal}: step must climb 0→1→2. Old bug_008: each
    // tick cleared (from `spawned`) before marking → entry removed and
    // re-minted at step 0 forever. merged_bug_005: in-window re-marks
    // refresh at the same rung, so the climb is forced through
    // `force_expire` — the climb reaching 2 still proves `spawned`
    // never cleared (a clear would reset it).
    for i in 0..3 {
        if i > 0 {
            actor.ice.force_expire(&cell);
        }
        actor
            .handle_ack_spawned_intents(
                std::slice::from_ref(&spawned),
                &["intel-6:spot".into()],
                &[],
                &[],
                &[],
                None,
            )
            .expect("applied under leadership");
    }
    assert_eq!(
        actor.ice.step(&cell),
        Some(2),
        "spawned-ack must NOT clear; backoff doubles across consecutive \
         post-expiry failures"
    );

    // `registered_cells` IS the success signal → resets.
    actor
        .handle_ack_spawned_intents(&[], &[], &["intel-6:spot".into()], &[], &[], None)
        .expect("applied under leadership");
    assert_eq!(actor.ice.step(&cell), None, "registered_cells clears");
}

/// The pull-mint ICE clear keeps the bug_030 discipline now that the
/// heartbeat edge is gone: a successful pull is the success signal for
/// its intent, but a pod's `nodeAffinity` is an OR over A' — for
/// `|A'|>1` the pull identifies no single cell, so nothing is cleared
/// (over-clearing `cells[0]` would defeat `ice_step_doubles`); for
/// `|A'|==1` the single armed cell is cleared. `registered_cells`
/// (A18) stays the per-cell signal either way.
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn pull_mint_ice_clear_only_at_single_cell() -> TestResult {
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};

    let term = |h: &str| NodeSelectorTerm {
        match_expressions: vec![
            NodeSelectorRequirement {
                key: "rio.build/hw-class".into(),
                operator: "In".into(),
                values: vec![h.into()],
            },
            NodeSelectorRequirement {
                key: "karpenter.sh/capacity-type".into(),
                operator: "In".into(),
                values: vec!["spot".into()],
            },
        ],
    };
    let ack =
        |intent_id: &str, hw: &[&str], unfulfillable: &[&str]| ActorCommand::AckSpawnedIntents {
            reply: tokio::sync::oneshot::channel().0,
            spawned: vec![SpawnIntent {
                intent_id: intent_id.into(),
                hw_class_names: hw.iter().map(|h| h.to_string()).collect(),
                node_affinity: hw.iter().map(|h| term(h)).collect(),
                ..Default::default()
            }],
            unfulfillable_cells: unfulfillable.iter().map(|c| c.to_string()).collect(),
            registered_cells: vec![],
            observed_instance_types: vec![],
            bound_intents: vec![],
            binding_snapshot: None,
        };
    let masked = |snap: &crate::actor::SpawnIntentsSnapshot, cell: &str| {
        snap.ice_masked_cells.iter().any(|c| c == cell)
    };
    async fn snapshot(handle: &ActorHandle) -> crate::actor::SpawnIntentsSnapshot {
        handle
            .query_unchecked(|reply| {
                ActorCommand::Admin(AdminQuery::GetSpawnIntents {
                    req: crate::actor::SpawnIntentsRequest::default(),
                    reply,
                })
            })
            .await
            .expect("actor alive")
    }

    let (_db, handle, _task) = setup().await;

    // |A'| = 2: arm both cells for "ice-multi" and mark h0 ICE.
    let _ev1 = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ice-multi",
        PriorityClass::Scheduled,
    )
    .await?;
    handle
        .send_unchecked(ack("ice-multi", &["h0", "h1"], &["h0:spot"]))
        .await?;
    barrier(&handle).await;
    assert!(
        masked(&snapshot(&handle).await, "h0:spot"),
        "precondition: h0:spot marked ICE before the pull"
    );

    let _assignment = pull_attempt(&handle, "ice-multi").await;
    assert!(
        masked(&snapshot(&handle).await, "h0:spot"),
        "|A'|>1: the pull identifies no single cell; the mask (and its backoff step) \
         must survive the mint"
    );

    // |A'| = 1: the single armed cell IS cleared by the pull.
    let _ev2 = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ice-single",
        PriorityClass::Scheduled,
    )
    .await?;
    handle
        .send_unchecked(ack("ice-single", &["h2"], &["h2:spot"]))
        .await?;
    barrier(&handle).await;
    assert!(
        masked(&snapshot(&handle).await, "h2:spot"),
        "precondition: h2:spot marked ICE before the pull"
    );
    let _assignment = pull_attempt(&handle, "ice-single").await;
    let snap = snapshot(&handle).await;
    assert!(
        !masked(&snap, "h2:spot"),
        "|A'|==1: the pull is the success edge for the single armed cell — cleared"
    );
    assert!(
        masked(&snap, "h0:spot"),
        "the multi-cell intent's mask is untouched by the other intent's pull"
    );
    Ok(())
}

/// `r[sched.sla.hw-class.epsilon-explore]`: ε_h coin is a pure
/// function of `drv_hash`; `h_explore ∈ H\A` (or `H\{argmin price}`
/// on miss / A=H) is pinned in `MemoEntry.pinned_explore` and carried
/// across `inputs_gen` churn. The resulting affinity is
/// `⊆ {h_explore}×{spot,od}`. The memo is never overwritten.
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn epsilon_h_draws_outside_a() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    // Builders-only fixture: this test compares `in_a` (drv-routable
    // classes) against `h_all = cfg.hw_classes`. Featureless drvs
    // never route to fetcher cells (∅-guard), so fetcher-* in `h_all`
    // would make `in_a ⊊ h_all` trivially true.
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d", Some("test-pkg"), "x86_64-linux", false);

    // Baseline at ε=0 (bare_actor_hw default) so the memo is the
    // unrestricted solve, not an ε_h hit.
    let (hw, cost, g0) = actor.solve_inputs();
    let baseline = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    actor.sla_config.hw_explore_epsilon = 0.2; // max
    let in_a: std::collections::HashSet<String> = baseline
        .node_affinity
        .iter()
        .filter_map(|t| {
            t.match_expressions
                .iter()
                .find(|r| r.key == "rio.build/hw-class")
                .and_then(|r| r.values.first().cloned())
        })
        .collect();
    let h_all: std::collections::HashSet<String> =
        actor.sla_config.hw_classes.keys().cloned().collect();
    assert_ne!(in_a, h_all, "fixture: distinct factors ⇒ A⊊H");

    // ── Determinism (bug_049 regression) ───────────────────────────
    // Same (drv_hash, inputs_gen) → identical affinity across 10
    // calls. Before the fix, `rand::rng()` re-rolled per call →
    // selector-drift reap churn.
    let first = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    for _ in 0..10 {
        let again = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
        assert_eq!(
            again.node_affinity, first.node_affinity,
            "ε_h draw is deterministic for fixed (drv_hash, inputs_gen)"
        );
    }
    // Different inputs_gen → §Fourth-strike Option 2: ε_h pin is
    // carried across the staleness miss; seed no longer XORs
    // `inputs_gen`. Pass `g0+1` directly — equivalent to a real
    // hw/cost solve-relevant change. The Opt2-falsification contract
    // test asserts identity across REAL `inputs_gen` churn; this just
    // re-asserts determinism at the new gen (which it would have been
    // even pre-Opt2).
    let g1 = g0.wrapping_add(1);
    let after_bump = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g1);
    for _ in 0..10 {
        let again = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g1);
        assert_eq!(
            again.node_affinity, after_bump.node_affinity,
            "still deterministic at the new generation"
        );
    }
    // 200 distinct drv_hashes at ε=0.2 → ~40 expected hits; every hit
    // emits ≤2 terms over a single h ∉ A.
    let mut explore_hits = 0;
    for i in 0..200 {
        let dh = format!("d{i}");
        actor.test_inject_ready(&dh, Some("test-pkg"), "x86_64-linux", false);
        let intent = actor.solve_intent_for(actor.dag.node(dh.as_str()).unwrap(), &hw, &cost, g1);
        let hs: std::collections::HashSet<String> = intent
            .node_affinity
            .iter()
            .filter_map(|t| {
                t.match_expressions
                    .iter()
                    .find(|r| r.key == "rio.build/hw-class")
                    .and_then(|r| r.values.first().cloned())
            })
            .collect();
        if hs.len() == 1 && hs != in_a {
            explore_hits += 1;
            let h = hs.into_iter().next().unwrap();
            assert!(!in_a.contains(&h), "h_explore={h} must be ∉ A; A={in_a:?}");
            // A' ⊆ {h_explore}×{spot,od}.
            assert!(intent.node_affinity.len() <= 2);
        }
    }
    // ε=0.2, 200 trials → ~40 expected; ≥10 with overwhelming prob.
    assert!(explore_hits >= 10, "ε_h fires: got {explore_hits}/200");
    // Memo unchanged across all draws (one ModelKey, one override).
    assert_eq!(actor.solve_cache.len(), 1);

    // ── Fallback branch: in_a=∅ OR A=H → H\{argmin price} ──────────
    // Regression: with the old `pool = H\in_a; if empty → H\{cheapest}`,
    // the in_a=∅ case (memo=None / BestEffort) gave pool=H and the
    // fallback never fired. Both `in_a.is_empty()` and `in_a==H` now
    // route directly to H\{cheapest}. Exercise via A=H (observable):
    // equalize hw factors + prices within τ → every spot cell in A.
    // Seed prices are cap-only (all h equal) → cheapest_h would be
    // HashMap-iteration order; set distinct prices within τ=0.15 so
    // cheapest is deterministic AND A still spans H.
    {
        use crate::sla::config::CapacityType::Spot;
        let mut ct = actor.cost_table.write();
        ct.set_price("intel-6", Spot, 0.0100, 1e9);
        ct.set_price("intel-7", Spot, 0.0105, 1e9);
        ct.set_price("intel-8", Spot, 0.0110, 1e9);
    }
    let cheapest = actor.cost_table.read().cheapest_h(&h_all).unwrap();
    assert_eq!(cheapest, "intel-6");
    let mut m = HashMap::new();
    for h in &h_all {
        m.insert(h.clone(), 1.0);
    }
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));
    // hw + cost both changed → re-derive (no bump call to forget).
    let (hw2, cost2, g2) = actor.solve_inputs();
    assert_ne!(g2, g0, "seed_hw + set_price → derived inputs_gen changed");
    actor.sla_config.hw_explore_epsilon = 0.0;
    let in_a2: std::collections::HashSet<_> = actor
        .solve_intent_for(actor.dag.node("d").unwrap(), &hw2, &cost2, g2)
        .node_affinity
        .iter()
        .filter_map(|t| {
            t.match_expressions
                .iter()
                .find(|r| r.key == "rio.build/hw-class")
                .and_then(|r| r.values.first().cloned())
        })
        .collect();
    assert_eq!(in_a2, h_all, "fixture: equal factors + prices in τ ⇒ A=H");
    actor.sla_config.hw_explore_epsilon = 0.2;
    let mut hits = 0;
    for i in 0..200 {
        let dh = format!("e{i}");
        actor.test_inject_ready(&dh, Some("test-pkg"), "x86_64-linux", false);
        let intent = actor.solve_intent_for(actor.dag.node(dh.as_str()).unwrap(), &hw2, &cost2, g2);
        let hs: std::collections::HashSet<String> = intent
            .node_affinity
            .iter()
            .filter_map(|t| {
                t.match_expressions
                    .iter()
                    .find(|r| r.key == "rio.build/hw-class")
                    .and_then(|r| r.values.first().cloned())
            })
            .collect();
        if hs.len() == 1 {
            hits += 1;
            let h = hs.into_iter().next().unwrap();
            assert_ne!(
                h, cheapest,
                "A=H fallback must draw from H\\{{argmin price={cheapest}}}"
            );
        }
    }
    assert!(hits >= 10, "ε_h fires under A=H: got {hits}/200");
}

/// `solve_full` wired into the actor: with `hw_cost_source` set,
/// `hw_classes` configured, a populated hw-factor table, and a fitted
/// key, `SpawnIntent.node_affinity` carries OR-of-ANDs `(h, cap)`
/// terms.
// r[verify sched.sla.hw-class.admissible-set]
#[tokio::test]
async fn spawn_intent_node_affinity_from_solve_full() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux", false);
    actor.test_inject_ready("cold", Some("never-seen"), "x86_64-linux", false);

    let snap = actor.compute_spawn_intents(&Default::default());

    let fitted = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .unwrap();
    assert!(
        !fitted.node_affinity.is_empty(),
        "solve_full populated affinity: {:?}",
        fitted.node_affinity
    );
    // Each term: h-label conjunction PLUS capacity-type.
    for term in &fitted.node_affinity {
        assert!(
            term.match_expressions
                .iter()
                .any(|r| r.key == "karpenter.sh/capacity-type")
        );
        assert!(
            term.match_expressions
                .iter()
                .any(|r| r.key == "rio.build/hw-class")
        );
    }

    let cold = snap.intents.iter().find(|i| i.intent_id == "cold").unwrap();
    assert!(
        cold.node_affinity.is_empty(),
        "no fit → hw-agnostic intent_for path"
    );

    // Determinism: re-emit returns the SAME affinity (memoized; no
    // softmax re-roll). The controller's `reap_stale_for_intents` sees
    // the same fingerprint across re-polls.
    let snap2 = actor.compute_spawn_intents(&Default::default());
    let fitted2 = snap2
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .unwrap();
    assert_eq!(
        fitted.node_affinity, fitted2.node_affinity,
        "deterministic — memoized"
    );
}

/// `solve_full` gate predicates: each of `enableParallelBuilding=false`
/// / `--tier` override falls through to the hw-agnostic `intent_for`
/// path even with `hw_cost_source` set and a usable fit.
/// `required_features=["kvm"]` with NO hwClass providing kvm ALSO falls
/// through (h_all empties under the §13c partition →
/// `!h_all.is_empty()` gate fails). `--mem`-only override does NOT gate
/// it off — solve_full runs and the override overlays the result.
/// §13e: FOD no longer falls through — `effective_features = [fetcher]`
/// partitions `h_all` to `fetcher-*` and FOD participates in solve_full.
// r[verify sched.sla.hw-class.admissible-set]
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn solve_full_gate_skips_kvm_serial_and_override() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Re-seed under pname "pkg" (bare_actor_hw seeds "test-pkg").
    seed_fit(&actor, "pkg");

    // Baseline (non-FOD, no features) — proves the fixture DOES route
    // through solve_full so the negative assertions below are
    // meaningful.
    actor.test_inject_ready("base", Some("pkg"), "x86_64-linux", false);
    // FOD with the same fitted pname — §13e: routes to fetcher-*.
    actor.test_inject_ready("fod", Some("pkg"), "x86_64-linux", true);
    // required_features non-empty (kvm pool).
    actor.test_inject_ready_with_features("kvm", Some("pkg"), "x86_64-linux", &["kvm"]);
    // enableParallelBuilding=false — set on the state directly
    // (RecoveryDerivationRow has no column for it).
    actor.test_inject_ready("serial", Some("pkg"), "x86_64-linux", false);
    actor
        .dag
        .node_mut("serial")
        .unwrap()
        .enable_parallel_building = Some(false);

    let snap = actor.compute_spawn_intents(&Default::default());
    let by_id = |id: &str| snap.intents.iter().find(|i| i.intent_id == id).unwrap();

    assert!(
        !by_id("base").node_affinity.is_empty(),
        "fixture sanity: baseline routes through solve_full"
    );
    let fod = by_id("fod");
    assert!(
        !fod.hw_class_names.is_empty()
            && fod.hw_class_names.iter().all(|h| h.starts_with("fetcher-")),
        "§13e: FOD routes through solve_full to fetcher-* cells; got {:?}",
        fod.hw_class_names
    );
    assert!(
        by_id("kvm").node_affinity.is_empty(),
        "no hwClass provides kvm → h_all=∅ → hw-agnostic fallthrough"
    );
    assert!(
        by_id("kvm").hw_class_names.is_empty(),
        "no hwClass provides kvm → hw_class_names=∅"
    );
    let serial = by_id("serial");
    assert!(
        serial.node_affinity.is_empty(),
        "enableParallelBuilding=false stays hw-agnostic"
    );
    assert_eq!(
        serial.cores, 1,
        "serial drv pinned to 1 core via intent_for (r[sched.sla.intent-from-solve])"
    );

    // ── override gating (deferred from 2b6001b7) ────────────────────
    // `tier` override → solve_full skipped (intent_for honors tier).
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "pkg".into(),
            tier: Some("normal".into()),
            ..Default::default()
        }]);
    let snap = actor.compute_spawn_intents(&Default::default());
    let base = snap.intents.iter().find(|i| i.intent_id == "base").unwrap();
    assert!(
        base.node_affinity.is_empty(),
        "tier override gates solve_full off"
    );

    // `mem`-only override → solve_full SKIPPED (bug_033: any override
    // field routes hw-agnostic intent_for; the post-solve overlay is
    // gone), mem honored by intent_for.
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "pkg".into(),
            mem_bytes: Some(32 << 30),
            ..Default::default()
        }]);
    let snap = actor.compute_spawn_intents(&Default::default());
    let base = snap.intents.iter().find(|i| i.intent_id == "base").unwrap();
    assert!(
        base.node_affinity.is_empty(),
        "mem-only override gates solve_full off (bug_033)"
    );
    assert_eq!(base.mem_bytes, 32 << 30, "forced_mem honored by intent_for");
}

/// SLA mode: `try_dispatch_one` writes `solve_intent_for().0` to
/// `sched.est_cores`, and `build_assignment_proto` forwards it as
/// `WorkAssignment.assigned_cores`.
// r[verify sched.sla.cores-reach-nix-build-cores]
#[tokio::test]
async fn work_assignment_carries_sla_cores() {
    use crate::sla::types::*;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    // Same Amdahl fit as `spawn_intent_from_sla_estimator` → c*≈1.95 → 2.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "test-pkg".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(6 << 30),
        },
        disk_p90: Some(DiskBytes(10 << 30)),
        sigma_resid: 0.1,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(10.0),
        fit_df: FitDf(10.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(32.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });

    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux", false);
    let intent = {
        let state = actor.dag.node("fitted").unwrap();
        solve_intent(&actor, state)
    };
    let expected_cores = intent.cores;

    // The pull mint is the intent writer now: stamp the solve onto the
    // node exactly as `mint_and_deliver` does, then build the payload it
    // would deliver.
    actor.dag.node_mut("fitted").unwrap().sched.last_intent = Some(intent);
    let assignment = actor
        .build_assignment_proto(
            &"fitted".into(),
            &"fitted".into(),
            rio_evidence_kernel::pull::PullKind::Build,
        )
        .await
        .expect("payload for an injected Ready node");
    assert_eq!(
        assignment.assigned_cores,
        Some(expected_cores),
        "SLA mode: WorkAssignment.assigned_cores == solve_intent_for().cores"
    );
}

/// `solve_intent_for`'s fitted-path `deadline_secs` is wall-clock, not
/// ref-seconds. `t_at()` evaluates the ref-second fit; with a slow
/// hw_class (factor < 1) in the table the wall-clock budget on that
/// node is `ref / factor` — without de-normalization a build there is
/// killed at `ref_q99 × 5` < `wall_q99 × 5`. Reverting the
/// `/ hw.min_factor()` in `snapshot.rs` makes `slow > fast` fail.
// r[verify sched.sla.hw-ref-seconds]
#[tokio::test]
async fn solve_intent_deadline_denormalized_to_slowest_hw() {
    use crate::sla::{hw, types::*};
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    // probe deadline 1s so the `c.max(probe_deadline)` floor doesn't
    // mask the computed value.
    actor.sla_config.probe.deadline_secs = 1;
    // High-S Amdahl so c* clamps low and `T(c)` ≈ S regardless of how
    // many cores the solve picks — keeps the deadline arithmetic
    // independent of the cores decision.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "test-pkg".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(1000.0),
            p: RefSeconds(10.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(6 << 30),
        },
        disk_p90: Some(DiskBytes(10 << 30)),
        sigma_resid: 0.0,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(10.0),
        fit_df: FitDf(10.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(32.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });
    actor.test_inject_ready("d", Some("test-pkg"), "x86_64-linux", false);
    let state = actor.dag.node("d").unwrap();

    // Baseline: empty hw table → min_factor()=1.0 → deadline ≈ ref_q99×5.
    let baseline = solve_intent(&actor, state).deadline_secs;

    // Fast-only table (factor 2.0): wall = ref/2 → deadline ≈ baseline/2.
    let mut fast = HashMap::new();
    fast.insert("aws-8-nvme".into(), 2.0);
    actor.sla_estimator.seed_hw(hw::HwTable::from_map(fast));
    let fast_dl = solve_intent(&actor, state).deadline_secs;
    assert!(
        fast_dl < baseline,
        "factor>1 → deadline shrinks (was over-budget): {fast_dl} < {baseline}"
    );

    // Slow class added (factor 0.5): worst-case wall = ref/0.5 = 2×ref.
    // Deadline must budget for the slowest band → ≈ 2×baseline.
    let mut mixed = HashMap::new();
    mixed.insert("aws-8-nvme".into(), 2.0);
    mixed.insert("aws-4-slow".into(), 0.5);
    actor.sla_estimator.seed_hw(hw::HwTable::from_map(mixed));
    let slow_dl = solve_intent(&actor, state).deadline_secs;
    assert!(
        slow_dl > baseline && slow_dl > fast_dl,
        "factor<1 in table → deadline budgets slowest wall: {slow_dl} > {baseline}"
    );
    // Ratio check: slow/fast ≈ (1/0.5)/(1/2.0) = 4. Allow slack for the
    // p≠0 term in T(c) and integer truncation.
    let ratio = f64::from(slow_dl) / f64::from(fast_dl);
    assert!(
        (3.5..=4.5).contains(&ratio),
        "slow/fast deadline ratio ≈ min_factor inverse: {ratio}"
    );
}

// ---------------------------------------------------------------------------
// I-065 fleet-exhaustion: system/feature awareness
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Detached-substitute completion: leader gate, one-shot suppress, progress
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// I-139: locally-present completion is batched (no per-row PG awaits)
// ---------------------------------------------------------------------------

/// `batch_probe_cached_ready`'s locally-present branch must batch the
/// PG writes — pre-fix it awaited `complete_ready_from_store` per
/// item (≥3 sequential PG RTTs each); on warm-restart of a large
/// closure ~all 2048 candidates hit it → 12-30s actor stall.
/// Structural assertion via `persist_status_calls`: batch path uses
/// `persist_status_batch` (NOT counted), so 0 singular calls during
/// the dispatch pass means the batch helper was used.
#[tokio::test]
async fn batch_probe_locally_present_batches_pg() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    // Builder for an unrelated arch: heartbeat sets dispatch_dirty so
    // 50 leaves, all outputs seeded present → all hit the
    // locally-present branch on the first dispatch_ready.
    let mut nodes = Vec::with_capacity(50);
    {
        let mut paths = store.state.paths.write().unwrap();
        for i in 0..50 {
            let out = test_store_path(&format!("bp-{i}-out"));
            paths.insert(out.clone(), Default::default());
            let mut n = make_node(&format!("bp-{i}"));
            n.expected_output_paths = vec![out];
            nodes.push(n);
        }
    }
    // fail_find_missing during merge so check_cached_outputs +
    // merge's inline dispatch_ready don't complete them — we want
    // them Ready at the NEXT dispatch-time batch.
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], false).await?;
    barrier(&handle).await;
    store
        .faults
        .fail_find_missing
        .store(false, Ordering::SeqCst);

    let before = handle.debug_counters().await?.persist_status_calls;
    tick(&handle).await?;
    barrier(&handle).await;
    let after = handle.debug_counters().await?.persist_status_calls;

    for i in 0..50 {
        assert_eq!(
            expect_drv(&handle, &format!("bp-{i}")).await.status,
            DerivationStatus::Completed,
            "node bp-{i} must be Completed by the dispatch-time batch"
        );
    }
    assert_eq!(
        after, before,
        "locally-present completion must use persist_status_batch (0 singular calls); \
         pre-fix this was {} (one per node + one per newly-ready)",
        50
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// I-139/I-140: batch-probe truncated tail must NOT hit per-drv FMP fallback
// ---------------------------------------------------------------------------

// r[verify sched.dispatch.fod-substitute+3]
/// With > `DISPATCH_PROBE_BATCH_CAP` Ready leaves and the batch RPC
/// failing-open, the truncated tail must NOT fall through to the
/// per-drv `ready_check_or_spawn` (one inline-awaited FMP each =
/// O(N) sequential 30s timeouts in the actor). Pre-fix:
/// `find_missing_calls == 1 + tail` (tail × 30s ≈ 24h stall with an
/// unreachable store). Post-fix: 1 batch + 0 per-drv; tail dispatches
/// fail-open and is batch-probed next pass.
#[tokio::test]
async fn batch_probe_tail_never_per_drv_fmp() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // CAP+12 IA leaves with non-empty expected_output_paths so they're
    // batch candidates. No worker connected → all defer (we only care
    // about FMP call count, not assignment).
    let n = crate::actor::DISPATCH_PROBE_BATCH_CAP + 12;
    let nodes: Vec<_> = (0..n)
        .map(|i| {
            let tag = format!("bpt-{i}");
            let mut node = make_node(&tag);
            node.expected_output_paths = vec![test_store_path(&format!("bpt-{i}-out"))];
            node
        })
        .collect();
    // Batch FMP fails-open (Unavailable). Per-drv calls would also fail
    // — but the bug is they're MADE at all.
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], false).await?;
    let after_merge = store.calls.find_missing_calls.load(Ordering::SeqCst);
    // The ready-set sweep runs inline inside merge; count what THAT did.
    tick(&handle).await?;
    let total = store.calls.find_missing_calls.load(Ordering::SeqCst);
    // 1 batch FMP per sweep (merge's inline sweep + our explicit tick).
    // Neither may trigger per-drv tail calls — the per-drv fallback no
    // longer exists; the bug case was +12 EXTRA (one per tail node).
    let sweep_calls = total - after_merge;
    assert!(
        sweep_calls <= 2,
        "tick → ≤2 ready-set sweeps → ≤2 batch FMPs; \
         got {sweep_calls} (pre-fix: 1 batch + 12 per-drv tail = 13)"
    );
    assert!(
        total <= 8,
        "total FMPs across merge+tick bounded by batch passes, NOT by \
         tail size; got {total} (12 truncated nodes; pre-fix the tail \
         leaked to per-drv calls each pass)"
    );
    Ok(())
}

/// merged_bug_017: SolveCache is keyed on `model_key_hash` and bounded
/// by "|live SlaEstimator keys| × |overrides|" — but the bound only
/// holds if LRU eviction propagates. Without the `on_evict` hook, the
/// solve_cache entry for an evicted fit is orphaned forever
/// (`solve_intent_for` short-circuits on `cached(k) == None` before
/// reaching `get_or_insert_with`).
#[tokio::test]
async fn solve_cache_evicted_with_lru() {
    use crate::sla::solve::model_key_hash;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut cfg = test_hw_sla_config();
    cfg.max_keys_per_tenant = 3;
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

    // Fill LRU to cap and populate solve_cache for each.
    for p in ["p0", "p1", "p2"] {
        seed_fit(&actor, p);
        actor.test_inject_ready(p, Some(p), "x86_64-linux", false);
        let _ = solve_intent(&actor, actor.dag.node(p).unwrap());
    }
    assert_eq!(actor.solve_cache.len(), 3, "solve_cache one per fit");

    // cap+1 insert via the SAME `on_evict` wiring `maybe_refresh_estimator`
    // uses (housekeeping.rs). LRU evicts the least-recently-used fit;
    // solve_cache must drop its memo.
    let solve_cache = std::sync::Arc::clone(&actor.solve_cache);
    let p3 = make_fit("p3");
    actor.sla_estimator.insert(&p3.key.clone(), p3, |k| {
        solve_cache.remove_model_key(model_key_hash(k))
    });
    assert_eq!(
        actor.solve_cache.len(),
        2,
        "evicted ModelKey's solve_cache entry dropped via on_evict"
    );

    // Steady-state: solving for the surviving fits + p3 stays ≤ cap.
    seed_fit(&actor, "p3");
    actor.test_inject_ready("p3", Some("p3"), "x86_64-linux", false);
    let _ = solve_intent(&actor, actor.dag.node("p3").unwrap());
    assert!(
        actor.solve_cache.len() <= 3,
        "solve_cache bounded by max_keys_per_tenant"
    );
}

/// merged_bug_028: `inputs_gen` was an `AtomicU64` whose `bump()` was
/// unconditional every 60s, so ε_h re-rolled before Karpenter could
/// provision an explore node. Third-strike redesign: `inputs_gen` is
/// **derived** from `(HwTable, CostTable)` solve-relevant projection;
/// nobody bumps. merged_bug_018 / fourth-strike Option 1: that
/// projection hashed bit-exact f64 EMA *state* (diverging) instead of
/// the converging *signal* solve reads — quantize. This asserts the
/// projection's stability under noise-band perturbation, NOT just
/// bit-identical re-inserts.
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn inputs_gen_stable_across_noop_refresh() -> TestResult {
    use crate::sla::solve::SolveInputs;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let sdb = SchedulerDb::new(db.pool.clone());
    let est = crate::sla::SlaEstimator::for_test(&test_hw_sla_config());
    let mut cost = crate::sla::cost::CostTable::default();
    let derive = |est: &crate::sla::SlaEstimator, cost: &crate::sla::cost::CostTable| {
        SolveInputs {
            hw: &est.hw_table(),
            cost,
        }
        .inputs_gen()
    };
    let seed_hw = |pod: String, tenant: String, alu: f64| {
        sqlx::query(
            "INSERT INTO hw_perf_samples \
             (hw_class, pod_id, submitting_tenant, factor) \
             VALUES ('intel-7', $1, $2, jsonb_build_object('alu', $3::float8))",
        )
        .bind(pod)
        .bind(tenant)
        .bind(alu)
        .execute(&db.pool)
    };

    // 10 refreshes, no hw_perf rows → solve_relevant_hash unchanged →
    // derived inputs_gen identical.
    let g0 = derive(&est, &cost);
    for _ in 0..10 {
        est.refresh(&sdb, &[], |_| {}).await?;
    }
    assert_eq!(derive(&est, &cost), g0, "no-op refresh → inputs_gen stable");

    // PREFIX: ≥FLEET_MEDIAN_MIN_TENANTS (5) distinct submitting_tenant
    // rows — else hw.rs cross_tenant_median pins factor=[1.0;K] and the
    // noise-band assertions below are vacuous. pod_ids 0→5 crosses
    // HW_MIN_PODS → trust bool flips → key enters hash → inputs_gen
    // changes. factor[0] = median(5× 1.4) = 1.4 → bucket 140.
    for i in 0..5 {
        seed_hw(format!("pod-{i}"), format!("t{i}"), 1.4).await?;
    }
    est.refresh(&sdb, &[], |_| {}).await?;
    let g1 = derive(&est, &cost);
    assert_ne!(g1, g0, "0→5 pods crosses trust threshold → derived change");

    // merged_bug_011 regression: pod_ids 5→6, factor bit-identical →
    // inputs_gen UNCHANGED. The old `content_hash` hashed raw `pod_ids`
    // and would have changed here.
    seed_hw("pod-x".into(), "t0".into(), 1.4).await?;
    est.refresh(&sdb, &[], |_| {}).await?;
    assert_eq!(
        derive(&est, &cost),
        g1,
        "pod_ids 5→6 within trusted, factor unchanged → inputs_gen UNCHANGED"
    );

    // (a) merged_bug_018: 5 NEW tenants at alu=1.401 (NOT bit-identical)
    // → median of 10 tenant-medians = xs[5] = 1.401. Bit-exact hash
    // would change; quantized (1.401·100).round()=140=(1.4·100).round()
    // → UNCHANGED. This is the noise-band assertion the prior test
    // (bit-identical 1.4 re-insert) missed.
    for i in 5..10 {
        seed_hw(format!("pod-{i}"), format!("t{i}"), 1.401).await?;
    }
    est.refresh(&sdb, &[], |_| {}).await?;
    assert_eq!(
        derive(&est, &cost),
        g1,
        "factor 1.4→1.401 within 1% bucket → inputs_gen UNCHANGED"
    );

    // (b) cost-side: λ hashed as the converging `lambda_for` quotient,
    // NOT diverging (num, den) sums. Steady 600s exposure with
    // interrupts at LAMBDA_SEED rate → λ̂ stays exactly SEED (the
    // Gamma-Poisson `(a·s + b·s)/(a+b)=s` identity) regardless of how
    // (num, den, node_count) grow per tick. First tick adds the λ key
    // → new baseline g2; second tick → (num,den) bit-change, λ̂ doesn't.
    let seed_tick = |at: f64| {
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value, at) \
             VALUES ('', 'intel-7', 'exposure', 600, to_timestamp($1)), \
                    ('', 'intel-7', 'interrupt', $2, to_timestamp($1))",
        )
        .bind(at)
        .bind(600.0 * crate::sla::cost::LAMBDA_SEED)
        .execute(&db.pool)
    };
    seed_tick(1000.0).await?;
    cost.refresh_lambda(&sdb).await?;
    let g2 = derive(&est, &cost);
    assert_ne!(g2, g1, "λ key entered → cost-side hash changes");
    seed_tick(1600.0).await?;
    cost.refresh_lambda(&sdb).await?;
    assert_eq!(
        derive(&est, &cost),
        g2,
        "steady refresh_lambda → λ̂ converges, (num,den) diverge → UNCHANGED"
    );

    // (c) Real change: factor moves >1% (out of bucket). Clear and
    // re-seed 5 tenants at alu=1.5 → bucket 150 ≠ 140 → CHANGED.
    sqlx::query("DELETE FROM hw_perf_samples")
        .execute(&db.pool)
        .await?;
    for i in 0..5 {
        seed_hw(format!("pod-c{i}"), format!("t{i}"), 1.5).await?;
    }
    est.refresh(&sdb, &[], |_| {}).await?;
    assert_ne!(
        derive(&est, &cost),
        g2,
        "factor 1.401→1.5 crosses 1% bucket → inputs_gen CHANGED"
    );
    Ok(())
}

/// `compute_spawn_intents` carries `IceBackoff::masked_cells()` in the
/// snapshot. Pre-R24B6a `admin/spawn_intents.rs` hardcoded `vec![]`, so
/// the controller's `cover_deficit` mask never received the scheduler's
/// accumulated ladder and rediscovered ICE per cell on every controller
/// restart (mb_001).
#[tokio::test]
async fn compute_spawn_intents_carries_ice_masked_cells() -> TestResult {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let actor = bare_actor_sla(db.pool.clone());
    actor
        .ice
        .mark(&("hi-ebs-x86".to_string(), CapacityType::Spot));
    let snap = actor.compute_spawn_intents(&Default::default());
    assert!(
        snap.ice_masked_cells
            .contains(&"hi-ebs-x86:spot".to_string()),
        "masked cell flows to snapshot via cell_label: got {:?}",
        snap.ice_masked_cells
    );
    Ok(())
}

// r[verify sched.build.terminal-status-settled+3]
/// A dispatch-time store hit that fans out to a resident terminal build
/// must not mutate its served accounting and must not emit a
/// post-terminal `BuildProgress` to its watchers.
///
/// Staging: B1 builds X and succeeds (terminal, resident for the cleanup
/// window, interest retained on X). B2 re-merges X while X's output is
/// missing from the store, so the stale-Completed verify resets X to
/// Ready. The output then appears in the store and the next dispatch
/// sweep's batched Ready probe completes X as cached, fanning out to
/// every interested build — including terminal B1. B1's served
/// accounting (cached_derivations), its live event stream, and its
/// WatchBuild snapshot must all stay settled: a post-`BuildCompleted`
/// `BuildProgress` recomputed from the still-mutating DAG would reach
/// watchers with totals the finished build no longer describes.
#[tokio::test]
async fn test_terminal_build_frozen_on_dispatch_store_hit() -> TestResult {
    use rio_proto::types::build_event::Event;

    // Setup: mock store. B1's broadcast receiver is held for the whole
    // test so every event B1's watchers would see is observable.
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_, _| {});

    let x_out = test_store_path("tfz-x-out");

    // B1: single node X. Nothing in the store at merge time → X Ready.
    let b1 = Uuid::new_v4();
    let mut x = make_node("tfz-x");
    x.expected_output_paths = vec![x_out.clone()];
    let mut b1_events = merge_dag(&handle, b1, vec![x.clone()], vec![], false).await?;
    barrier(&handle).await;

    // Build X via the pull surface → B1 Succeeded.
    pull_complete_success(&handle, "tfz-x", &x_out).await?;
    wait_for_status(&handle, "tfz-x", DerivationStatus::Completed).await;
    let settled = query_status(&handle, b1).await?;
    assert_eq!(
        settled.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "precondition: B1 finished"
    );
    assert_eq!(
        settled.cached_derivations, 0,
        "precondition: B1 built X from source — no cache hits"
    );

    // Drain B1's stream up to (and including) its BuildCompleted,
    // CAPTURING the emitted payload — the settled snapshot must serve
    // exactly these bytes later, whatever happens to the shared DAG.
    let mut live_completed: Option<rio_proto::types::BuildCompleted> = None;
    while let Ok(ev) = b1_events.try_recv() {
        if let Some(Event::Completed(c)) = ev.event {
            live_completed = Some(c);
        }
    }
    let live_completed = live_completed.expect("precondition: B1's BuildCompleted observed");

    // B2 re-merges X (+ a sibling root Z so B2 stays live). x_out is NOT
    // in the mock store → the stale-Completed verify resets X to Ready.
    // B1 (terminal, resident) keeps its interest in X.
    let b2 = Uuid::new_v4();
    let mut z = make_node("tfz-z");
    z.expected_output_paths = vec![test_store_path("tfz-z-out")];
    merge_dag(&handle, b2, vec![x, z], vec![], false).await?;
    barrier(&handle).await;
    let xs = expect_drv(&handle, "tfz-x").await;
    assert!(
        matches!(
            xs.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "precondition: stale-Completed verify must reset X (its output is \
         missing from the store); got {:?}",
        xs.status
    );

    // merged_bug_097 (settled-payload leg): while X is mid-flight under
    // B2 — claimed by a worker, Running with B2's exec — the TERMINAL
    // B1's served surfaces must not re-derive from the mutated DAG:
    //   - counts stay settled (completed == total, not shrunk by the
    //     reset),
    //   - the running set stays EMPTY (not listing B2's execution),
    //   - output_paths stay the captured emit's (not recomputed from
    //     the reset node).
    let b2_assignment = pull_attempt(&handle, "tfz-x").await;
    barrier(&handle).await;
    {
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::WatchBuild {
                build_id: b1,
                caller_tenant: None,
                reply: reply_tx,
            })
            .await?;
        let (_rx, snapshot) = reply_rx.await??;
        let Some(Event::Snapshot(mid)) = snapshot.event else {
            panic!("WatchBuild reply must carry a Snapshot");
        };
        assert_eq!(
            mid.completed_derivations, mid.total_derivations,
            "settled counts: a terminal build's completed must not \
             shrink when a shared node is reset under a later build"
        );
        assert!(
            mid.running.is_empty(),
            "settled running set: a terminal build must not list \
             another build's execution (got exec {})",
            b2_assignment.exec_id
        );
        assert_eq!(
            mid.output_paths, live_completed.output_paths,
            "settled output_paths: the snapshot serves the captured \
             emit, not a recomputation from the mutated DAG"
        );
        // PG: the persisted counts are final at the terminal transition.
        let row: (i64,) =
            sqlx::query_as("SELECT completed_drvs::bigint FROM builds WHERE build_id = $1")
                .bind(b1)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(
            row.0, mid.total_derivations as i64,
            "persisted completed_drvs stays at total after the terminal \
             transition (no post-terminal overwrite)"
        );
    }

    // Release B2's claim: a transient failure requeues X to Ready so the
    // original dispatch-time store-hit flow below proceeds unchanged.
    pull_complete_failure(
        &handle,
        "tfz-x",
        rio_proto::types::BuildResultStatus::TransientFailure,
        "release the claim for the cached-fanout leg",
    )
    .await?;
    wait_for_status(&handle, "tfz-x", DerivationStatus::Ready).await;

    // The output appears in the store → the next dispatch sweep's
    // batched Ready probe completes X as cached and fans out to ALL
    // interested builds — including terminal B1.
    store.seed_with_content(&x_out, b"x");
    tick(&handle).await?;
    barrier(&handle).await;

    // Sanity: the fan-out ran — the live build counted the store hit.
    let s2 = query_status(&handle, b2).await?;
    assert!(
        s2.cached_derivations >= 1,
        "the live build counts the dispatch-time store hit; got {}",
        s2.cached_derivations
    );

    // 1. The terminal build's served accounting is frozen.
    let after = query_status(&handle, b1).await?;
    assert_eq!(
        after.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "B1 stays terminal"
    );
    assert_eq!(
        after.cached_derivations, 0,
        "a terminal build's cached_derivations must not drift after the \
         terminal transition"
    );

    // 2. No post-terminal BuildProgress reached B1's live stream. The
    //    per-drv DerivationCached fan-out event is allowed (it is a fact
    //    about the derivation, not aggregate progress of the finished
    //    build); aggregate Progress is not.
    let mut post_terminal_progress = 0usize;
    while let Ok(ev) = b1_events.try_recv() {
        if matches!(ev.event, Some(Event::Progress(_))) {
            post_terminal_progress += 1;
        }
    }
    assert_eq!(
        post_terminal_progress, 0,
        "no BuildProgress after BuildCompleted may be emitted to a \
         terminal build's watchers (it would show totals shrunk by DAG \
         mutations the finished build no longer describes)"
    );

    // 3. The WatchBuild snapshot — what a re-attaching watcher would see
    //    — also reports the settled accounting, not the mutated DAG's.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id: b1,
            caller_tenant: None,
            reply: reply_tx,
        })
        .await?;
    let (_rx, snapshot) = reply_rx.await??;
    let Some(Event::Snapshot(snap)) = snapshot.event else {
        panic!(
            "WatchBuild reply must carry a Snapshot, got {:?}",
            snapshot.event
        );
    };
    assert_eq!(
        snap.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "snapshot reports the settled terminal state"
    );
    assert_eq!(
        snap.cached_derivations, 0,
        "snapshot serves the settled accounting, not the mutated DAG's"
    );
    Ok(())
}

/// bug_282 (the produced-but-unenforced backoff): after a transient
/// failure arms `backoff_until`, an immediate re-pull answers
/// NotYetReady — the kernel's fresh-mint arm holds the window — and
/// the next pull AFTER the window delivers.
///
/// RED (pre-fix): the immediate re-pull answered Deliver — the backoff
/// was set by handle_transient_failure and read by NOTHING in the pull
/// architecture (the dispatch-time defer died with the queue).
#[tokio::test]
async fn transient_retry_not_redispatched_inside_backoff() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        // A real, short window: base 2s, no jitter — long enough that
        // the immediate re-pull is deterministically inside it, short
        // enough to wait out for the post-window half.
        c.retry_policy.backoff_base_secs = 2.0;
        c.retry_policy.backoff_multiplier = 2.0;
        c.retry_policy.backoff_max_secs = 4.0;
        c.retry_policy.jitter_fraction = 0.0;
    });
    let drv = "backoff-gate";
    let _ev = merge_single_node(&handle, Uuid::new_v4(), drv, PriorityClass::Scheduled).await?;

    pull_complete_failure(
        &handle,
        drv,
        rio_proto::types::BuildResultStatus::TransientFailure,
        "flaky",
    )
    .await?;
    barrier(&handle).await;
    let info = expect_drv(&handle, drv).await;
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "requeued, not poisoned"
    );
    assert!(
        info.retry.backoff_until.is_some(),
        "precondition: the transient arm armed the backoff"
    );

    // Inside the window: the fresh mint is held.
    let inside = try_pull_attempt(&handle, drv).await;
    assert!(
        matches!(inside, Ok(PullOutcome::NotYetReady { .. })),
        "a pull inside the backoff window answers NotYetReady \
         (RED pre-fix: Deliver — the window was enforced nowhere); got {inside:?}"
    );

    // Past the window: the mint proceeds (and clears the backoff).
    tokio::time::sleep(Duration::from_millis(2300)).await;
    let after = try_pull_attempt(&handle, drv).await;
    assert!(
        matches!(after, Ok(PullOutcome::Deliver(_))),
        "a pull after the window delivers; got {after:?}"
    );
    Ok(())
}

/// bug_282, the spawn-intent half: a Ready node inside its backoff
/// window is excluded from spawn intents (a pod spawned for it would
/// loop on NotYetReady until the window lapses), and re-enters the
/// candidate set once the window passes.
///
/// RED (pre-fix): the node was emitted — controllers spawned pods
/// against a guaranteed-refusing mint.
#[tokio::test]
async fn spawn_intents_exclude_backoff_window() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: test_sla_config(),
            ..Default::default()
        },
    );
    actor.test_inject_ready("backoff-spawn", None, "x86_64-linux", false);
    actor.test_inject_ready("backoff-free", None, "x86_64-linux", false);
    if let Some(state) = actor.dag.node_mut("backoff-spawn") {
        state.retry.backoff_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
    }

    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(
        snap.intents.len(),
        1,
        "the in-backoff node is not spawn-intent demand; got {:?}",
        snap.intents
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>()
    );
    assert_eq!(snap.intents[0].intent_id, "backoff-free");

    // The window lapses → candidacy restored.
    if let Some(state) = actor.dag.node_mut("backoff-spawn") {
        state.retry.backoff_until =
            Some(std::time::Instant::now() - std::time::Duration::from_secs(1));
    }
    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(
        snap.intents.len(),
        2,
        "an expired window restores candidacy"
    );
    Ok(())
}

// r[verify sched.admin.snapshot-substituting+4]
/// bug_129: `ClusterStatus.queued_by_system` equals
/// `GetSpawnIntents.queued_by_system` BY CONSTRUCTION (both surfaces
/// read the one shared Ready-node classifier), INCLUDING the
/// in-backoff Ready set — retry backoff suppresses spawn-intent
/// EMISSION only, never demand accounting. Pre-fix RED (the bug_282
/// backoff `continue` sat above the spawn-intents aggregate while the
/// snapshot Ready arm had no backoff check): one in-backoff Ready
/// node → ClusterStatus said 1, GetSpawnIntents said 0
/// (`left: Some(1) / right: None`).
#[tokio::test]
async fn queued_by_system_equal_across_both_rpcs_under_backoff() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor.test_inject_ready("129-backoff", None, "x86_64-linux", false);
    if let Some(state) = actor.dag.node_mut("129-backoff") {
        state.retry.backoff_until =
            Some(std::time::Instant::now() + std::time::Duration::from_secs(60));
    }

    let snapshot = actor.compute_cluster_snapshot();
    let intents = actor.compute_spawn_intents(&Default::default());

    // The aggregates agree per system…
    assert_eq!(
        snapshot.queued_by_system.get("x86_64-linux").copied(),
        Some(1),
        "an in-backoff Ready node is still builder-queue demand on ClusterStatus"
    );
    assert_eq!(
        intents.queued_by_system.get("x86_64-linux").copied(),
        Some(1),
        "GetSpawnIntents counts the SAME demand as ClusterStatus"
    );
    // …while the backoff still suppresses the intent (bug_282 kept).
    assert!(
        intents.intents.is_empty(),
        "backoff suppresses intent emission, got {:?}",
        intents
            .intents
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>()
    );
}

/// The sweep-budget law (bug_127), red and green side by side on a
/// paused clock with 8 hung tenants (every probe future pends
/// forever):
///
/// RED (kept as the falsify twin): the pre-budget shape — sequential
/// awaits, each under its own full timeout — pays 8 x 30 s = 240 s.
/// GREEN: `fan_out_probes` under ONE `AttemptBudget` completes the
/// same 8 hung probes in exactly one grpc_timeout (30 s), every
/// outcome `TimedOut` → the dropped-from-fold arm.
// r[verify sched.dispatch.probe-budget]
#[tokio::test(start_paused = true)]
async fn hung_tenant_sweep_is_bounded_by_one_timeout() {
    use crate::actor::dispatch::{ProbeOutcome, fan_out_probes};
    const T: usize = 8;
    let grpc_timeout = std::time::Duration::from_secs(30);

    // RED: the sequential pre-budget shape.
    let t0 = tokio::time::Instant::now();
    for _ in 0..T {
        let hung = std::future::pending::<()>();
        assert!(
            tokio::time::timeout(grpc_timeout, hung).await.is_err(),
            "a hung probe answered — the recorded red would be stale"
        );
    }
    assert_eq!(
        t0.elapsed(),
        grpc_timeout * T as u32,
        "sequential awaits pay T x timeout — the bug_127 shape"
    );

    // GREEN: the budgeted fan-out.
    let budget = rio_common::transport::AttemptBudget::new(grpc_timeout);
    let probes: Vec<(usize, ())> = (0..T).map(|i| (i, ())).collect();
    let t0 = tokio::time::Instant::now();
    let fold = fan_out_probes(probes, &budget, grpc_timeout, |()| {
        std::future::pending::<Result<tonic::Response<()>, tonic::Status>>()
    })
    .await;
    assert_eq!(fold.len(), T);
    assert!(
        fold.iter()
            .all(|(_, o)| matches!(o, ProbeOutcome::TimedOut)),
        "every hung probe must fold as TimedOut (dropped from the fold)"
    );
    assert_eq!(
        t0.elapsed(),
        grpc_timeout,
        "the whole hung sweep costs exactly ONE grpc_timeout"
    );
}

// ---------------------------------------------------------------------------
// Budget expiry is not store-health evidence (merged_bug_179)
// ---------------------------------------------------------------------------

/// merged_bug_179: a sweep-budget expiry short-circuits the probe
/// WITHOUT issuing the RPC -- pre-fix it returned `TimedOut`, and the
/// fold stamped `last_store_rpc_failure` for it, so under multi-tenant
/// load (ceil(T/8) * L > budget on a HEALTHY store) every dispatch
/// pass re-stamped the corroboration gate's store-health OR-leg and
/// permanently satisfied it -- re-opening the single-node
/// store_degraded forgery lane the merged_bug_032 gate closes. The
/// never-issued shape is now its own variant, the probe future is
/// provably never polled, and the exhaustive evidence policy refuses
/// it.
#[tokio::test]
async fn budget_expiry_short_circuit_is_not_store_health_evidence() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let issued = std::sync::Arc::new(AtomicBool::new(false));
    let issued_probe = std::sync::Arc::clone(&issued);
    // A budget that is already spent before any probe's turn comes.
    let budget = rio_common::transport::AttemptBudget::new(std::time::Duration::ZERO);
    let out = crate::actor::dispatch::fan_out_probes(
        vec![("tenant-a", ())],
        &budget,
        std::time::Duration::from_secs(5),
        move |_req: ()| {
            // The flag sets when the RPC future is POLLED (a tonic
            // call future is lazy; creation does not issue).
            let polled = std::sync::Arc::clone(&issued_probe);
            async move {
                polled.store(true, Ordering::SeqCst);
                Ok(tonic::Response::new(()))
            }
        },
    )
    .await;
    assert!(
        !issued.load(Ordering::SeqCst),
        "an expired budget must short-circuit WITHOUT issuing the RPC"
    );
    let (_, outcome) = &out[0];
    let evidence = crate::actor::dispatch::is_store_health_evidence(outcome);
    assert!(
        matches!(outcome, crate::actor::dispatch::ProbeOutcome::BudgetExpired),
        "the never-issued shape must be its own variant, got evidence={evidence}"
    );
    assert!(
        !evidence,
        "left: degraded / right: healthy (a budget-expiry storm must \
         NOT trip the store-degraded corroboration gate)"
    );
}

/// The exhaustive evidence-policy witness: only ISSUED-RPC failures
/// stamp. Adding a `ProbeOutcome` variant breaks this match (and the
/// production one) at compile time.
#[test]
fn store_health_evidence_policy_is_issued_only() {
    use crate::actor::dispatch::{ProbeOutcome, is_store_health_evidence};
    assert!(!is_store_health_evidence::<()>(&ProbeOutcome::Answered(())));
    assert!(is_store_health_evidence::<()>(&ProbeOutcome::Failed(
        tonic::Status::internal("x")
    )));
    assert!(is_store_health_evidence::<()>(&ProbeOutcome::TimedOut));
    assert!(!is_store_health_evidence::<()>(
        &ProbeOutcome::BudgetExpired
    ));
}
