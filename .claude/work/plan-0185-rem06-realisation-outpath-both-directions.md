# Plan 0185: Rem-06 — Realisation outPath BOTH directions (prepend Register, strip Query)

## Design

**HIGH (downgraded from CRITICAL — `realisations` table empty in prod, CA gated behind `--experimental-features`).** Nix's `Realisation` JSON serializes `outPath` as a BASENAME (`StorePath::to_string()` omits `/nix/store/`); rio's gRPC/PG representation uses full paths. The gateway is the translation boundary and was translating in **neither** direction.

| Direction | Opcode | Bug | Fix |
|---|---|---|---|
| client → store | 42 `wopRegisterDrvOutput` | passed basename → store's `validate_store_path` rejected → `STDERR_ERROR` | prepend `/nix/store/` (idempotent guard) |
| store → client | 43 `wopQueryRealisation` | passed full path → client's `StorePath::parse` rejected `illegal base-32 char '/'` | `strip_prefix` before `json!` |

The **correct** pattern already exists at `rio-nix/src/protocol/build.rs:350-351` (read) and `:415-418` (write). Commit `5786f82` (P0171) fixed `write_build_result` but never touched `opcodes_read.rs` — same field name, different struct.

**Why tests didn't catch it (three compounding failures):** (1) `ca_roundtrip.rs` fixtures sent full paths and asserted full paths — the two bugs cancelled. (2) `MockStore::register_realisation` had no `validate_store_path` — the real store's check never exercised. (3) `nix/tests/scenarios/protocol.nix` subtest labeled `"Realisation outPath basename"` exercised opcode **47** (`BuildResult.builtOutputs`), not opcode 43.

**MockStore hardened:** now mirrors rio-store's prefix+length check; tests flip to basename-on-wire, full-path-in-store. `protocol.nix` comments corrected.

Remediation doc: `docs/src/remediations/phase4a/06-realisation-outpath-both-directions.md` (690 lines). ONE atomic commit — landing either direction alone makes `ca_roundtrip.rs` fail.

## Files

```json files
[
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "opcode 42: prepend /nix/store/ (idempotent); opcode 43: strip_prefix before json!"},
  {"path": "rio-gateway/tests/ca_roundtrip.rs", "action": "MODIFY", "note": "fixtures flip to basename-on-wire, full-path-in-store"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_read.rs", "action": "MODIFY", "note": "byte-level tests for both opcodes"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "MODIFY", "note": "roundtrip"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockStore::register_realisation adds validate_store_path check"},
  {"path": "nix/tests/scenarios/protocol.nix", "action": "MODIFY", "note": "comment correction: subtest covers opcode 47 not 43"}
]
```

## Tracey

No new markers — both opcodes under existing `r[gw.opcode.mandatory-set]` (comment at `opcodes_read.rs:514` already has `// r[impl gw.opcode.mandatory-set]`).

## Entry

- Depends on P0171: smoke-test blockers (the `protocol/build.rs` instance of this fix — same pattern, different site)

## Exit

Merged as `69e30ed` (plan doc) + `a827293` (fix, atomic). `.#ci` green. CA paths register and query correctly.
