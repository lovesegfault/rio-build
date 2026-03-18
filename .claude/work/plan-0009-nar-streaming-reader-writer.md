# Plan 0009: NAR archive streaming reader/writer

## Context

NAR (Nix ARchive) is Nix's content-addressed archive format. Every store path uploaded to the gateway arrives as a NAR stream; every store path downloaded leaves as a NAR stream. Phase 1a (P0001) shipped `wopNarFromPath` as a stub that read pre-stored NAR bytes from `MemoryStore` — it never had to produce a NAR from a filesystem tree or parse one back. Phase 1b's upload opcodes (`wopAddToStoreNar`, `wopAddMultipleToStore`) need both directions: parse incoming NARs to validate their hash, and eventually serialize filesystem trees back into NARs for worker→store upload.

The phase 1b doc lists this as "NAR format: streaming reader and writer, synchronous `Read`/`Write`-based, golden-tested against `nix-store --dump`." Synchronous was a deliberate choice: the NAR format itself is strictly sequential (no random-access offsets, no trailing index), so async adds nothing over "wrap the sync parser in `spawn_blocking`." The async streaming work came later (P0025) at the I/O layer, not the format layer.

## Commits

- `cd43d76` — feat(rio-nix): add NAR archive streaming reader and writer

Single commit. New module `rio-nix/src/nar.rs`.

## Files

```json files
[
  {"path": "rio-nix/src/nar.rs", "action": "NEW", "note": "NAR parse/serialize; dump_path (fs→NAR), extract_to_path (NAR→fs), extract_single_file (.drv extraction)"},
  {"path": "rio-nix/src/lib.rs", "action": "MODIFY", "note": "pub mod nar"},
  {"path": "rio-nix/Cargo.toml", "action": "MODIFY", "note": "add tempfile dev-dep for fs roundtrip tests"},
  {"path": "docs/src/phases/phase1b.md", "action": "MODIFY", "note": "mark NAR task complete"}
]
```

## Design

The NAR format is a tagged-union tree encoded as length-prefixed strings. The root is always `( type regular ... )` or `( type symlink ... )` or `( type directory ... )`. Tokens are Nix-wire-format strings (u64 length + padded bytes) — the same encoding as `protocol/wire.rs` but over `std::io::Read`/`Write` instead of tokio `AsyncRead`/`AsyncWrite`. Directories contain `entry ( name <name> node <recurse> )` for each child, sorted by name ascending (byte-order, not locale).

Four entry points:

- `parse(reader)` → `NarEntry` tree in memory. Used to validate incoming NARs and to extract single files.
- `serialize(entry, writer)` → byte stream. Inverse of parse.
- `dump_path(fs_path, writer)` — equivalent to `nix-store --dump`. Walks a filesystem tree, sorts directory entries, emits NAR.
- `extract_to_path(reader, fs_path)` — equivalent to `nix-store --restore`. Parses NAR, creates files/symlinks/directories.

`extract_single_file` is a specialization: when the gateway receives a `.drv` file wrapped in a NAR (which is how `nix copy` sends them — store paths are always NAR-wrapped on the wire), it needs to pull out the file contents to pass to the ATerm parser. NAR-wrapping a single file adds ~112 bytes of overhead (`( type regular contents <len> <data> )`), so this is effectively "skip the header, return the contents."

Golden tests compare `dump_path` byte-for-byte against `nix-store --dump` output for both a single file and a directory with mixed entries (regular, executable, symlink, nested subdirectory). Filesystem roundtrip tests verify `dump_path` → `extract_to_path` recreates an identical tree. This is the Month 4 milestone from the phase doc: "NAR round-trip (read then write then read) is byte-identical."

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule nix.nar.format`, `nix.nar.dump`, `nix.nar.extract`.

## Outcome

`dump_path` matches `nix-store --dump` byte-for-byte. Parse/serialize round-trips. `extract_single_file` ready for `.drv` cache population in P0012. The synchronous-only design meant no integration with the async opcode handlers yet — that's P0012's job (wrap in `spawn_blocking`) and P0025's job (true streaming).
