# Plan 0225: rio-store small deferrals — scopeguard assess + /nix-cache-info auth

Two small `TODO(phase4c)` resolutions bundled: (a) the `cas.rs:564` scopeguard TODO (per A7+GT11: **assess-then-document, likely no code change**); (b) the `cache_server/mod.rs:77` `/nix-cache-info` route positioning.

**Part (a) — THE SCOPEGUARD MAY BE A NON-CHANGE.** The existing comment at [`rio-store/src/cas.rs:555-565`](../../rio-store/src/cas.rs) is explicit:

> Cancellation edge case: if THIS awaiter is cancelled between `shared.await` and here, the inflight entry isn't removed. This is SELF-HEALING: the Shared future already completed, so the next caller to hit `entry().or_insert_with()` gets the completed Shared and awaits it instantly, then THAT caller's remove() fires. Worst case: ~100-byte map entry leaks until the next get for this hash. Acceptable; a scopeguard here would need careful Drop ordering with the Shared clone.

**Do NOT blindly `scopeguard::defer!`.** The warning is real: the `Shared` clone is held while the scope runs. A naive defer changes when the last clone drops, which can deadlock the `DashMap` entry or change completion ordering. The safe path: prove self-healing with a test, improve the comment, delete the TODO tag.

## Tasks

### T1 — `test(store):` prove self-healing of inflight leak

NEW unit test in `rio-store/src/cas.rs` test module:

```rust
#[tokio::test]
async fn inflight_leak_self_heals_on_next_get() {
    let store = test_store();
    let hash = fake_hash();

    // Spawn a get() that we'll abort mid-await
    let handle = tokio::spawn({
        let store = store.clone();
        async move { store.get(&hash).await }
    });
    // Let it insert into inflight and start awaiting the Shared
    tokio::task::yield_now().await;
    // Abort — cancellation between shared.await and remove()
    handle.abort();
    let _ = handle.await;  // JoinError::Cancelled

    // Inflight map now has a stale entry. Prove self-heal:
    // next get() for the same hash should complete AND clear the entry.
    let _ = store.get(&hash).await;

    // After the second get's remove() fires, inflight is empty.
    // (expose inflight.len() via #[cfg(test)] accessor or just
    //  assert behavior: a third get() doesn't coalesce with a dead entry)
    assert_eq!(store.inflight_len(), 0);  // add #[cfg(test)] pub fn
}
```

**If this test passes cleanly** → self-heal is proven → proceed to T2 (doc-only).
**If this test reveals the leak is NOT self-healing** (e.g., the completed `Shared` hangs the second awaiter, or the map entry persists) → escalate: the existing comment is wrong and a real fix is needed. Report to coordinator before proceeding.

### T2 — `docs(store):` improve comment, delete TODO tag (if T1 passes)

MODIFY [`rio-store/src/cas.rs:555-565`](../../rio-store/src/cas.rs) — expand the comment with the now-proven self-heal bound + test reference, and **delete the `TODO(phase4c)` tag**:

```rust
// Cancellation edge case: if THIS awaiter is cancelled between
// `shared.await` and here, the inflight entry isn't removed.
// SELF-HEALING (proven by inflight_leak_self_heals_on_next_get):
// the Shared future already completed, so the next caller to
// `entry().or_insert_with()` gets the completed Shared, awaits it
// instantly (no I/O), and THAT caller's remove() fires. Bound:
// ~100-byte DashMap entry per cancelled-between-await-and-remove,
// cleaned on next get for the same hash.
//
// NO scopeguard: a defer! here changes when the last Shared clone
// drops (the guard holds a reference across the await). Drop
// ordering with the DashMap entry is subtle — the self-heal is
// simpler and correct.
self.inflight.remove(hash);
```

### T3 — `fix(store):` /nix-cache-info before auth middleware

MODIFY [`rio-store/src/cache_server/mod.rs:77`](../../rio-store/src/cache_server/mod.rs) — the `TODO(phase4c)` says `/nix-cache-info` is currently behind auth. Move the route registration before the auth middleware OR use a nested router merge:

```rust
// /nix-cache-info must be public: nix clients hit it FIRST to
// discover the cache's `StoreDir` + `Priority`, before they know
// which token to present. 401 here breaks `nix build --substituters`.
let public = Router::new()
    .route("/nix-cache-info", get(nix_cache_info));

let authed = Router::new()
    .route("/:hash.narinfo", get(narinfo))
    // ... etc
    .layer(auth_middleware);

let app = public.merge(authed);
```

Adjust the existing test at `:527` (or wherever the `/nix-cache-info` test lives) to assert 200 on unauthenticated request.

## Exit criteria

- `/nbr .#ci` green
- `inflight_leak_self_heals_on_next_get` test passes (proves self-heal)
- `cas.rs` no longer contains `TODO(phase4c)` (grep)
- Unauthenticated `GET /nix-cache-info` → 200 (test at `:527` or new test)
- Authenticated `GET /{hash}.narinfo` → still requires Bearer (existing test still passes)

## Tracey

No markers — these are hardening/cleanup, not new normative spec behavior.

## Files

```json files
[
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "T1+T2: self-heal test + improved comment, delete TODO(phase4c) tag at :564"},
  {"path": "rio-store/src/cache_server/mod.rs", "action": "MODIFY", "note": "T3: /nix-cache-info before auth middleware; adjust test at :527"}
]
```

```
rio-store/src/
├── cas.rs                  # T1+T2: test + comment, delete TODO
└── cache_server/mod.rs     # T3: public route merge
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "Wave-1 frontier. Neither file hot. P0242 has a weak soft-dep (bonus assertion on /nix-cache-info 200) — skippable if this merges later."}
```

**Depends on:** none.
**Conflicts with:** none — neither file in the hot-file matrix.

**Escalation path:** if T1 reveals the self-heal comment is WRONG (leak persists or second awaiter hangs), STOP. Report to coordinator with the failing test output. A real scopeguard with correct Drop ordering is a ~30-line careful change, not a ~5-line defer.
