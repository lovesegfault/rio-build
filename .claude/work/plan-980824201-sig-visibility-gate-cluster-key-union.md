# Plan 980824201: sig_visibility_gate — include rio cluster key in trusted_keys union

Bughunter found a timing-window correctness bug in [`sig_visibility_gate`](../../rio-store/src/grpc/mod.rs) at `:471-524`. The gate discriminates built-vs-substituted paths by `path_tenants` row count: ≥1 row → built by someone → skip gate; 0 rows → substitution-only → verify signature against tenant's `trusted_keys`. The design doc (at `:455-466`) says this "correctly handles the pre-`path_tenants` timing window" — but it doesn't.

**The bug:** `path_tenants` is populated by the *scheduler* at build-completion (`upsert_path_tenants` in [`rio-scheduler/src/db/live_pins.rs`](../../rio-scheduler/src/db/live_pins.rs)), NOT by `PutPath`. During the PutPath→scheduler-completion window (typically sub-second, but can be longer under load), `path_tenants` count is 0, so the gate fires. The path is rio-signed (cluster key), but the tenant's `trusted_keys` contains only *upstream* pubkeys — NOT the rio cluster key. So a freshly-built, rio-signed path returns `NotFound` to its own tenant until the scheduler catches up.

**The design intent** (quoted in the followup): "tenant trusted = upstream pubkeys ∪ rio cluster key". The implementation dropped the union — it only checks upstream keys. The fix is to include the cluster public key in the verification set.

Discovered by bughunter during cross-tenant sig-visibility review ([P0463](plan-0463-upstream-substitution-surface.md) context).

## Entry criteria

- [P0463](plan-0463-upstream-substitution-surface.md) merged (sig_visibility_gate introduced). Already DONE on `sprint-1`.

## Tasks

### T1 — `fix(store):` add cluster pubkey to sig_visibility_gate verification set

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) at `sig_visibility_gate` (`:471`). Where the gate currently builds the trusted-key set from `tenant_upstreams.trusted_keys`, union in the cluster public key:

```rust
// r[impl store.substitute.tenant-sig-visibility]
// Trusted = tenant's upstream pubkeys ∪ rio cluster key.
// Without the cluster key, a freshly-built path (rio-signed,
// path_tenants not yet populated by scheduler) would be gated
// as "untrusted substitution" and return NotFound.
let mut trusted = fetch_tenant_upstream_keys(pool, tenant_id).await?;
trusted.push(self.cluster_pubkey.clone());
```

The cluster pubkey is already available on the service struct (see `:387` "ResignPaths needs the SAME cluster key as PutPath").

### T2 — `test(store):` timing-window regression test

NEW integration test in [`rio-store/tests/grpc/`](../../rio-store/tests/grpc/). Sequence:
1. `PutPath` a freshly-built path (rio-signed with cluster key)
2. Do NOT populate `path_tenants` (simulate the scheduler-not-yet-caught-up window)
3. `QueryPathInfo` as the owning tenant
4. Assert: returns the path (NOT `NotFound`)

Before this fix step 4 returns `NotFound`. After, it returns the path because the cluster-key signature verifies.

### T3 — `docs(store):` correct the doc-comment at :462

MODIFY [`rio-store/src/grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) `:455-466`. The current comment claims the timing window is "correctly handled" — it isn't, pre-fix. Rewrite to explain the cluster-key union:

```rust
/// The PutPath→scheduler-completion window (path_tenants count=0,
/// but path IS built) is handled by including the rio cluster key
/// in the trusted set. A rio-signed path always verifies; only
/// paths signed ONLY by upstream keys the tenant doesn't trust
/// are gated.
```

## Exit criteria

- `cargo nextest run -p rio-store sig_visibility_gate_cluster_key_timing_window` → pass
- `grep 'cluster_pubkey\|cluster.*key' rio-store/src/grpc/mod.rs` in `sig_visibility_gate` body → ≥1 hit
- T2's precondition self-check: assert `path_tenants` count is 0 before the QueryPathInfo — proves we're testing the timing window, not the post-scheduler path
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[store.substitute.tenant-sig-visibility]` — T1 corrects the implementation to match spec intent (upstream pubkeys ∪ cluster key)
- `r[sched.gc.path-tenants-upsert]` — context: scheduler populates path_tenants, not PutPath; this is the source of the timing window

## Files

```json files
[
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T1: cluster-key union at :471 sig_visibility_gate; T3: doc-comment correction :455-466"},
  {"path": "rio-store/tests/grpc/sig_visibility.rs", "action": "MODIFY", "note": "T2: timing-window regression test (NEW if no existing sig_visibility test file)"}
]
```

```
rio-store/
├── src/grpc/mod.rs         # T1: union; T3: doc
└── tests/grpc/
    └── sig_visibility.rs   # T2: regression test
```

## Dependencies

```json deps
{"deps": [463], "soft_deps": [], "note": "P0463 introduced sig_visibility_gate. Bug was latent from introduction."}
```

**Depends on:** [P0463](plan-0463-upstream-substitution-surface.md) — introduced `sig_visibility_gate` at `:471`.

**Conflicts with:** None on the rsb chain — this is a distinct code path from P0474/P0475's chunk-upload/rollback logic.
