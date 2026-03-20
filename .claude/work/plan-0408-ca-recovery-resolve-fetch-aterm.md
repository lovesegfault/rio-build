# Plan 0408: CA recovery-resolve — fetch ATerm from store when drv_content empty

rev-p254 feature at [`rio-scheduler/src/actor/dispatch.rs:661-665`](../../rio-scheduler/src/actor/dispatch.rs). [P0254](plan-0254-ca-metrics-vm-demo.md) T8 picked option-(a) defer-further: re-tag `TODO(P0254)` → `TODO(P0NNN)` pointing at a future plan that fetches the ATerm from the store when `drv_content` is empty on a recovered floating-CA dispatch. Per [CLAUDE.md §Deferred work](../../CLAUDE.md): every TODO must be `TODO(P0NNN)` — a sentinel string (`TODO(recovery-resolve)` or `TODO(P0254)` pointing at a DONE plan) is an orphan.

**The deferred work:** after scheduler restart (leadership acquisition, recovery from PostgreSQL), the DAG is reloaded from `derivations` + `build_derivations` tables, but `drv_content` is NOT persisted (too large to store for every derivation; the common case is `drv_content` inline in the gateway's `SubmitBuild` and held in-memory). The guard at `:663-665` returns `state.drv_content.clone()` (empty bytes) when `drv_content.is_empty()` — which means `maybe_resolve_ca` is a no-op for recovered floating-CA derivations, even when their CA inputs are fully resolvable. The worker then receives an unresolved `.drv` with placeholder paths, fails the build, the retry eventually succeeds once the scheduler re-receives a fresh submission with inline `drv_content`.

**The fix:** `GetPath(drv_path)` RPC against the store fetches the ATerm bytes (~1-50KB, ~10-50ms round-trip). Derivations are always uploaded to the store before dispatch (that's how workers fetch them when `drv_content` is empty — see [`build_types.proto:231`](../../rio-proto/proto/build_types.proto) comment). The scheduler can do the same fetch at resolve-time.

Edge case of an edge case: requires (1) scheduler restart mid-build, (2) the build includes a floating-CA derivation, (3) that derivation has floating-CA INPUTS (CA-on-CA chain). All three conditions together: rare. But the failure mode is ugly (worker-fail-retry-self-heal, wasted build + confusing error log). And the fetch is cheap (~10-50ms once per recovered CA-on-CA dispatch).

## Entry criteria

- [P0254](plan-0254-ca-metrics-vm-demo.md) merged (T5-T7 wire `ca_modular_hash` + `collect_ca_inputs` + `pending_realisation_deps` — resolve machinery is functional, not stubbed; T8 re-tagged the TODO)

## Tasks

### T1 — `fix(scheduler):` dispatch.rs — fetch drv_content from store on empty (recovered case)

MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) at `:658-665` (post-P0254 line refs; re-grep at dispatch for `drv_content.is_empty()`). Replace the early-return-empty with a store fetch:

```rust
// No drv_content → recovered derivation (scheduler restart, DAG
// reloaded from PG, drv_content not persisted). The store has the
// ATerm — fetch it. Workers do the same (build_types.proto:231:
// "Empty = fallback; worker fetches via GetPath"). ~10-50ms
// round-trip, once per recovered CA-on-CA dispatch.
let drv_content = if state.drv_content.is_empty() {
    match self.fetch_drv_content_from_store(drv_hash, state).await {
        Some(bytes) => bytes,
        None => {
            // Store unreachable or drv not found — dispatch
            // unresolved (worker fails on placeholder, self-heals
            // via retry after resubmit). Same degrade as before.
            warn!(
                drv_hash = %drv_hash,
                "recovered CA dispatch: drv_content empty + store fetch failed; dispatching unresolved"
            );
            return state.drv_content.clone();
        }
    }
} else {
    state.drv_content.clone()
};
```

The `fetch_drv_content_from_store` helper (new, private to `dispatch.rs` or in `actor/mod.rs`):

```rust
async fn fetch_drv_content_from_store(
    &self,
    drv_hash: &DrvHash,
    state: &DerivationState,
) -> Option<Vec<u8>> {
    let store_client = self.store_client.as_ref()?;
    // drv_path is /nix/store/<hash>-<name>.drv — drv_hash is the
    // BFS-key (store path hash); reconstruct full path from
    // state.drv_path (DAG persists it per DerivationState).
    let req = tonic::Request::new(rio_proto::types::GetPathRequest {
        store_path: state.drv_path.clone(),
    });
    match tokio::time::timeout(
        std::time::Duration::from_secs(2),
        store_client.clone().get_path(req),
    )
    .await
    {
        Ok(Ok(stream)) => {
            // GetPathResponse is a stream (chunked NAR). For a .drv
            // (plain-text ATerm, ~1-50KB), concat all chunks.
            let mut bytes = Vec::new();
            let mut stream = stream.into_inner();
            while let Some(chunk) = stream.message().await.ok()? {
                bytes.extend_from_slice(&chunk.content);
            }
            // NAR-wrapped? If GetPath returns NAR framing, strip it
            // (nix-store --dump format). Check at dispatch: does
            // rio-store's GetPath handler return raw bytes or NAR-
            // framed? The worker's fetch path knows — read
            // rio-worker/src/executor/fetch.rs for the unwrap pattern.
            Some(bytes)
        }
        Ok(Err(e)) => {
            debug!(drv_hash = %drv_hash, error = %e, "GetPath failed");
            None
        }
        Err(_elapsed) => {
            debug!(drv_hash = %drv_hash, "GetPath timeout (2s)");
            None
        }
    }
}
```

**CARE — NAR unwrap:** `GetPath` likely returns NAR-framed bytes (`nix-store --dump` format: `"nix-archive-1"` header + `"type" "regular"` + `"contents"` + length + bytes + padding). The .drv ATerm content is inside the NAR frame. Either (a) add a minimal NAR-unwrap helper (the `rio-nix` crate has `nar::Reader`, check if it can yield the single-file contents), or (b) check whether rio-store has a `GetRawFile` RPC that bypasses NAR framing. Option (b) preferred if it exists; option (a) otherwise. The worker's fetch code at [`rio-worker/src/executor/`](../../rio-worker/src/executor/) handles this same case — read it for the canonical pattern.

**CARE — `store_client: Option<_>`:** the scheduler's `store_client` is `Option<StoreServiceClient<Channel>>` (see [`actor/handle.rs:62`](../../rio-scheduler/src/actor/handle.rs)). `None` means test-mode or store-unconfigured — the `?` in `fetch_drv_content_from_store` short-circuits to `None` cleanly, preserving the pre-fix degrade behavior.

### T2 — `fix(scheduler):` dispatch.rs TODO re-tag — P0254 → P0408 (this plan)

MODIFY the TODO comments at `:661` and `:700-705` (post-P0254 line refs). If P0254-T8's implementer already re-tagged to a placeholder sentinel, replace that. Otherwise re-tag `TODO(P0254)` → `TODO(P0408)`:

```rust
// Was: TODO(P0254): fetch from store here when drv_content empty
// (this task closes that — delete the comment, or leave as an
// impl-history note pointing at this plan's commit)
```

The `:700-705` TODO about stashing `resolved.lookups` on `DerivationState` is a DIFFERENT piece of work (realisation_deps insertion ordering) — that one stays as `TODO(P0254)` if P0254-T7 closes it, or gets its own plan tag if T7 defers further. Check at dispatch: `grep 'pending_realisation_deps' rio-scheduler/src/actor/` — if P0254-T7 landed the stash+consume, the `:700-705` TODO is closed.

### T3 — `test(scheduler):` recovered-CA-on-CA dispatch resolves via store fetch

MODIFY [`rio-scheduler/src/actor/dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) `cfg(test)` mod (near P0311-T63's `maybe_resolve_ca` gate tests). One test:

```rust
#[tokio::test]
async fn recovered_ca_on_ca_dispatch_fetches_from_store() {
    // Build a CA-on-CA DAG: parent (floating-CA) depends on
    // child (floating-CA). Merge both with drv_content populated,
    // transition child → Completed (with realisation registered),
    // then SIMULATE recovery: clear parent.drv_content (as PG
    // reload would leave it).
    //
    // MockStore.get_path seeded with parent's ATerm bytes.
    //
    // dispatch_ready(parent) → maybe_resolve_ca sees empty
    // drv_content → fetches from MockStore → resolves placeholders
    // → WorkAssignment.drv_content is the RESOLVED ATerm, not empty.
    //
    // r[verify sched.ca.resolve]
    let (actor, mock_store) = setup_actor_with_mock_store();
    let child_drv_content = b"Derive([...])";  // minimal CA .drv
    let parent_drv_content = b"Derive([...placeholder-child-out...])";
    mock_store.seed_get_path("/nix/store/parent-hash-parent.drv", parent_drv_content);

    // ... merge both, complete child, clear parent.drv_content ...
    actor.dag_mut().get_node_mut("parent").unwrap().drv_content = Vec::new();

    let assignment = actor.dispatch_ready("parent").await.unwrap();
    assert!(!assignment.drv_content.is_empty(),
            "drv_content must be fetched from store, not left empty");
    assert!(!assignment.drv_content.contains(b"placeholder"),
            "placeholders must be resolved post-fetch");
}
```

**CARE — test infrastructure:** MockStore at [`rio-test-support/src/grpc.rs`](../../rio-test-support/src/grpc.rs) may not have a `get_path` stub yet — if so, add a `seed_get_path: HashMap<String, Vec<u8>>` field + handler (same pattern as the existing `MockStore` methods). Check at dispatch.

## Exit criteria

- `grep 'TODO(P0254)' rio-scheduler/src/actor/dispatch.rs` → 0 hits for the `:661` recovery-resolve TODO (closed by T1)
- `grep 'drv_content.is_empty()' rio-scheduler/src/actor/dispatch.rs` → ≥1 hit, but the branch no longer early-returns-empty — it fetches (check via `grep -A5 'is_empty' | grep 'fetch_drv_content'`)
- `cargo nextest run -p rio-scheduler recovered_ca_on_ca_dispatch_fetches_from_store` → 1 passed
- `nix develop -c tracey query rule sched.ca.resolve` shows ≥1 additional verify site (T3's test)
- `/nbr .#ci` green
- **Fail-safe preserved:** store unreachable (mock `get_path` returns `Err`) → dispatch still proceeds with empty `drv_content` (same degrade as pre-fix); worker fails on placeholder + self-heals via retry. T3 should include a second test case proving this fallback.

## Tracey

References existing markers:
- `r[sched.ca.resolve+2]` — T1 extends the implementation (resolve now works for recovered derivations too), T3 verifies. The `+2` version bump at [`scheduler.md:273`](../../docs/src/components/scheduler.md) describes resolve semantics; this plan doesn't change the WHAT (rewrite inputDrvs placeholders to realized paths), only extends the WHEN (now also on recovery). No bump needed.
- `r[sched.recovery.gate-dispatch]` — T1 is downstream of this gate (dispatch only fires after `recovery_complete`; the empty-`drv_content` case only reachable post-recovery)

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "T1: :658-665 replace early-return-empty with store GetPath fetch. T2: re-tag TODO(P0254)→closed or impl-history note. T3: cfg(test) mod — recovered_ca_on_ca_dispatch_fetches_from_store. HOT count=25"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "T3: MockStore +seed_get_path field + get_path handler stub (IF not already present — check at dispatch)"}
]
```

```
rio-scheduler/src/actor/
└── dispatch.rs            # T1: fetch helper + wire into maybe_resolve_ca
                           # T2: TODO re-tag
                           # T3: cfg(test) recovered-CA-on-CA test
