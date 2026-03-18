# Plan 0083: QueryPathFromHashPart + AddSignatures store RPCs

## Design

Two gateway opcodes had phase2c-tagged TODOs at `rio-gateway/src/handler/opcodes_read.rs:292,337` — both were workarounds that silently misbehaved. This plan replaced both with dedicated store RPCs.

**QueryPathFromHashPart:** `wopQueryPathFromHashPart` (opcode 29) hands the gateway just the 32-char nixbase32 hash — no `/nix/store/` prefix, no name suffix. The old gateway workaround called `QueryPathInfo` with a constructed `/nix/store/{hash}` path. That's always a miss: a store path without a name component is not a valid `StorePath`, so the lookup never succeeds. The dedicated RPC does a `LIKE` prefix lookup: `WHERE store_path LIKE '/nix/store/{hash}-%'`. Validation runs BEFORE the query: `hash_part` feeds a `LIKE` pattern, so unvalidated `%` or `_` is an injection vector. `nixbase32::decode()` checks both charset (the alphabet contains neither metachar) and length in one call. This same helper was reused by P0088's cache server for `/{hash}.narinfo` HTTP requests.

**AddSignatures:** `wopAddSignatures` (opcode 37) was a stub that read wire data and discarded it. Now it appends to `narinfo.signatures TEXT[]` via PG array concat (`||`). No deduplication — Nix's own `AddSignatures` doesn't dedupe either. No `manifests`-join — signing a path whose upload got stuck shouldn't fail on that coupling; signatures are narinfo metadata, not upload state. Empty sigs list is a no-op (not an error), short-circuiting before the PG roundtrip.

The gateway test `test_add_signatures_stub_returns_success` was renamed and split: it now seeds `MockStore` first (the stub accepted anything; the real RPC needs the path to exist) and verifies sigs actually land in `MockStore`. A new error-path test covers unknown-path → `NOT_FOUND` → `STDERR_ERROR`.

## Files

```json files
[
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "QueryPathFromHashPart, AddSignatures RPC defs"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "request/response message types"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "query_by_hash_part (LIKE prefix, nixbase32 validation), append_signatures (array concat, no manifests join)"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "RPC handlers"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "wopQueryPathFromHashPart real impl (was QueryPathInfo workaround), wopAddSignatures real impl (was discard stub)"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "MODIFY", "note": "test_add_signatures_stub renamed/split; unknown-path error test"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "RPC integration tests"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockStore sigs HashMap"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0082**: `narinfo.signatures` column and `metadata.rs` post-migration-006 state.

## Exit

Merged as `f1af1d6` (1 commit). Tests: 630 → 637 (+7).

Resolved `TODO(phase2c)` at `rio-gateway/src/handler/opcodes_read.rs:292,337`.
