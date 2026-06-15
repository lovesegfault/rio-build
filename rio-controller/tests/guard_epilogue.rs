//! W10-AS / W10-AU: the guard runtime drains its lease epilogue
//! (bug_118, R27 BANNER) and every guard-runtime spawn site carries a
//! typed join obligation (the cross-domain census's task-lifetime
//! axis).
//!
//! Proposition (W10-AS, the law's own quantifier): on shutdown
//! cancellation, an epilogue hosted on the guard runtime — the lease
//! loop's graceful-release `step_down()` PATCH — EXECUTES before the
//! guard runtime drops. The witness is structural, not wall-clock: the
//! lease RECORD at the (mock) apiserver shows the release. Pre-fix the
//! hosting wiring discarded the lease-loop JoinHandle and keyed the
//! guard thread's `block_on` root to the same cancellation token as
//! its tasks, so the runtime dropped the in-flight PATCH: the record
//! kept naming the dead pod and the successor waited out the full
//! STEAL_AFTER (~19s) instead of one 5s tick.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Duration;

use rio_test_support::kube_mock::MockApiServer;

/// EVERY `.rs` under `rio-controller/src`, embedded at compile time
/// (the (wwwww) embed form — the nix gate runs test binaries without
/// the source tree on disk, so a runtime walk is premise-unreachable
/// exactly where it gates). Machine-generated — sorted
/// (relpath, include_str!) pairs; regenerate with the python walk
/// recorded in the owning commit body. The completeness pin
/// (`join_census_universe_matches_live_tree`) forces this table to
/// track the live tree exactly in both directions.
#[rustfmt::skip]
const JOIN_CENSUS_SOURCES: &[(&str, &str)] = &[
    ("config.rs", include_str!("../src/config.rs")),
    ("error.rs", include_str!("../src/error.rs")),
    ("fixtures.rs", include_str!("../src/fixtures.rs")),
    ("guard.rs", include_str!("../src/guard.rs")),
    ("lib.rs", include_str!("../src/lib.rs")),
    ("main.rs", include_str!("../src/main.rs")),
    ("observability.rs", include_str!("../src/observability.rs")),
    ("reconcilers/componentscaler/decide.rs", include_str!("../src/reconcilers/componentscaler/decide.rs")),
    ("reconcilers/componentscaler/mod.rs", include_str!("../src/reconcilers/componentscaler/mod.rs")),
    ("reconcilers/fence.rs", include_str!("../src/reconcilers/fence.rs")),
    ("reconcilers/gc_schedule.rs", include_str!("../src/reconcilers/gc_schedule.rs")),
    ("reconcilers/mod.rs", include_str!("../src/reconcilers/mod.rs")),
    ("reconcilers/node_informer.rs", include_str!("../src/reconcilers/node_informer.rs")),
    ("reconcilers/nodeclaim_pool/consolidate.rs", include_str!("../src/reconcilers/nodeclaim_pool/consolidate.rs")),
    ("reconcilers/nodeclaim_pool/cover.rs", include_str!("../src/reconcilers/nodeclaim_pool/cover.rs")),
    ("reconcilers/nodeclaim_pool/evidence.rs", include_str!("../src/reconcilers/nodeclaim_pool/evidence.rs")),
    ("reconcilers/nodeclaim_pool/ffd.rs", include_str!("../src/reconcilers/nodeclaim_pool/ffd.rs")),
    ("reconcilers/nodeclaim_pool/health.rs", include_str!("../src/reconcilers/nodeclaim_pool/health.rs")),
    ("reconcilers/nodeclaim_pool/lifecycle_tests.rs", include_str!("../src/reconcilers/nodeclaim_pool/lifecycle_tests.rs")),
    ("reconcilers/nodeclaim_pool/mod.rs", include_str!("../src/reconcilers/nodeclaim_pool/mod.rs")),
    ("reconcilers/nodeclaim_pool/pods.rs", include_str!("../src/reconcilers/nodeclaim_pool/pods.rs")),
    ("reconcilers/nodeclaim_pool/sketch.rs", include_str!("../src/reconcilers/nodeclaim_pool/sketch.rs")),
    ("reconcilers/nodeclaim_pool/wedge.rs", include_str!("../src/reconcilers/nodeclaim_pool/wedge.rs")),
    ("reconcilers/pool/candidate.rs", include_str!("../src/reconcilers/pool/candidate.rs")),
    ("reconcilers/pool/disruption.rs", include_str!("../src/reconcilers/pool/disruption.rs")),
    ("reconcilers/pool/job.rs", include_str!("../src/reconcilers/pool/job.rs")),
    ("reconcilers/pool/jobs.rs", include_str!("../src/reconcilers/pool/jobs.rs")),
    ("reconcilers/pool/mod.rs", include_str!("../src/reconcilers/pool/mod.rs")),
    ("reconcilers/pool/pod.rs", include_str!("../src/reconcilers/pool/pod.rs")),
    ("reconcilers/pool/tests/builders_tests.rs", include_str!("../src/reconcilers/pool/tests/builders_tests.rs")),
    ("reconcilers/pool/tests/disruption_tests.rs", include_str!("../src/reconcilers/pool/tests/disruption_tests.rs")),
    ("reconcilers/pool/tests/jobs_tests.rs", include_str!("../src/reconcilers/pool/tests/jobs_tests.rs")),
    ("reconcilers/pool/tests/mod.rs", include_str!("../src/reconcilers/pool/tests/mod.rs")),
];

