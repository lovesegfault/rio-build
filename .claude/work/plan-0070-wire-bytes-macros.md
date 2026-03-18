# Plan 0070: wire_bytes! / wire_send! test macros + adoption

## Design

Every wire-level test constructed request bytes with a sequence of `wire::write_*().await.unwrap()` calls. rustfmt imposes 3 lines per call (the `.await` and `.unwrap()` each get their own line past a certain length). For a test sending 10 wire primitives, that's 30 lines of preamble before the assertion. Multiply by ~60 wire tests.

**`wire_bytes!`** (`c81f7c6`): builds a `Vec<u8>` from sequential wire primitives in one expression. Kinds: `u64`, `string`, `strings`, `bool`, `bytes`, `framed` (8KiB chunks), `raw` (extend_from_slice). Self-test roundtrips through `wire::read_*` to verify encoding correctness — the macro isn't just a syntax compressor, it's a correctness-verified DSL.

**`wire_send!`**: same but writes to a stream and flushes. Handles `&mut` reborrow so callers can pass `s: &mut T` without consuming the reference.

**Aggressive adoption** (`2c57396`): every `wire::write_*` call in test files replaced.
- `wire_opcodes/opcodes_write.rs`: 114 calls → 14 blocks (-94 LOC)
- `wire_opcodes/opcodes_read.rs`: 40 calls → 19 blocks (-27 LOC)
- `wire_opcodes/build.rs` + `misc.rs`: 53 calls (-30 LOC)
- `golden/mod.rs` `build_*_bytes`: 8 functions to one-liners (-26 LOC)
- `golden/daemon.rs`: 4 phase buffers (-13 LOC)
- `integration_distributed.rs`: handshake phases (-11 LOC)
- `wire.rs`: `do_handshake`/`send_set_options` self-dogfood (-8 LOC)

`AsyncWriteExt` import removed where `flush` is now inside `wire_send!`.

**Schema-driven golden parsers** (`dbf8793`): 7 of 10 `parse_*_fields` functions were mechanical `vec![ResponseField{...}]` literals. Replaced with a `FieldType` enum + `parse_schema` table-driven parser. Kept explicit: `parse_handshake_fields` (reference example), `parse_query_path_info_fields` (conditional on valid bit), `parse_nar_from_path_fields` (stream loop).

**Scheduler test fixture consolidation** (`bcb449d`): 28 tests started with identical 2–3 line `TestDb + setup_actor` boilerplate. `setup()` and `setup_with_worker()` helpers. -52 LOC across `coverage.rs`/`integration.rs`/`wiring.rs`. `_db` binding kept (not bare `_`) to preserve `TestDb::Drop` semantics.

**Final `#[allow]` removal** (`b474a72`): `BuildResult` fields made pub — the last `#[allow(dead_code)]` in the workspace.

## Files

```json files
[
  {"path": "rio-test-support/src/wire.rs", "action": "MODIFY", "note": "wire_bytes! + wire_send! macros (u64/string/strings/bool/bytes/framed/raw); self-test roundtrip"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "MODIFY", "note": "114 calls → 14 blocks (-94 LOC)"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_read.rs", "action": "MODIFY", "note": "40 calls → 19 blocks (-27 LOC)"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "macro adoption (-30 LOC combined with misc)"},
  {"path": "rio-gateway/tests/wire_opcodes/misc.rs", "action": "MODIFY", "note": "macro adoption"},
  {"path": "rio-gateway/tests/wire_opcodes/main.rs", "action": "MODIFY", "note": "import cleanup"},
  {"path": "rio-gateway/tests/golden/mod.rs", "action": "MODIFY", "note": "build_*_bytes → one-liners; FieldType enum + parse_schema table-driven parser"},
  {"path": "rio-gateway/tests/golden/daemon.rs", "action": "MODIFY", "note": "4 phase buffers → wire_bytes!"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "handshake phases via macro"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "adopt pub BuildResult fields"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "setup()/setup_with_worker() fixture"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "setup() fixture helpers"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "setup() fixture"},
  {"path": "rio-scheduler/src/actor/tests/wiring.rs", "action": "MODIFY", "note": "setup_with_worker() fixture"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "adopt pub BuildResult fields"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "BuildResult fields pub, remove last #[allow(dead_code)]"},
  {"path": "rio-nix/src/derivation/hash.rs", "action": "MODIFY", "note": "minor cleanup"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`.

## Entry

- Depends on **P0059** (test-support modularize): `rio-test-support/src/wire.rs` was created in P0059.
- Depends on **P0066** (module splits): `wire_opcodes/*.rs` submodules and `actor/tests/*.rs` submodules were created in P0066 and P0064.

## Exit

Merged as `c81f7c6..b474a72` (5 commits). `.#ci` green at merge. Net test LOC: -~260. Zero `#[allow(dead_code)]` remaining.
