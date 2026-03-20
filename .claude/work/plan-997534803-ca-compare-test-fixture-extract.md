# Plan 997534803: CA-compare test fixture — setup_ca_fixture + complete_ca helpers

consol-mc235 feature. Eight copies of the `spawn_mock_store_with_client` + `setup_actor_with_store` + `connect_worker` + `is_content_addressed=true` + `merge_dag` dance across [`actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) (~12-15L setup per test). [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) added five of these this window (T54, T61×3, T67); three pre-existing. Plus four `is_content_addressed=true` sites in [`actor/tests/dispatch.rs`](../../rio-scheduler/src/actor/tests/dispatch.rs).

| Test | Line | Setup boilerplate |
|---|---|---|
| `ca_cutoff_compare_slow_store` | [`:339`](../../rio-scheduler/src/actor/tests/completion.rs) | spawn_mock_store + setup_actor_with_store + connect_worker + make_test_node(ca=true) + merge_dag |
| `ca_compare_zero_outputs` | [`:414`](../../rio-scheduler/src/actor/tests/completion.rs) | same |
| `ca_compare_malformed_hash` | [`:480`](../../rio-scheduler/src/actor/tests/completion.rs) | same |
| `ca_compare_rpc_err` | [`:534`](../../rio-scheduler/src/actor/tests/completion.rs) | same |
| `cascade_only_skips_verified` | [`:627`](../../rio-scheduler/src/actor/tests/completion.rs) | same + multi-node DAG |
| (3 pre-existing CA tests) | [`:29`, `:245`, `:772`](../../rio-scheduler/src/actor/tests/completion.rs) | same |

Secondary: [`complete_success()` at helpers.rs:243](../../rio-scheduler/src/actor/tests/helpers.rs) hardcodes `vec![0u8;32]` output hash. CA tests need hash control — `[0xAB;32]` valid hash, `[0xCD;16]` malformed (wrong length), or a store-seeded real hash for compare-match scenarios.

**Extraction:** `setup_ca_fixture(key: &str) -> CaFixture` in [`helpers.rs`](../../rio-scheduler/src/actor/tests/helpers.rs). Struct holds `MockStoreHandle` (for fault-flag arming), `ActorHandle`, `TestDb`, drv path `String`, `Receiver<Assignment>`. Absorbs: `TestDb::new` + `spawn_mock_store_with_client` + `setup_actor_with_store` + `connect_worker` + `make_test_node(is_content_addressed=true)` + `merge_dag` single-node. Each test drops from ~15L setup to 1-2L.

Plus `complete_ca(handle, worker, drv, outputs: &[(name, path, hash)])` — like `complete_success` but caller controls per-output hash bytes (for malformed/mismatch scenarios).

~100L net saved; each future `sched.ca.*` test-gap plan adds one line not fifteen. Next `r[sched.ca.*]` coverage expansion (there are 5 markers, only 2 have ≥3 verify sites) adds the 9th copy otherwise.

## Entry criteria

- [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) merged (**DONE** — the 5 CA-compare tests at `:339-:627` exist)

## Tasks

### T1 — `refactor(scheduler):` CaFixture struct + setup_ca_fixture helper

MODIFY [`rio-scheduler/src/actor/tests/helpers.rs`](../../rio-scheduler/src/actor/tests/helpers.rs). Add after [`setup_actor_with_store` at `:26`](../../rio-scheduler/src/actor/tests/helpers.rs):

```rust
/// Bundle of handles for CA-compare test scenarios. See `setup_ca_fixture`.
pub(crate) struct CaFixture {
    /// MockStore handle — arm fault flags (`content_lookup_hang`,
    /// `fail_content_lookup`, etc.) for timeout/error-path tests.
    pub store: rio_test_support::grpc::MockStoreHandle,
    /// Actor handle — send commands, await replies.
    pub actor: ActorHandle,
    /// Connected worker's assignment receiver — `rx.recv()` to get the
    /// dispatched assignment after `merge_dag` sends the node to ready.
    pub rx: tokio::sync::mpsc::Receiver<rio_proto::scheduler::Assignment>,
    /// The single CA derivation's path (`test_drv_path(key)`).
    pub drv_path: String,
    /// PG test database — keep alive for the actor's pool.
    pub _db: rio_test_support::pg::TestDb,
    /// MockStore tokio task guard — keep alive for the gRPC server.
    pub _store_task: tokio::task::JoinHandle<()>,
}

/// Standard CA-compare test setup: spawn MockStore, actor with store
/// client, connect a worker, merge a single `is_content_addressed=true`
/// node, return all handles.
///
/// Absorbs the 8-copy boilerplate: TestDb::new + spawn_mock_store_with_client
/// + setup_actor_with_store + connect_worker + make_test_node(ca=true) +
/// merge_dag. 5 of the 8 copies landed with P0311 (ca_cutoff_compare_slow_store
/// + T61×3 + T67); 3 pre-existing. Each future sched.ca.* test would add a
/// 9th+ without this helper (P997534803 consolidation).
///
/// Returns `CaFixture` with the MockStoreHandle exposed so tests can arm
/// fault flags (content_lookup_hang for timeout, fail_content_lookup for
/// Err path, etc.) BEFORE driving the actor to the CA-compare callsite.
pub(crate) async fn setup_ca_fixture(
    key: &str,
) -> anyhow::Result<CaFixture> {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    let db = rio_test_support::pg::TestDb::new().await?;
    let (store, store_client, store_task) = spawn_mock_store_with_client().await?;
    let (actor, _gen) = setup_actor_with_store(db.pool(), Some(store_client));
    let rx = connect_worker(&actor, &format!("w-{key}"), "x86_64-linux", 4).await?;
    let mut node = make_test_node(key, "x86_64-linux");
    node.is_content_addressed = true;
    let drv_path = node.derivation.clone();
    merge_dag(&actor, vec![node], vec![]).await?;
    Ok(CaFixture { store, actor, rx, drv_path, _db: db, _store_task: store_task })
}
```

### T2 — `refactor(scheduler):` complete_ca — hash-controllable completion helper

MODIFY [`rio-scheduler/src/actor/tests/helpers.rs`](../../rio-scheduler/src/actor/tests/helpers.rs). Add after [`complete_success` at `:243`](../../rio-scheduler/src/actor/tests/helpers.rs):

```rust
/// Like `complete_success` but caller controls per-output hash bytes.
///
/// CA-compare tests need specific hash values: `[0xAB;32]` for a valid
/// hash the MockStore can be seeded to match, `[0xCD;16]` for a malformed
/// length that triggers the 32-byte guard at completion.rs:295-304, or
/// a store-seeded real hash for compare-match scenarios.
///
/// `complete_success` hardcodes `vec![0u8;32]` — fine for IA tests where
/// the hash is opaque, wrong for CA tests where the hash IS the test
/// subject (P997534803 extraction).
pub(crate) async fn complete_ca(
    handle: &ActorHandle,
    worker: &str,
    drv: &str,
    outputs: &[(&str, &str, Vec<u8>)],  // (name, path, hash)
) -> anyhow::Result<()> {
    // ... mirrors complete_success body but uses caller-provided outputs
    // instead of synthesizing a single default output with [0u8;32] hash.
    // Grep complete_success at :243 for the ActorCommand::BuildResult
    // construction — copy that shape, replace the outputs field.
    todo!("implement at dispatch — mirror complete_success with outputs param")
}
```

(Concrete body left to implementer — mirror `complete_success`'s `ActorCommand::BuildResult` construction but substitute the outputs vec. ~20L.)

### T3 — `refactor(scheduler):` migrate 8 completion.rs CA tests to setup_ca_fixture

MODIFY [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs). For each of the 8 tests (lines `:29`, `:245`, `:339`, `:414`, `:480`, `:534`, `:627`, `:772` — re-grep at dispatch for exact), replace the ~12-15L setup block with:

```rust
let f = setup_ca_fixture("test-key").await?;
// Optional: arm fault flag BEFORE driving the actor
f.store.content_lookup_hang.store(true, SeqCst);
// ...
```

Tests that need multi-node DAGs (`cascade_only_skips_verified` at `:627`) may need a `setup_ca_fixture_graph(nodes, edges)` variant OR post-`setup_ca_fixture` manual `merge_dag` call — the helper handles single-node; multi-node tests can call it for the first node then `merge_dag` additional nodes. Decide at dispatch based on how many multi-node CA tests exist.

~100L net removed across the 8 tests.

### T4 — `test(scheduler):` regression guard — fixture preserves fault-flag timing

ADD a test that proves the fixture's ordering is correct: fault flag armed BEFORE the actor reaches the CA-compare callsite. The risk is that `setup_ca_fixture` eagerly drives the actor past the compare (if `merge_dag` triggers an immediate dispatch → worker receives assignment → BuildResult → CA-compare fires before the test body arms the flag).

```rust
#[tokio::test]
async fn setup_ca_fixture_does_not_race_past_ca_compare() -> anyhow::Result<()> {
    // Precondition: after setup_ca_fixture returns, the CA-compare callsite
    // has NOT fired — arming a fault flag still affects the first compare.
    //
    // Proof: arm content_lookup_hang AFTER setup, drive to completion,
    // observe the hang. If the compare already fired before we armed,
    // the test passes instantly (hang never observed) — that would prove
    // the fixture races past the callsite.
    let f = setup_ca_fixture("race-guard").await?;
    f.store.content_lookup_hang.store(true, SeqCst);
    let assignment = f.rx.recv().await.expect("assignment dispatched");
    // ... complete the build, observe the timeout/hang at the CA-compare
    // callsite. If this returns quickly, the fixture raced past compare.
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        complete_success(&f.actor, "w-race-guard", &f.drv_path),
    ).await;
    assert!(result.is_err() || /* hung at compare */, "fixture raced past CA-compare — flag armed too late");
    Ok(())
}
```

This is a self-check on the fixture itself — proves `setup_ca_fixture` returns with the actor waiting for `BuildResult`, not past it. If `merge_dag` → auto-dispatch → auto-compare without a `BuildResult`, the fixture is wrong by construction and this test catches it.

## Exit criteria

- `/nixbuild .#ci` green
- `grep 'setup_ca_fixture\|CaFixture\|complete_ca' rio-scheduler/src/actor/tests/helpers.rs` → ≥6 hits (T1+T2: struct + fn + doc-comments)
- `grep -c 'spawn_mock_store_with_client' rio-scheduler/src/actor/tests/completion.rs` → ≤3 (T3: 8 sites → 0-3 residual; multi-node tests may still inline)
- `grep -c 'setup_ca_fixture' rio-scheduler/src/actor/tests/completion.rs` → ≥5 (T3: at least the 5 P0311-added single-node tests migrated)
- `wc -l rio-scheduler/src/actor/tests/completion.rs` → pre-minus-~80L (T3: ~12L×8 removed, ~2L×8 added, net ≈-80L; actual depends on multi-node residuals)
- `cargo nextest run -p rio-scheduler 'actor::tests::completion'` → all pass (T3: no test behavior change)
- `cargo nextest run -p rio-scheduler setup_ca_fixture_does_not_race` → 1 passed (T4)
- `nix develop -c tracey query rule sched.ca.cutoff-compare` → same verify-site count as before (T3 migration doesn't drop `r[verify]` annotations)

## Tracey

References existing markers (no new markers; test-helper extraction):
- `r[sched.ca.cutoff-compare]` — T3 migrates the tests that verify this marker. The `r[verify sched.ca.cutoff-compare]` annotations at [`completion.rs:339+`](../../rio-scheduler/src/actor/tests/completion.rs) stay on the migrated tests.
- `r[sched.ca.cutoff-propagate]` — `cascade_only_skips_verified` at `:627` verifies this; T3 migration preserves its `r[verify]`.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "T1: +CaFixture struct + setup_ca_fixture() after :26. T2: +complete_ca() after :243 (hash-controllable completion)"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "T3: migrate 8 CA tests (~:29,:245,:339,:414,:480,:534,:627,:772) to setup_ca_fixture — ~12L→2L each, net -80L. T4: +setup_ca_fixture_does_not_race_past_ca_compare regression guard"}
]
```

```
rio-scheduler/src/actor/tests/
├── helpers.rs       # T1+T2: CaFixture + setup_ca_fixture + complete_ca
└── completion.rs    # T3+T4: 8 migrations + race-guard test
```

## Dependencies

```json deps
{"deps": [311], "soft_deps": [419, 251, 393, 304], "note": "HARD-dep P0311 (DONE — the 5 CA-compare tests at completion.rs:339-627 exist; discovered_from=consolidator). Soft-dep P0419 (grpc_timeout plumbing — touches completion.rs:339's ca_cutoff_compare_slow_store for the 30s→3s shrink. That plan edits the test's outer timeout guard; this plan replaces the test's setup block. Non-overlapping edits in the same test fn — P0419 at :384-389 outer guard, T3 at :339-356 setup block. Rebase-clean either order but sequence P0419 FIRST so T3 migrates the already-shortened test). Soft-dep P0251 (DONE — CA-compare hook at completion.rs:289-337 exists; the tests being migrated verify it). Soft-dep P0393 (ContentLookup short-circuit — may change the CA-compare callsite count; T3's migrated tests assert on post-P0393 behavior if it landed). Soft-dep P0304-T148/T157/T194 (same completion.rs file — T148/T157 add counter labels at :329, T194 rewrites line-cites; all non-overlapping with T3's setup-block replacements). COLLISION: completion.rs is count=32 HOT (2nd highest in actor/). T3 is a bulk edit touching 8 fn bodies. Check at dispatch: grep in-flight plans for completion.rs touchers, sequence accordingly. The setup-block replacement is at the TOP of each test fn (first ~15L after the fn sig) — most other plans edit the assertion tail (counter checks, r[verify] annotations). Low-overlap risk despite high-touch-count."}
```

**Depends on:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) — the 5 P0311-added CA tests at `:339-:627` are the primary extraction subjects.

**Conflicts with:** [`rio-scheduler/src/actor/tests/completion.rs`](../../rio-scheduler/src/actor/tests/completion.rs) count=32 HOT — [P0419](plan-0419-ca-compare-slow-store-test-timeout.md) edits the outer timeout guard of `ca_cutoff_compare_slow_store` at `:384-389`; T3 edits that same test's setup block at `:339-356`. Different regions of same fn. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T148/T157/T194 edit counter labels + line-cites at `:329+`; non-overlapping with T3's setup blocks. [`rio-scheduler/src/actor/tests/helpers.rs`](../../rio-scheduler/src/actor/tests/helpers.rs) low-traffic (count ≈5) — T1+T2 are additive after `:26` and `:243`.
