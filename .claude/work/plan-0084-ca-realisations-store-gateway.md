# Plan 0084: CA realisations — store RPCs + gateway opcodes

## Design

CA (content-addressed) derivation realisations map `(modular_drv_hash, output_name)` → `(output_path, output_hash)`. Modern Nix clients send `wopRegisterDrvOutput` opportunistically after every CA build. The old gateway stub read the wire payload and dropped it silently — every subsequent build of the same CA derivation missed a cache-hit opportunity. Now realisations are stored (~200 bytes/row, one INSERT) and future builds find them via `QueryRealisation`.

The key insight is that modular `drv_hash` (`hashDerivationModulo`) depends only on fixed attributes, NOT on output paths. Two CA derivations with identical inputs hash the same — that's what makes realisations work as a cache key across rebuilds. Idempotency via `ON CONFLICT DO NOTHING`: CA derivations are deterministic, so a duplicate `(drv_hash, output_name)` always maps to the same `output_path`. A repeat insert is "we already knew that", not a conflict worth surfacing.

**Store layer** (`rio-store/src/realisations.rs`): new module with `[u8;32]` hashes as the DB-facing type. The gRPC layer validates proto `Vec<u8>` lengths at the trust boundary before converting. DB-egress validation in `try_into_validated` catches corrupt rows (psql writes, migration bugs) at the boundary instead of propagating bad data through the type system. `MockStore` gained a `realisations` HashMap for the gateway opcode tests.

**Gateway layer** (`wopRegisterDrvOutput`/`wopQueryRealisation`): parses Nix `Realisation` JSON and calls the store RPCs. `DrvOutput` id format is `sha256:<hex>!<name>` — hex not nixbase32, same as `BuildResult` `builtOutputs` (`rio-nix/src/protocol/build.rs:341`). The `sha256:` prefix is literal; Nix doesn't support other algorithms here. `parse_drv_output_id()` splits on `!`, strips `sha256:`, hex-decodes to `[u8;32]`. Rejects wrong length, bad hex, empty output name.

Soft-fail on malformed input: log + accept-and-discard (Register) or empty-set (Query). The stubs did this implicitly; making it explicit means a buggy client that used to work doesn't suddenly hard-fail. A bad registration just means the cache-hit doesn't happen — degraded, not broken. `output_hash` in the Nix Realisation JSON isn't included, so zeros are stored for now (TODO(phase5) populates it from `QueryPathInfo(outPath).nar_hash` before signing realisations). Zeros are fine for cache-hit purposes — `QueryRealisation` only keys on `(drv_hash, output_name)`.

`QueryRealisation` response: `count=1` + reconstructed JSON (`id` echoed, `outPath` from store, `sigs` from store, `dependentRealisations` empty). `serde_json::json!` gives correct escaping. Store `NOT_FOUND` → `count=0` (cache miss, normal). Other store errors after `STDERR_LAST`: can't send `STDERR_ERROR` anymore, so log + empty-set (one missed cache hit, not corruption; the next opcode hits the same store and fails properly). `serde_json` was promoted from dev-dep to prod dep.

## Files

```json files
[
  {"path": "rio-store/src/realisations.rs", "action": "NEW", "note": "[u8;32] hashes, ON CONFLICT DO NOTHING, try_into_validated DB-egress check"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "pub mod realisations"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "RegisterRealisation/QueryRealisation handlers, Vec<u8> length validation"},
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "RegisterRealisation, QueryRealisation RPCs"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "Realisation message type"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "wopRegisterDrvOutput/wopQueryRealisation real impls, parse_drv_output_id, soft-fail"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "serde_json prod import"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_read.rs", "action": "MODIFY", "note": "7 parse_drv_output_id unit tests"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "MODIFY", "note": "4 wire opcode integration tests (replace 2 stub tests)"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockStore realisations HashMap"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0082**: `realisations` table created in migration 006.

## Exit

Merged as `d23e776..758c1cb` (2 commits). Tests: 637 → 652 (+15: 4 store RPC, 7 `parse_drv_output_id` unit, 4 wire opcode integration replacing 2 stubs).

This is the first half of the CA cache-hit path — P0095 completed it with `ContentLookup` and the end-to-end roundtrip test.
