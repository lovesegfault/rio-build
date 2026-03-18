# Plan 0021: QueryPathFromHashPart (opcode 29) + isolated golden daemon

## Context

`wopQueryPathFromHashPart` takes the 32-character nixbase32 hash prefix of a store path and returns the full path if it exists in the store. Phase 1a (P0001) shipped it as a stub — always returned empty string. That's fine when the store is empty; it breaks when a real `nix build` tries to check if an input path is already available by hash. This plan replaces the stub with a real `Store` trait method.

It also fixes a golden-test architecture problem: tests were using the system `nix-daemon`, which meant results depended on the host's store state. A path that existed in CI's store but not locally (or vice versa) produced flakes. The fix: golden tests always spawn their own isolated daemon with an empty store root — reproducible regardless of host.

## Commits

- `d54065f` — feat(rio-build): implement QueryPathFromHashPart (opcode 29)

Single commit. Opcode implementation + golden-test architectural change bundled together.

## Files

```json files
[
  {"path": "rio-build/src/store/traits.rs", "action": "MODIFY", "note": "add Store::query_path_from_hash_part(hash_part) → Option<StorePath>"},
  {"path": "rio-build/src/store/memory.rs", "action": "MODIFY", "note": "MemoryStore: linear scan over keys, match on hash_part()"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "handle_query_path_from_hash_part: store lookup, path or empty string, STDERR_ERROR on store error"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "found + not-found byte-level tests"},
  {"path": "rio-build/tests/golden/daemon.rs", "action": "MODIFY", "note": "always spawn isolated nix-daemon (empty store, ca-derivations enabled)"},
  {"path": "rio-build/tests/golden_conformance.rs", "action": "MODIFY", "note": "found/not-found/CA golden tests; multi-opcode sequence fix"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "opcode 29: stub → implemented"},
  {"path": "docs/src/phases/phase1a.md", "action": "MODIFY", "note": "back-reference opcode 29 completion"}
]
```

## Design

**`Store::query_path_from_hash_part`.** Takes a `&str` (32-char nixbase32), returns `Option<StorePath>`. `MemoryStore` implements as a linear scan over the path map — `store.keys().find(|p| p.hash_part() == hash_part)`. Linear is acceptable at phase 1b scale (MemoryStore holds a handful of paths per test); phase 2's filesystem/database stores index by hash part.

**Handler.** Reads the hash part string, calls the store, writes the full path string (or empty string for not-found) after `STDERR_LAST`. Store errors (distinct from not-found) write `STDERR_ERROR` — this is the only opcode where "not found" is a successful response, not an error.

**Isolated golden daemon.** `tests/golden/daemon.rs` previously tried to connect to the system daemon at `/nix/var/nix/daemon-socket/socket`, falling back to spawning its own only if the system one was unavailable. Removed the fallback logic; now always spawns an isolated `nix-daemon --stdio` with a temp-dir store root and `ca-derivations` experimental feature enabled. Every test starts from a known empty state; CA path resolution is testable end-to-end.

**CA golden test.** With the isolated daemon enabling CA derivations, added a golden test for content-addressed path resolution — upload a CA path via opcode 7 (P0015), query its hash part via opcode 29, compare rio's response byte-for-byte with the real daemon's.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.opcode.wopQueryPathFromHashPart`.

## Outcome

Opcode 29 returns real results. Golden tests are host-independent. CA path resolution has end-to-end coverage. The isolated-daemon change was a prerequisite for P0022 — pinning to nix 2.33.3 only works if tests control which daemon version they spawn.
