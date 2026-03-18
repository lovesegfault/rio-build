# Plan 0066: Module file splits across remaining crates

## Design

P0062 split `rio-scheduler/src/actor.rs`. P0063 split `rio-gateway/src/handler.rs`. This plan brings the remaining large files to the same one-concern-per-file granularity so the rest of the project works with stable module boundaries.

**`rio-worker/src/executor.rs`** → `executor/{mod,daemon,inputs}.rs`. `daemon.rs` holds the `nix-daemon --stdio` subprocess lifecycle: spawn, namespace setup, handshake, `read_build_stderr_loop`, cleanup. `inputs.rs` holds the pre-build closure computation: `fetch_drv_from_store`, `fetch_input_metadata`, `compute_input_closure` (the parallelized versions from P0061). `mod.rs` holds `execute_build` orchestration and `ExecutionResult`.

**`rio-worker/src/fuse/mod.rs`** → extract `fuse/{fetch,inode,ops}.rs`. `fetch.rs` holds `fetch_and_extract` (store gRPC → NAR unpack → cache). `inode.rs` holds the inode table. `ops.rs` holds the `fuser::Filesystem` trait impl (lookup/getattr/read/readdir callbacks). `mod.rs` keeps `NixStoreFs` struct and mount lifecycle.

**`rio-nix/src/derivation.rs`** → `derivation/{mod,aterm,hash}.rs`. `aterm.rs` is the ATerm parser/serializer. `hash.rs` is `hash_derivation_modulo` (the recursive hash-rewriting that Nix uses for input-addressed derivations). `mod.rs` holds `Derivation` struct and its high-level methods.

**`rio-nix/src/protocol/wire.rs`** → extract `wire/framed.rs`. `FramedStreamReader` (the 8KiB-chunked streaming reader for `wopAddToStoreNar`) was ~200 LOC embedded in the wire-primitives file.

**`rio-common::string_newtype!` macro** + split `rio-scheduler/src/state.rs` → `state/{mod,newtypes}.rs`. The `DrvHash`/`WorkerId` boilerplate from P0062 (`Deref`+`Borrow`+`Display`+`From`+`PartialEq<str>`) became a declarative macro; the newtype definitions move to their own file.

**`rio-gateway/tests/wire_opcodes.rs`** → `wire_opcodes/{main,build,misc,opcodes_read,opcodes_write}.rs`. Test file split by opcode family (mirrors `handler/` structure).

**`rio-scheduler/src/{dag,grpc}.rs`** → move inline `#[cfg(test)] mod tests` to sibling `*/tests.rs` files (keeps production code and test code in separate compilation units for `cargo check` speed).

## Files

```json files
[
  {"path": "rio-worker/src/executor.rs", "action": "DELETE", "note": "split into executor/ submodules"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "NEW", "note": "execute_build orchestration, ExecutionResult"},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "NEW", "note": "nix-daemon --stdio subprocess lifecycle, read_build_stderr_loop"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "NEW", "note": "fetch_drv_from_store, fetch_input_metadata, compute_input_closure"},
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "NEW", "note": "fetch_and_extract (store gRPC → NAR unpack)"},
  {"path": "rio-worker/src/fuse/inode.rs", "action": "NEW", "note": "inode table"},
  {"path": "rio-worker/src/fuse/ops.rs", "action": "NEW", "note": "fuser::Filesystem trait impl (lookup/getattr/read/readdir)"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "reduced to NixStoreFs struct + mount lifecycle"},
  {"path": "rio-nix/src/derivation.rs", "action": "DELETE", "note": "split into derivation/ submodules"},
  {"path": "rio-nix/src/derivation/mod.rs", "action": "NEW", "note": "Derivation struct"},
  {"path": "rio-nix/src/derivation/aterm.rs", "action": "NEW", "note": "ATerm parser/serializer"},
  {"path": "rio-nix/src/derivation/hash.rs", "action": "NEW", "note": "hash_derivation_modulo"},
  {"path": "rio-nix/src/protocol/wire/mod.rs", "action": "NEW", "note": "wire primitives (was wire.rs)"},
  {"path": "rio-nix/src/protocol/wire/framed.rs", "action": "NEW", "note": "FramedStreamReader (8KiB-chunked streaming)"},
  {"path": "rio-common/src/newtype.rs", "action": "NEW", "note": "string_newtype! macro (Deref+Borrow+Display+From+PartialEq<str>)"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "mod newtype"},
  {"path": "rio-scheduler/src/state/mod.rs", "action": "NEW", "note": "DerivationState, WorkerState, BuildInfo"},
  {"path": "rio-scheduler/src/state/newtypes.rs", "action": "NEW", "note": "DrvHash, WorkerId via string_newtype!"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "NEW", "note": "production code"},
  {"path": "rio-scheduler/src/dag/tests.rs", "action": "NEW", "note": "test module (was inline #[cfg(test)])"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "NEW", "note": "production code"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "NEW", "note": "test module (was inline #[cfg(test)])"},
  {"path": "rio-gateway/tests/wire_opcodes.rs", "action": "DELETE", "note": "split by opcode family"},
  {"path": "rio-gateway/tests/wire_opcodes/main.rs", "action": "NEW", "note": "test binary entrypoint"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "NEW", "note": "build-opcode wire tests"},
  {"path": "rio-gateway/tests/wire_opcodes/misc.rs", "action": "NEW", "note": "handshake, set-options, misc"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_read.rs", "action": "NEW", "note": "read-opcode wire tests"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "NEW", "note": "write-opcode wire tests"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`.

## Entry

- Depends on **P0064** (newtype follow-through): `state.rs` was the newtype home; splitting it here needs P0064's completed adoption.

## Exit

Merged as `b1be4f2..ee372e1` (6 commits). `.#ci` green at merge. Zero semantic change — pure file reorganization; test count unchanged.