/// The closed `drain-census:` disposition vocabulary. The fourth class
/// (process-joined) is the bug_023 lifetime: the guard THREAD's
/// `JoinHandle` is captured in a `GuardJoin` and the process owner
/// `.join()`s it after the working domain drains — the
/// §Verifier-one-step-removed(b) recurrence of bug_118 one lifecycle
/// level up (runtime→task drain was fixed; process→thread join was
/// not).
const DRAIN_DISPOSITIONS: [&str; 3] = ["runtime-lifetime", "caller-joined", "process-joined"];

/// The guard spawn family (W10-AU population, enumerated from spawn
/// sites — R15): method-call forms reachable anywhere in the crate,
/// plus the raw forms only constructible inside `guard.rs` (the
/// `guard_rt` field is private; bare `tokio::spawn` inside `guard.rs`
/// lands on the guard runtime via the root's `block_on` context).
const GUARD_SPAWN_FORMS: [&str; 3] = [
    ".spawn_lease(",
    ".spawn_lease_with_client(",
    ".spawn_on_guard(",
];
const GUARD_RAW_FORMS: [&str; 2] = ["guard_rt.spawn(", "tokio::spawn("];

/// OS-thread spawn forms (the process↔thread join axis; bug_023): a
/// thread hosting the guard runtime owes a PROCESS-level join,
/// discharged structurally by capture into a `GuardJoin {` constructor
/// later in the same file — the linear join token `guard::spawn`
/// returns to the process owner. A discarded `std::thread::JoinHandle`
/// is exactly the bug_023 shape: `main()` returns and process exit
/// kills the detached thread mid-`step_down()` PATCH.
const OS_THREAD_FORMS: [&str; 2] = ["std::thread::Builder::new(", "std::thread::spawn("];

/// The `guard::spawn` callsite forms (the process owner's side of the
/// process↔thread axis; bug_023): every callsite MUST tuple-bind the
/// returned `GuardJoin` AND `.join()` it later in the same file.
/// `#[must_use]` alone does NOT catch `let (g, _) = …` — verified on
/// stable; `_` suppresses the lint — so this census is the static
/// enforcement and the `GuardJoin` Drop-panic is the runtime one.
const PROCESS_JOIN_FORMS: [&str; 3] = [
    "rio_controller::guard::spawn(",
    "crate::guard::spawn(",
    "guard::spawn(",
];

/// Parse the second binding name out of `let (X, Y) = …` on a
/// `guard::spawn` site line. None when the line is not tuple-bound.
fn process_join_binding(line: &str) -> Option<String> {
    let after = line.split_once("let (")?.1;
    let inside = after.split_once(')')?.0;
    let second = inside.split(',').nth(1)?.trim();
    (!second.is_empty() && second.chars().all(|c| c.is_alphanumeric() || c == '_'))
        .then(|| second.to_string())
}

