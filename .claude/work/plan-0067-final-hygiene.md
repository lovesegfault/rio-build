# Plan 0067: Final hygiene — dead-code pass 2, dedup, Rust 2024 idioms

## Design

The last cleanup sweep before the feature bulk. After all the module splits (P0062–P0066), a second pass found what the first pass couldn't see: code that was only reachable from the old monolithic files, helpers that could be extracted now that the same pattern appeared in three *separate* files, and idioms that clippy-nursery caught once the files were small enough to lint cleanly.

**Dead code round 2** (4 commits): `rio-nix` wire primitives `read_u8`/`write_u8`/`read_i64`/`write_i64` (zero callers; the doc comment on `read_u8` even said "Do not use for protocol integer fields"). `NarEntry::name()`/`node()` accessors (all in-crate usage went through public fields directly). Scheduler `dag::node_count`/`find_leaves`/`topological_order` (only referenced by their own tests). `db::upsert_derivation`/`insert_edge` (superseded by the batched versions from P0061). `queue::peek`. `client_build_derivation` (test-only, `cfg`-gated). `query_path_info_by_hash`. Visibility tightening: several `pub fn` → `pub(crate)` or `pub(super)`.

**Dedup helpers** (5 commits):
- `Derivation::parse_from_nar` — the pipeline `extract_single_file → String::from_utf8 → Derivation::parse` appeared 3× with near-identical error wrapping. Gateway `try_cache_drv`: 23→9 LOC; `resolve_derivation`: 8→2 LOC; worker `fetch_drv_from_store`: 8→2 LOC.
- Gateway `opcodes_write` helpers: `path_info_for_computed` (5-param constructor with defaults for `AddToStore`/`AddTextToStore`), `parse_reference_paths` (iterator-based ref parse with error context).
- `send_store_error(anyhow!(...))` → `stderr_err!` at 12 sites (the macro from P0063 applied to more sites uncovered by the splits).
- Worker `bind_mount` helper for the `pre_exec` closure.
- `rio-nix` shared ATerm tail between `to_aterm` and `to_aterm_modulo` (the only difference is which hash goes in the output field; the rest was 40 duplicated lines).

**`StorePath` full-string caching** (`6b6fae6`): previously `StorePath` stored only `(hash, name)` and regenerated the full `/nix/store/...` string via `nixbase32::encode` on every `Display`/`to_string()` call — ~15 hot-path sites in `reconstruct_dag`, wire writes, gRPC calls. Added `full: String` field populated at parse time. `as_str()`/`Deref`/`AsRef`/`Borrow<str>` for ergonomics. Manual `Hash`/`Eq`/`PartialEq` on `full` lets `HashMap<StorePath>` look up by `&str` via `Borrow`. `PartialEq<str>`/`<&str>`/`<String>` for test assertions. 8 call sites migrated from `.to_string()` to `.as_str()` where `&str` is expected. All remaining `.to_string()` sites (proto `String` fields need owned) are now cheap (clone cached string, no encode). Memory cost: ~50 bytes per `StorePath`. Worth it.

**Rust 2024 idioms sweep** (`dffefd8`): `.is_some_and()`, `.map_or()`, `(!empty).then()`, `let-else`, `mem::take().into_iter()` over `drain(..)` when vec is reused, wildcard match arms replaced with explicit variants, removed `async` from functions that don't await (40 `.await` calls removed from callers), `&Arc<RwLock<T>>` → `&RwLock<T>` where Arc never cloned.

Final test dedup: narinfo tests (-70 LOC), merged setup helpers, `init_from_env` for observability tests.

## Files

```json files
[
  {"path": "rio-nix/src/protocol/wire/mod.rs", "action": "MODIFY", "note": "delete read_u8/write_u8/read_i64/write_i64"},
  {"path": "rio-nix/src/protocol/wire/framed.rs", "action": "MODIFY", "note": "visibility tightening"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "delete NarEntry::name()/node() accessors, pub fields"},
  {"path": "rio-nix/src/derivation/mod.rs", "action": "MODIFY", "note": "Derivation::parse_from_nar helper (NarExtract/InvalidUtf8 variants)"},
  {"path": "rio-nix/src/derivation/aterm.rs", "action": "MODIFY", "note": "extract shared aterm tail from to_aterm/to_aterm_modulo"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "cache full string, as_str/Deref/Borrow<str>, manual Hash/Eq on full"},
  {"path": "rio-nix/src/hash.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "test dedup -70 LOC"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "delete client_build_derivation, cfg-gate test-only"},
  {"path": "rio-nix/src/protocol/handshake.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-nix/src/protocol/stderr.rs", "action": "MODIFY", "note": "visibility tightening"},
  {"path": "rio-nix/src/protocol/derived_path.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "delete node_count/find_leaves/topological_order"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "delete upsert_derivation/insert_edge (superseded by batched)"},
  {"path": "rio-scheduler/src/queue.rs", "action": "MODIFY", "note": "delete peek"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "delete dead actor variants; Rust 2024 idioms"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "delete query_path_info_by_hash"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "try_cache_drv 23→9 LOC, resolve_derivation 8→2 LOC (parse_from_nar)"},
  {"path": "rio-gateway/src/handler/opcodes_write.rs", "action": "MODIFY", "note": "path_info_for_computed, parse_reference_paths helpers"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "12 send_store_error(anyhow!)→stderr_err!"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-gateway/src/handler/grpc.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "StorePath as_str() adoption"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "Rust 2024 idioms"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "MODIFY", "note": "fetch_drv_from_store 8→2 LOC (parse_from_nar)"},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "MODIFY", "note": "bind_mount helper for pre_exec"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "visibility tightening"},
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "init_from_env test helper"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`.

## Entry

- Depends on **P0065** (helper extraction pass): this is a second pass over files P0065 already touched.
- Depends on **P0066** (module splits): dead code found because splits made it visible (e.g., `topological_order` was only reachable from its own test once `dag.rs` was isolated).

## Exit

Merged as `f1abc73..7c8e156` (14 commits). `.#ci` green at merge. Net workspace: -~500 LOC. Zero remaining `#[allow(dead_code)]` in production code.
