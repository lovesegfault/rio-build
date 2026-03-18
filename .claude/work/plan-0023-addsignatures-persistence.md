# Plan 0023: AddSignatures persistence (opcode 37)

## Context

`wopAddSignatures` lets a client attach additional signatures to an existing store path. Phase 1a (P0001) shipped it as a stub that read the signatures and discarded them — the opcode returned success, but a subsequent `wopQueryPathInfo` returned the same signature list as before. Harmless in phase 1b's single-node testing (nothing checks signatures yet), but it's a two-line-diff fix and leaving a known-broken handler in the tree is bad precedent.

## Commits

- `0268500` — feat(rio-build): implement AddSignatures persistence (opcode 37)

Single commit. Small.

## Files

```json files
[
  {"path": "rio-build/src/store/traits.rs", "action": "MODIFY", "note": "add Store::add_signatures(path, sigs)"},
  {"path": "rio-build/src/store/memory.rs", "action": "MODIFY", "note": "MemoryStore: set-union sigs into existing PathInfo (dedup matches nix-daemon)"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "handle_add_signatures calls store.add_signatures (was read-and-discard)"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "AddSignatures → QueryPathInfo sequence verifies persistence"},
  {"path": "rio-build/tests/golden_conformance.rs", "action": "MODIFY", "note": "golden test against real daemon"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "opcode 37: stub → implemented"}
]
```

## Design

`Store::add_signatures(path, sigs)` looks up the `PathInfo`, set-unions `sigs` into its signature list (deduplicating — signing twice with the same key doesn't duplicate the entry), writes it back. `MemoryStore` implements this as fetch-modify-write under a write lock. Handler reads path + signature strings from wire, calls the trait method, returns `STDERR_LAST` + `u64(1)`.

The dedup semantics match `nix-daemon`: adding a signature that's already present is a no-op, not an error. Signatures are opaque strings at this layer (base64 ed25519 blob with key name prefix) — no verification, just storage.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.opcode.wopAddSignatures`.

## Outcome

Signatures persist. `AddSignatures` → `QueryPathInfo` round-trip returns the union. One fewer stub opcode. Zero-stub state reached after this — every opcode the gateway recognizes either does real work or is explicitly a no-op by design (CA stubs from P0014, `AddTempRoot` which is semantically void on an in-memory store).