/// One join-obligation violation: a guard-runtime spawn site whose
/// window carries neither a structural discharge (`adopt_epilogue(` /
/// `DrainHandle {` capture) nor a closed-vocabulary `drain-census:`
/// disposition tag.
fn join_obligation_violations(rel_path: &str, src: &str) -> Vec<String> {
    // Test modules are still scanned (strict direction): a cfg(test)
    // spawn site discharges with a tag like any other. Path-level test
    // files are production-excluded by the SAME behavior pin the
    // timeout census carries.
    if rel_path.contains("tests") {
        return Vec::new();
    }
    let lines: Vec<&str> = src.lines().collect();
    let mut violations = Vec::new();
    for (idx, line) in lines.iter().enumerate() {
        let code = line.trim_start();
        if code.starts_with("//") {
            continue; // comment lines are not spawn sites
        }
        // Process↔thread axis, OS-thread side (bug_023): the
        // `std::thread` JoinHandle MUST flow into a `GuardJoin {`
        // constructor later in the same file — the linear join token.
        if OS_THREAD_FORMS.iter().any(|f| line.contains(f)) {
            let captured = lines[idx + 1..].iter().any(|l| l.contains("GuardJoin {"));
            if !captured {
                violations.push(format!(
                    "{rel_path}:{}: OS-thread spawn site with no join obligation — \
                     capture the JoinHandle in a GuardJoin and return it to the process owner",
                    idx + 1
                ));
            }
            continue;
        }
        // Process↔thread axis, process-owner side (bug_023): every
        // `guard::spawn(` callsite tuple-binds the GuardJoin AND
        // `.join()`s it later in the same file. `_` is NOT a
        // discharge (the must_use suppression the census exists to
        // close).
        if PROCESS_JOIN_FORMS.iter().any(|f| line.contains(f)) {
            let discharged = process_join_binding(line)
                .filter(|b| b != "_")
                .is_some_and(|b| {
                    let joined = format!("{b}.join()");
                    lines[idx + 1..].iter().any(|l| l.contains(&joined))
                });
            if !discharged {
                violations.push(format!(
                    "{rel_path}:{}: guard::spawn callsite with no process-level join — \
                     tuple-bind `(handle, guard_join)` and `guard_join.join()` before \
                     the process owner returns",
                    idx + 1
                ));
            }
            continue;
        }
        let is_site = GUARD_SPAWN_FORMS.iter().any(|f| line.contains(f))
            || (rel_path == "guard.rs" && GUARD_RAW_FORMS.iter().any(|f| line.contains(f)));
        if !is_site {
            continue;
        }
        // Definition lines are not callsites.
        if code.contains("fn spawn_lease") || code.contains("fn spawn_on_guard") {
            continue;
        }
        // The obligation window: the site line plus three lines above
        // (statement head for rustfmt-wrapped adoption; tag lines).
        let window_start = idx.saturating_sub(3);
        let window = &lines[window_start..=idx];
        let discharged_structurally = window
            .iter()
            .any(|l| l.contains("adopt_epilogue(") || l.contains("DrainHandle {"));
        let tag = window.iter().find_map(|l| {
            l.split("drain-census:").nth(1).map(|rest| {
                rest.trim()
                    .split([' ', '\u{2014}'])
                    .next()
                    .unwrap_or("")
                    .to_string()
            })
        });
        match (discharged_structurally, tag) {
            (true, _) => {}
            (false, Some(t)) if DRAIN_DISPOSITIONS.contains(&t.as_str()) => {}
            (false, Some(t)) => violations.push(format!(
                "{rel_path}:{}: drain-census disposition `{t}` outside the closed vocabulary {DRAIN_DISPOSITIONS:?}",
                idx + 1
            )),
            (false, None) => violations.push(format!(
                "{rel_path}:{}: guard-runtime spawn site with no join obligation — adopt the \
                 DrainHandle (adopt_epilogue) or declare `drain-census: <disposition>`",
                idx + 1
            )),
        }
    }
    violations
}

/// No-op hooks: the drain law under test is the HOSTING wiring's, not
/// the consumer edge table's (rio-lease's own tests pin hook
/// delivery).
#[derive(Clone)]
struct NoopHooks;
impl rio_lease::LeaseHooks for NoopHooks {
    fn on_acquire(&self) {}
    fn on_lose(&self) {}
    fn on_rebound(&self) {}
}

