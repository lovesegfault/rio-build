# Plan 0000: Nix wire protocol scaffold

## Context

Phase 1a's goal was to prove the `ssh-ng://` protocol approach with a read-only store before committing to the full architecture. This plan is the foundational layer: the pure types and wire encoding that every other component depends on. Nothing runtime here — no SSH server, no store backend, no metrics. Just the vocabulary the rest of the system speaks.

The phase doc (`docs/src/phases/phase1a.md`) lists these as separate task items: "rio-nix wire format", "Store path types", "Hash types", "Handshake", "STDERR streaming loop". They landed together in a single scaffold commit because each references the others — `Opcode` uses `StorePath`, `Handshake` uses `wire::read_u64`, `StderrMessage` uses `wire::write_bytes`. There was no meaningful intermediate state where only some of these existed.

## Commits

- `469ab1d` — feat: scaffold Phase 1a workspace with wire format, protocol types, and store path primitives

Single commit. 5,160 insertions, 28 deletions. Established the 3-crate workspace (`rio-nix`, `rio-build`, `rio-proto`).

## Files

```json files
[
  {"path": "rio-nix/src/protocol/wire.rs", "action": "NEW", "note": "u64-LE primitives, 8-byte padding, length-prefixed bytes/strings/collections"},
  {"path": "rio-nix/src/protocol/handshake.rs", "action": "NEW", "note": "4-phase server handshake (magic, version, features, trust)"},
  {"path": "rio-nix/src/protocol/stderr.rs", "action": "NEW", "note": "8-message STDERR loop (NEXT/READ/WRITE/LAST/ERROR/START_ACTIVITY/STOP_ACTIVITY/RESULT)"},
  {"path": "rio-nix/src/protocol/opcodes.rs", "action": "NEW", "note": "Opcode enum, PathInfo, ClientOptions wire types"},
  {"path": "rio-nix/src/protocol/mod.rs", "action": "NEW", "note": "module root"},
  {"path": "rio-nix/src/hash.rs", "action": "NEW", "note": "NixHash (SHA-256/512/1), truncation, colon-format parsing"},
  {"path": "rio-nix/src/store_path.rs", "action": "NEW", "note": "StorePath parsing + nixbase32 encode/decode"},
  {"path": "rio-nix/src/lib.rs", "action": "NEW", "note": "crate root"},
  {"path": "rio-build/src/main.rs", "action": "NEW", "note": "binary entry stub"},
  {"path": "rio-build/src/lib.rs", "action": "NEW", "note": "lib root"},
  {"path": "rio-proto/src/lib.rs", "action": "NEW", "note": "gRPC proto stub (empty in 1a)"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "workspace members: rio-nix, rio-build, rio-proto"}
]
```

## Design

The wire format follows the Nix daemon protocol's "everything is a u64" discipline: booleans are `u64(0)`/`u64(1)`, opcode numbers are u64, collection counts are u64, byte-string lengths are u64. Strings and byte vectors are length-prefixed and 8-byte-padded (so a 3-byte string writes as 8 bytes of length + 3 bytes of content + 5 bytes of zero padding). The design doc baseline landed in `ffbbd8a` (55 files in `docs/src/`) — `docs/src/components/gateway.md` is the primary spec for what landed here.

Safety bounds were built in from the first commit: 64 MiB max string length, 1M max collection count. These are checked on the read side before allocation. (Write-side enforcement came later in P0006.)

The `StorePath` type enforces the `/nix/store/<32-char-nixbase32-hash>-<name>` format at construction time. The 211-character name limit (Nix's own bound) was added in P0002; this commit has the basic format validation only.

The `nixbase32` implementation uses the custom Nix alphabet (`0123456789abcdfghijklmnpqrsvwxyz` — deliberately omits `e`, `o`, `u`, `t` to avoid accidental offensive strings). Note that Nix reads nixbase32 right-to-left, which is why the encode loop walks the hash bytes in reverse.

Handshake is the 4-phase sequence: magic exchange, version negotiation (min 1.37 — older clients are not worth supporting), feature list (protocol ≥1.38 only), then trust status + STDERR_LAST. This version got the phase structure right but had the magic bytes wrong (u32 instead of u64) — fixed in P0002 when integration testing revealed the bug.

39 tests passed at this commit: unit tests for each wire primitive, `StorePath` parse/display roundtrips, nixbase32 encode/decode, hash truncation. No proptests yet (added in P0002).

## Tracey

Predates tracey adoption. Phase 1a was developed before the `r[domain.*]` spec-marker system existed; no `r[impl ...]` or `r[verify ...]` annotations are present in the tree at the `phase-1a` tag. The spec markers in `docs/src/components/gateway.md` (`r[gw.handshake.*]`, `r[gw.wire.*]`) and the corresponding `// r[impl ...]` comments in `rio-nix/src/protocol/` were added retroactively in later phases.

To find the retro-tagged implementation sites for this plan's scope: `tracey query rule gw.wire` and `tracey query rule gw.handshake`.

## Outcome

39 tests green. The 3-crate workspace compiles. Every opcode handler, every SSH session, every store lookup in subsequent plans sits on top of these types. No runtime behavior yet — that arrives in P0001.
