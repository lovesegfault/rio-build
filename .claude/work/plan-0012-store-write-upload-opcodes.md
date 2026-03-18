# Plan 0012: Store write path + upload opcodes (AddToStoreNar, AddMultipleToStore, QueryDerivationOutputMap)

## Context

Phase 1a's `MemoryStore` was read-only — `query_path_info`, `is_valid_path`, `nar_from_path`. Before the gateway can build anything, the client has to upload the `.drv` files and input paths to the store. `nix build --store ssh-ng://...` does this via `wopAddToStoreNar` (single path) or `wopAddMultipleToStore` (batch, protocol ≥1.32). Once uploaded, the client asks `wopQueryDerivationOutputMap` to confirm the outputs are known before sending `wopBuildDerivation`.

This plan adds `Store::add_path` to the trait, implements it on `MemoryStore`, adds a per-session `.drv` cache to the gateway session state, and wires three opcode handlers. The `.drv` cache is the connective tissue: when a `.drv` arrives via `AddToStoreNar`, the handler parses it with P0008's ATerm parser and caches the `Derivation` in a session-scoped `HashMap`, so `QueryDerivationOutputMap` can answer without re-fetching and re-parsing.

## Commits

- `c7785c6` — feat(rio-build): add store write operations and per-session .drv cache
- `81f9ff1` — feat(rio-build): implement wopAddToStoreNar and wopQueryDerivationOutputMap
- `60243ff` — feat(rio-build): implement wopAddMultipleToStore with framed stream parsing

Three commits. First is the trait extension; second and third are the handlers.

## Files

```json files
[
  {"path": "rio-build/src/store/traits.rs", "action": "MODIFY", "note": "add Store::add_path(path, info, nar_bytes)"},
  {"path": "rio-build/src/store/memory.rs", "action": "MODIFY", "note": "MemoryStore::add_path — idempotent insert, existing paths not overwritten"},
  {"path": "rio-build/src/gateway/session.rs", "action": "MODIFY", "note": "add drv_cache: HashMap<StorePath, Derivation> to session state"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "handle_add_to_store_nar, handle_add_multiple_to_store, handle_query_derivation_output_map; try_cache_drv helper"},
  {"path": "rio-build/Cargo.toml", "action": "MODIFY", "note": "add hex dep for narHash decoding"},
  {"path": "docs/src/phases/phase1b.md", "action": "MODIFY", "note": "mark upload opcode tasks complete"}
]
```

## Design

**Store write trait.** `Store::add_path(&self, path: StorePath, info: PathInfo, nar: Vec<u8>)` is the one new method. `MemoryStore` implements it as idempotent insert — if the path already exists, the operation is a no-op (not an error). This matches Nix's semantics: store paths are content-addressed, so two uploads of the same path carry the same content by definition.

**wopAddToStoreNar (opcode 39).** This version used a STDERR_READ pull loop: server sends `STDERR_READ(n)`, client responds with up to n bytes, repeat until `nar_size` bytes received. That's the protocol for clients ≤1.22. Modern clients use framed stream — P0018 fixed this, but at this point the integration test (P0013) was testing against itself so the mismatch was invisible. The handler validates the received NAR's SHA-256 against the declared `narHash` (decoded from hex — this is where the CLAUDE.md "narHash on wire is hex, not nixbase32" rule comes from), stores the path, and if the path name ends in `.drv`, calls `try_cache_drv` to parse and cache it.

**wopAddMultipleToStore (opcode 44).** The modern batch upload. Outer framed stream (P0011) is reassembled into a buffer, then entries are parsed sequentially: each entry is `PathInfo` metadata + inner framed NAR. Same per-entry validation and `.drv` caching as opcode 39. The `dontCheckSigs` flag in the wire payload is always treated as `false` — signature verification policy is a server decision, not a client request.

**wopQueryDerivationOutputMap (opcode 41).** Given a `.drv` path, return `{output_name: output_store_path}`. First checks the session `drv_cache`; on miss, fetches the NAR from the store (`nar_from_path`), extracts the `.drv` file with P0009's `extract_single_file`, parses with P0008's ATerm parser, caches for next time, and returns the output map. The cache-miss path is the "store fallback" from the phase 1b task list.

**try_cache_drv.** Shared helper: given NAR bytes for a path ending in `.drv`, extract the file, parse ATerm, insert into `drv_cache`. Called by both upload handlers. Failures are logged at debug level and swallowed — a malformed `.drv` doesn't abort the upload (the upload succeeded; the cache miss later is recoverable). P0020 elevated this to error level after it masked a real bug during testing.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.opcode.wopAddToStoreNar`, `gw.opcode.wopAddMultipleToStore`, `gw.opcode.wopQueryDerivationOutputMap`, `store.write`.

## Outcome

Upload path is wired end-to-end: `nix copy --to ssh-ng://localhost` can push paths into `MemoryStore`. `.drv` files are parsed and cached on arrival. `QueryDerivationOutputMap` answers from cache. The STDERR_READ pull loop in `AddToStoreNar` was wrong for protocol ≥1.23 — P0018 replaced it with framed-stream — but the integration test (P0013) used the same encoding on both sides so the bug was masked until real-nix integration.