/// Bounded event poll: structural assertions wait for the EVENT, never
/// measure the duration. Returns whether `cond` became true within
/// `budget`.
async fn became_true(mut cond: impl FnMut() -> bool, budget: Duration) -> bool {
    let deadline = tokio::time::Instant::now() + budget;
    while tokio::time::Instant::now() < deadline {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cond()
}

/// W10-AU: the join-obligation census over the live tree — every
/// guard-runtime spawn site discharges its obligation.
// r[verify sys.epilogue.drain]
#[test]
fn w10_au_every_guard_spawn_site_carries_a_join_obligation() {
    let mut violations = Vec::new();
    for (rel, src) in JOIN_CENSUS_SOURCES {
        violations.extend(join_obligation_violations(rel, src));
    }
    assert!(
        violations.is_empty(),
        "guard-runtime spawn sites with undischarged join obligations:\n{}",
        violations.join("\n")
    );
}

/// R22′ plants — each evasion axis enters at the OUTERMOST layer (the
/// raw-source scan), red-first: the scanner must flag the strawman
/// forms and pass the discharged ones.
#[test]
fn w10_au_planted_reds_caught() {
    // Axis 1: the discarded handle (THE bug_118 shape).
    let discarded = "fn wire(guard: &GuardHandle) {\n    let _ = guard.spawn_lease(cfg, state, hooks, shutdown);\n}\n";
    assert_eq!(
        join_obligation_violations("plant.rs", discarded).len(),
        1,
        "the discarded-handle plant must red"
    );
    // Axis 2: out-of-vocabulary disposition.
    let bad_vocab = "// drain-census: detached\ntokio::spawn(noise());\n";
    assert_eq!(
        join_obligation_violations("guard.rs", bad_vocab).len(),
        1,
        "the out-of-vocabulary plant must red"
    );
    // Axis 3 (green twins — the discharged forms pass): structural
    // adoption across a rustfmt wrap, and a tagged runtime-lifetime
    // spawn.
    let adopted = "guard.adopt_epilogue(guard.spawn_lease(\n    cfg,\n    state,\n));\n";
    assert!(join_obligation_violations("plant.rs", adopted).is_empty());
    let tagged =
        "// drain-census: runtime-lifetime — measurement loop\ntokio::spawn(sentinel());\n";
    assert!(join_obligation_violations("guard.rs", tagged).is_empty());
    // Axis 4: raw guard_rt spawn outside a DrainHandle capture.
    let raw = "fn leak(rt: &Handle) {\n    self.guard_rt.spawn(fut);\n}\n";
    assert_eq!(
        join_obligation_violations("guard.rs", raw).len(),
        1,
        "the uncaptured raw-spawn plant must red"
    );
    // Axis 5 (bug_023, OS-thread side): the discarded
    // `std::thread::JoinHandle` — THE bug_023 shape at guard.rs:348.
    let os_detached =
        "std::thread::Builder::new().name(n).spawn(move || rt.block_on(f)).expect(e);\n";
    assert_eq!(
        join_obligation_violations("guard.rs", os_detached).len(),
        1,
        "the detached-OS-thread plant must red"
    );
    // Axis 5 green twin: the JoinHandle flows into a GuardJoin.
    let os_captured = "let t = std::thread::Builder::new().spawn(f).expect(e);\n(h, GuardJoin { thread: Some(t) })\n";
    assert!(join_obligation_violations("guard.rs", os_captured).is_empty());
    // Axis 6 (bug_023, process-owner side): `_`-suppressed GuardJoin —
    // the must_use evasion the census exists to close.
    let proc_suppressed = "let (g, _) = rio_controller::guard::spawn(h, r, c, s);\n";
    assert_eq!(
        join_obligation_violations("main.rs", proc_suppressed).len(),
        1,
        "the `_`-suppressed GuardJoin plant must red"
    );
    // Axis 6 green twin: tuple-bound AND joined later in the file.
    let proc_joined = "let (g, gj) = crate::guard::spawn(h, r, c, s);\ngj.join();\n";
    assert!(join_obligation_violations("main.rs", proc_joined).is_empty());
}

/// The (wwwww) bidirectional completeness pin: the embedded universe
/// equals the live `src/` tree (both directions). Skipped — disclosed,
/// never silent — where the source tree is absent (the nix sandbox).
#[test]
fn join_census_universe_matches_live_tree() {
    let src_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    if !src_root.is_dir() {
        eprintln!(
            "join_census_universe_matches_live_tree: src/ absent (sandbox) — \
             pin SKIPPED (the dev-tree run is the enforcing one)"
        );
        return;
    }
    let mut live = Vec::new();
    let mut stack = vec![src_root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read_dir under src/") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                live.push(
                    path.strip_prefix(&src_root)
                        .expect("under src root")
                        .to_str()
                        .expect("crate-relative source paths are UTF-8")
                        .replace(std::path::MAIN_SEPARATOR, "/"),
                );
            }
        }
    }
    live.sort();
    let embedded: Vec<String> = JOIN_CENSUS_SOURCES
        .iter()
        .map(|(r, _)| (*r).to_string())
        .collect();
    assert_eq!(
        embedded, live,
        "JOIN_CENSUS_SOURCES is stale against the live tree — regenerate the \
         include_str! table (the python walk in the owning commit body)"
    );
}

