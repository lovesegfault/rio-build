# Plan 0465: Gateway JWT propagation through read-opcodes → store

[P0464](plan-0464-upstream-substitution-validation.md) VM-test development found that `handle_query_missing` and `handle_query_path_info` in [`opcodes_read.rs`](../../rio-gateway/src/handler/opcodes_read.rs) call `store_client` **without** attaching the session's JWT as `x-rio-tenant-token` gRPC metadata. Result: substitution short-circuits when invoked via ssh-ng — the store sees an anonymous request, `FindMissingPathsRequest` returns empty `substitutable_paths`, and the client is told to build what it could have fetched. Only [`build.rs:378`](../../rio-gateway/src/handler/build.rs) (the `SubmitBuild` path) sends the JWT today.

Code already has `TODO(P0465)` at [`nix/tests/scenarios/substitute.nix:14`](../../nix/tests/scenarios/substitute.nix) where the grpcurl workaround lives. This plan threads the JWT through the read-opcode handlers, removes the workaround, and verifies end-to-end via `cargo xtask k8s -p kind rsb`.

## Entry criteria

- [P0463](plan-0463-upstream-substitution-surface.md) merged (`FindMissingPathsResponse.substitutable_paths` populated; `willSubstitute` wired in `wopQueryMissing`) — **DONE**
- [P0464](plan-0464-upstream-substitution-validation.md) merged (`substitute.nix` scenario exists with the grpcurl workaround + `TODO(P0465)` marker) — **DONE**

## Tasks

### T1 — `fix(gateway):` thread JWT through opcodes_read.rs store_client calls

MODIFY [`rio-gateway/src/handler/opcodes_read.rs`](../../rio-gateway/src/handler/opcodes_read.rs). Pattern is at [`build.rs:378-383`](../../rio-gateway/src/handler/build.rs):

```rust
if let Some(token) = jwt_token {
    request.metadata_mut().insert(
        rio_common::jwt_interceptor::TENANT_TOKEN_HEADER,
        tonic::metadata::MetadataValue::try_from(token)?,
    );
}
```

Apply to every handler that calls `store_client`. From `grep store_client opcodes_read.rs`:

| Handler | Line | gRPC call | JWT needed? |
|---|---|---|---|
| `handle_is_valid_path` | `:11` | `grpc_is_valid_path` | YES — tenant-scoped validity |
| `handle_query_valid_paths` | `:39` | `grpc_is_valid_path` | YES |
| `handle_query_path_info` | `:73` | `grpc_query_path_info` | YES — tenant-scoped narinfo filter (`r[store.tenant.narinfo-filter]`) |
| `handle_query_valid_derivers` | `:130` | `find_missing_paths` | YES |
| `handle_nar_from_path` | `:273` | `get_path` | YES — tenant-scoped read |
| `handle_query_path_from_hash_part` | `:338` | `query_path_from_hash_part` | YES |
| `handle_add_signatures` | `:382` | `add_signatures` | YES — tenant-scoped write |
| `handle_query_missing` | `:722` (actual call) | `find_missing_paths` | **YES — this is the rsb blocker** |
| others at `:449+` | varies | check each |

The handlers take `store_client: &mut StoreServiceClient<Channel>` directly. Thread the JWT one of two ways:

**Option A (per-call, explicit):** Add `jwt_token: Option<&str>` param to each handler, build `tonic::Request` explicitly instead of relying on the `impl IntoRequest` sugar, attach metadata. Matches `build.rs` precedent exactly.

**Option B (interceptor wrap, once):** Wrap `store_client` in a per-session `tower::Service` layer that injects `x-rio-tenant-token` from `SessionContext` on every outbound call. One change in `handler/mod.rs` where `store_client` is constructed; zero changes to individual handlers. Less duplication, but the interceptor must be session-scoped (JWT differs per connection).

**Prefer Option B** — it's one-shot and future-proof (new handlers get JWT for free). If the session-scoped interceptor is awkward to plumb, fall back to Option A for the blocking handlers (`handle_query_missing`, `handle_query_path_info`) and file a followup for the sweep.

### T2 — `test(vm):` substitute.nix — remove grpcurl workaround, exercise ssh-ng path

MODIFY [`nix/tests/scenarios/substitute.nix`](../../nix/tests/scenarios/substitute.nix) at `:14`:

1. Delete the `TODO(P0465)` comment block
2. Replace the grpcurl `FindMissingPaths` direct-call with a `nix-store --query --size` (or equivalent) that goes via ssh-ng → gateway → `wopQueryMissing` → store. The store should report the path as substitutable BECAUSE the tenant has `cache.nixos.org` configured AND the gateway propagated the JWT.
3. Assert the ssh-ng path works end-to-end: the client sees "N paths will be fetched" (not "N paths will be built") for a known-cached derivation like `hello`.

