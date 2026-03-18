# Plan 0063: Gateway handler module split + stderr_err! macro

## Design

`rio-gateway/src/handler.rs` was 2228 lines after the `SessionContext` refactor (P0062). This plan brought it to five submodules, but first it added a macro that made the split worthwhile.

**`stderr_err!` macro** (`6987feb`): the CLAUDE.md protocol-guidelines section is explicit — every handler error path must send `STDERR_ERROR` to the client before returning `Err`. Before the macro, this was 23 copies of a 4–5 line pattern: format the message, write the STDERR_ERROR frame, flush, return `Err(anyhow!(msg))`. The macro collapses each to one line. Shrunk `handler.rs` from 2228 to 2099 lines. Sites *not* converted: `send_store_error` (wraps a pre-formed `anyhow::Error` rather than a format string), the `parse_add_multiple_entry` loop (preserves the original error for context), the unknown-opcode handler (client and internal messages differ).

**Module split** (`dce0f1a`): 2099 lines → five files.
- `mod.rs` (355): the `stderr_err!` macro itself (MUST live here, BEFORE `mod` declarations — `macro_rules!` visibility is order-dependent), `SessionContext`, `handle_opcode` dispatch, `send_store_error`, and the drv-cache helpers `try_cache_drv`/`resolve_derivation`.
- `grpc.rs` (110): `grpc_*` request wrappers, `make_proto_path_info`.
- `build.rs` (645): build event stream translation + the three build opcode handlers.
- `opcodes_read.rs` (496): 13 read-only opcode handlers.
- `opcodes_write.rs` (509): 4 write handlers, `parse_cam_str`, inline tests.

Submodules use `use super::*;` for shared imports; cross-module functions are `pub(super)`. `resolve_derivation` stays `pub(crate)` in `mod.rs` because `translate.rs` (outside the handler module) calls it.

Bundled small fixes: direct `Uuid` binding (P0061 enabled the sqlx feature; this uses it), `NAR_CHUNK_SIZE` unification between `rio-store` and `rio-proto` (P0059 set the canonical value; a straggler in `rio-store` was still 64K), `active_build_ids` `Vec → HashMap` (fixes the `ptr_arg` clippy lint it was triggering), `From<WireError>/From<HandshakeError>` for `ExecutorError` (removes `.map_err(|e| ExecutorError::Wire(e))` boilerplate).

## Files

```json files
[
  {"path": "rio-gateway/src/handler.rs", "action": "DELETE", "note": "2099 lines → 5 submodules"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "NEW", "note": "355 LOC: stderr_err! macro (BEFORE mod decls), SessionContext, dispatch, drv-cache"},
  {"path": "rio-gateway/src/handler/grpc.rs", "action": "NEW", "note": "110 LOC: grpc_* wrappers, make_proto_path_info"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "NEW", "note": "645 LOC: event translation + 3 build opcodes"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "NEW", "note": "496 LOC: 13 read-only opcodes"},
  {"path": "rio-gateway/src/handler/opcodes_write.rs", "action": "NEW", "note": "509 LOC: 4 write handlers, parse_cam_str"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "active_build_ids Vec → HashMap (ptr_arg lint)"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "bind Uuid directly (sqlx uuid feature from P0061)"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "NAR_CHUNK_SIZE → rio-proto's 256 KiB"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "From<WireError>/From<HandshakeError> for ExecutorError"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive markers: `r[gw.opcode.*]` for each handler submodule.

## Entry

- Depends on **P0062** (actor split + SessionContext): `handler.rs` was modified in P0062 to adopt `SessionContext`; the split here preserves that structure.

## Exit

Merged as `d874466..dce0f1a` (6 commits). `.#ci` green at merge.