/// W10-AS (bug_023, the runtime enforcement face): a `GuardJoin`
/// dropped without `.join()` panics. The §Verifier-one-step-removed(b)
/// recurrence one lifecycle level up — `#[must_use]` cannot catch
/// `let (g, _) = …`, so the linear token's Drop is the runtime gate.
#[test]
#[should_panic(expected = "bug_023")]
fn dropped_guard_join_panics() {
    let shutdown = rio_common::signal::Token::new();
    let main = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .expect("main runtime");
    // The `_` suppression is THE evasion the Drop-panic exists to
    // close — axis-6 plant, here driven against the real spawn.
    #[allow(clippy::no_effect_underscore_binding)]
    let (_g, _) = rio_controller::guard::spawn(
        main.handle().clone(),
        Arc::new(AtomicBool::new(true)),
        rio_controller::guard::GuardConfig {
            health_addr: ([127, 0, 0, 1], 0).into(),
            ..Default::default()
        },
        shutdown.clone(),
    );
    shutdown.cancel();
    // GuardJoin drops here → panic("… bug_023 …").
}

// r[verify sys.epilogue.drain]
#[tokio::test(flavor = "multi_thread")]
async fn w10_as_process_join_gates_holder_clear() {
    let (client, mock) = MockApiServer::new();
    let shutdown = rio_common::signal::Token::new();
    let (guard, guard_join) = rio_controller::guard::spawn(
        tokio::runtime::Handle::current(),
        Arc::new(AtomicBool::new(true)),
        rio_controller::guard::GuardConfig {
            health_addr: ([127, 0, 0, 1], 0).into(),
            ..Default::default()
        },
        shutdown.clone(),
    );

    let state = rio_lease::LeaderState::pending(Arc::new(AtomicU64::new(1)));
    let cfg = rio_lease::LeaseConfig {
        lease_name: "guard-epilogue".into(),
        namespace: "default".into(),
        holder_id: "incumbent".into(),
        leader_pod_label: None,
    };
    guard.adopt_epilogue(guard.spawn_lease_with_client(
        client,
        cfg,
        state.clone(),
        NoopHooks,
        shutdown.clone(),
    ));

    // First election round: the loop Creates the lease and acquires.
    assert!(
        became_true(
            || mock.holder().as_deref() == Some("incumbent"),
            Duration::from_secs(10)
        )
        .await,
        "the lease loop must acquire against the healthy mock (holder = incumbent)"
    );
    assert!(state.is_leader(), "acquisition must flip is_leader");

    // SIGTERM mid-tenure: the only pending duty is the shutdown
    // epilogue — the graceful-release step_down() PATCH.
    shutdown.cancel();

    // The law (sys.epilogue.drain) at the PROCESS level (bug_023):
    // `GuardJoin::join()` returns only after the guard root's drain
    // state completes — the lease record shows the release
    // SYNCHRONOUSLY at the join, no polling. Pre-fix the W10-AS test
    // polled `holder().is_none()` from a tokio runtime that stayed
    // alive — it verified the drain protocol, not that the process
    // owner WAITS for it. spawn_blocking so the multi_thread runtime
    // keeps serving the mock apiserver while the rio-guard thread
    // drains.
    tokio::task::spawn_blocking(move || guard_join.join())
        .await
        .expect("the rio-guard thread panicked during the epilogue drain");
    assert_eq!(
        mock.holder(),
        None,
        "GuardJoin::join() returned before the shutdown release landed — \
         the process owner would exit with the lease still naming the dead pod \
         (the bug_023 ~19s STEAL_AFTER tax)"
    );
}