The existing `# r[verify store.substitute.upstream]` marker at the subtests wiring in [`default.nix`](../../nix/tests/default.nix) stays — this test still covers that marker, now via the real protocol path instead of the grpcurl backdoor.

### T3 — `test(manual):` cargo xtask k8s -p kind rsb — hello substitution works

Manual verification (not CI-gated, documented in exit criteria for the implementer):

```bash
cargo xtask k8s -p kind deploy
rio-cli upstream add --tenant <t> --url https://cache.nixos.org \
  --trusted-key 'cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY='
cargo xtask k8s -p kind rsb -L 'p#hello'
```

Expected: `hello` is FETCHED from `cache.nixos.org` via the store's substituter, not BUILT by a builder pod. The `rsb` output should show the substitution path; `kubectl logs -n rio-store deploy/rio-store | grep substitute` should show the upstream fetch.

## Exit criteria

- `/nixbuild .#ci` → green
- `grep 'TODO(P0465)' nix/tests/scenarios/substitute.nix rio-gateway/src/` → 0 hits (all resolved)
- `grep -c 'TENANT_TOKEN_HEADER\|x-rio-tenant-token' rio-gateway/src/handler/opcodes_read.rs` → ≥1 (JWT attached, either per-call or via interceptor import)
- `nix/tests/scenarios/substitute.nix` exercises ssh-ng → `wopQueryMissing` → store (no grpcurl backdoor)
- Manual: `cargo xtask k8s -p kind rsb -L 'p#hello'` succeeds with substitution after `rio-cli upstream add cache.nixos.org`

## Tracey

References existing markers:
- `r[gw.jwt.issue]` — T1 extends JWT attachment from scheduler-bound requests to store-bound requests ([`gateway.md:497`](../../docs/src/components/gateway.md))
- `r[gw.jwt.dual-mode]` — T1's `Option<&str>` param respects the dual-mode (absent → fallback) contract ([`gateway.md:503`](../../docs/src/components/gateway.md))
- `r[gw.opcode.query-missing]` — T1 fixes the handler; T2 verifies ([`gateway.md:303`](../../docs/src/components/gateway.md))
- `r[gw.opcode.query-path-info]` — T1 fixes the handler ([`gateway.md:251`](../../docs/src/components/gateway.md))
- `r[store.substitute.upstream]` — T2 verifies via real ssh-ng path instead of grpcurl backdoor ([`store.md:212`](../../docs/src/components/store.md))
- `r[store.tenant.narinfo-filter]` — T1's JWT propagation makes this reachable via ssh-ng ([`store.md:195`](../../docs/src/components/store.md))

No new markers — this is a correctness fix wiring existing JWT infrastructure through handlers that were missed.

## Files

```json files
[
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "T1: thread JWT through store_client calls — Option B interceptor wrap preferred, Option A per-handler fallback"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "T1 Option B: wrap store_client in session-scoped JWT-injecting interceptor"},
  {"path": "nix/tests/scenarios/substitute.nix", "action": "MODIFY", "note": "T2: remove grpcurl workaround + TODO(P0465) at :14; exercise ssh-ng wopQueryMissing path"}
]
```

```
rio-gateway/src/handler/
├── opcodes_read.rs            # T1: JWT metadata on store_client calls
└── mod.rs                     # T1 Option B: session-scoped interceptor
nix/tests/scenarios/
└── substitute.nix             # T2: ssh-ng path, no grpcurl
```

## Dependencies

```json deps
{"deps": [463, 464], "soft_deps": [973647703], "note": "Pre-allocated — TODO(P0465) markers exist at substitute.nix:14. P0463 shipped willSubstitute wiring; P0464 shipped the VM scenario with grpcurl workaround that found this gap. Soft-dep P973647703 (downloadSize/narSize wire-up) — both touch opcodes_read.rs handle_query_missing; P973647703 edits :779-780 (response tail), this plan edits :722 (request head). Non-overlapping hunks in same fn; either order works but landing P0465 first means P973647703 sees a complete JWT-aware handler. discovered_from=464 (VM test development)."}
```

**Depends on:** [P0463](plan-0463-upstream-substitution-surface.md) — `willSubstitute` wired. [P0464](plan-0464-upstream-substitution-validation.md) — substitute.nix scenario + TODO marker exist. Both **DONE**.
**Soft-dep:** [P973647703](plan-973647703-wopquerymissing-download-nar-size.md) — same fn, non-overlapping hunks.
**Conflicts with:** [`opcodes_read.rs`](../../rio-gateway/src/handler/opcodes_read.rs) count varies — T1 here touches handler signatures + request construction. P973647703 touches `:779-780` response tail. Check collisions at dispatch.