rio-test-support/src/
└── grpc.rs                # T3: MockStore.seed_get_path (conditional)
```

## Dependencies

```json deps
{"deps": [254], "soft_deps": [398, 405, 393, 311], "note": "rev-p254 feature (discovered_from=254). HARD-dep P0254: the resolve machinery (collect_ca_inputs returning non-empty, ca_modular_hash plumbing, pending_realisation_deps stash+consume) must be functional before the recovery-fetch is worth anything. Without P0254-T5-T7, collect_ca_inputs returns [] and maybe_resolve_ca is a no-op regardless of drv_content — fetching would just waste an RPC. Soft-dep P0398 (inputDrvs always-empty — changes resolve.rs serialization, doesn't touch dispatch.rs gates; sequence-independent). Soft-dep P0405 (BFS walker dedup — touches completion.rs not dispatch.rs; sequence-independent). Soft-dep P0393 (ContentLookup breaker/timeout — same store_client, different RPC; this plan's 2s timeout is independent). Soft-dep P0311-T63 (maybe_resolve_ca gate tests in same cfg(test) mod — additive test fns, non-overlapping names). dispatch.rs count=25 HOT — T1 inserts ~30L inside maybe_resolve_ca body, no signature change; T3 is cfg(test)-only. MockStore at grpc.rs count=22 — T3's seed_get_path is additive field+handler, same shape as existing MockStore methods. NAR-unwrap: check rio-nix nar::Reader or rio-worker fetch-pattern at dispatch — may add rio-nix as a test-dep IF single-file NAR-unwrap needed."}
```

**Depends on:** [P0254](plan-0254-ca-metrics-vm-demo.md) — T5-T7 wire the resolve machinery; T8 defers THIS work with a plan-number re-tag pointing here.
**Conflicts with:** [`dispatch.rs`](../../rio-scheduler/src/actor/dispatch.rs) count=25 — [P0311-T63](plan-0311-test-gap-batch-cli-recovery-dash.md) adds gate tests in the same `cfg(test)` mod; additive, non-overlapping. [P0254](plan-0254-ca-metrics-vm-demo.md)-T6 rewrites `collect_ca_inputs` at `:726-752`; T1 here is inside `maybe_resolve_ca` at `:658-665` — non-overlapping. [`grpc.rs`](../../rio-test-support/src/grpc.rs) count=22 — [P0311-T19](plan-0311-test-gap-batch-cli-recovery-dash.md) adds `MockStore.fail_batch_precondition`; T3 here adds `seed_get_path`; both additive fields, non-overlapping.
