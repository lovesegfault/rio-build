# Plan 0073: ValidatedPathInfo + typed path boundaries

## Design

The gRPC `PathInfo` proto has `store_path: String`, `nar_hash: Vec<u8>`, `references: Vec<String>`. Every consumer was either re-validating these fields inline (parsing `StorePath`, checking hash length is 32) or — worse — *not* validating and passing raw strings deeper. `NarinfoRow::into_path_info` (DB egress) did zero validation: malformed rows propagated silently.

**`ValidatedPathInfo`** (`54902d6`, pure additive): `store_path: StorePath`, `nar_hash: [u8; 32]`, `references: Vec<StorePath>`. `TryFrom<PathInfo>` validates at the boundary; `From<ValidatedPathInfo> for PathInfo` is infallible (validated → raw always works). Validation policy:
- `store_path`: hard-fail (primary key everywhere)
- `nar_hash`: hard-fail if ≠32 bytes (SHA-256 is the only wire hash)
- `references`: hard-fail on any invalid entry (feeds `synth_db` → Nix SQLite `Refs` table; malformed ref there is a security issue)
- `deriver`: **soft**-fail (nix-daemon sends empty for source paths; non-empty-invalid → `warn!` + `None`; deriver is informational)
- `content_address`: pass-through, empty → `None`
- `store_path_hash`: not validated (may be empty; `rio-store` computes server-side)

`rio-proto` now depends on `rio-nix` (no cycle: `rio-nix` doesn't depend on `rio-proto`) + `tracing` (for the deriver soft-fail warn). 10 tests.

**Consumer migration** (`ad79bf7`, 14 files, +439/-273): boundary conversions where raw proto meets internal code.
- `rio-proto::client`: `query_path_info_opt` + `get_path_nar` validate via `TryFrom`; malformed store responses become errors, not silent pass-through. `chunk_nar_for_put` takes `ValidatedPathInfo`.
- `rio-store::grpc` `PutPath` ingress: three inline checks (each parsed `StorePath` and discarded it) → single `TryFrom`. `check_bound` for references count KEPT (TryFrom validates each ref syntax but not count). Server egress converts `ValidatedPathInfo → PathInfo` for the wire.
- `rio-store::metadata`: `NarinfoRow::into_path_info` (zero validation) → `try_into_validated`. DB rows validated at read time; malformed rows become loud errors.
- `rio-worker::synth_db`: `path_info_to_synth` → `From<ValidatedPathInfo>` (cleaner than a free fn per user feedback).
- `rio-worker::upload`: `do_upload` defensively parses the overlay-scanned store path (nix-daemon generates these; overlay bugs are possible).
- `rio-gateway::opcodes_write`: `path_info_for_computed` takes pre-parsed `StorePath` + `[u8; 32]` + `Vec<StorePath>` — these were already parsed but thrown away before.

**`StorePath` scheduler sweep** (`ac3c6c9`): related but distinct. `grpc/mod.rs:151` only checked `!drv_path.is_empty()` — a path like `/tmp/evil` would have become a DAG key. `StorePath::parse` catches missing `/nix/store/` prefix, bad hash length/chars, traversal, oversized names. Also `.is_derivation()` check — non-.drv as drv_path is a protocol violation. `DerivationState.drv_path: String → StorePath`. `DagError::InvalidDrvPath` variant. Test fixture migration: ~165 sites, 11 files, net -80 LOC (`make_test_node` drops path arg).

## Files

```json files
[
  {"path": "rio-proto/src/validated.rs", "action": "NEW", "note": "ValidatedPathInfo {store_path: StorePath, nar_hash: [u8;32], references: Vec<StorePath>}; TryFrom<PathInfo>/From"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "mod validated; removes TODO(phase2b)"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "query_path_info_opt + get_path_nar validate via TryFrom; chunk_nar_for_put takes ValidatedPathInfo"},
  {"path": "rio-proto/Cargo.toml", "action": "MODIFY", "note": "rio-nix + tracing deps"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "PutPath ingress: single TryFrom (was 3 inline parses); egress ValidatedPathInfo→PathInfo"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "NarinfoRow::into_path_info → try_into_validated (was zero validation)"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "test adaptation"},
  {"path": "rio-gateway/src/handler/grpc.rs", "action": "MODIFY", "note": "ValidatedPathInfo adoption"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "ValidatedPathInfo adoption"},
  {"path": "rio-gateway/src/handler/opcodes_write.rs", "action": "MODIFY", "note": "path_info_for_computed takes pre-parsed StorePath + [u8;32]"},
  {"path": "rio-gateway/tests/golden/mod.rs", "action": "MODIFY", "note": "test adaptation"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "test adaptation"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "MODIFY", "note": "Vec<StorePath> refs → Vec<String> for closure HashSet"},
  {"path": "rio-test-support/src/fixtures.rs", "action": "MODIFY", "note": "ValidatedPathInfo in fixtures"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockStore ValidatedPathInfo"},
  {"path": "rio-scheduler/Cargo.toml", "action": "MODIFY", "note": "rio-nix promoted from [dev-deps] to [deps]"},
  {"path": "rio-scheduler/src/state/mod.rs", "action": "MODIFY", "note": "DerivationState.drv_path: String → StorePath"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "from_node → try_from_node; DagError::InvalidDrvPath; path_to_hash stays HashMap<String,_> via Borrow<str>"},
  {"path": "rio-scheduler/src/dag/tests.rs", "action": "MODIFY", "note": "make_node/make_edge fixture migration"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "DerivationRow stays String (sqlx push_bind needs &str)"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "StorePath::parse + .is_derivation() validation (was only !is_empty())"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "test adaptation"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "make_test_node drops path arg (~165 sites migrated across 11 files)"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "merge_single_node drops path arg"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "fixture migration"},
  {"path": "rio-scheduler/src/actor/tests/wiring.rs", "action": "MODIFY", "note": "fixture migration"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive: `r[proto.validated.pathinfo]`, `r[sched.validate.drvpath]`.

## Entry

- Depends on **P0011** (wire-types from 1b): `StorePath`/`NixHash` types being wrapped.
- Depends on **P0063** (gateway handler split): `handler/opcodes_*.rs` submodules.
- Depends on **P0059** (proto client helpers): `query_path_info_opt`/`get_path_nar` being modified.

## Exit

Merged as `54902d6..ac3c6c9` (3 commits). `.#ci` green at merge. Removes `TODO(phase2b)` at `rio-proto/src/lib.rs:18`.
