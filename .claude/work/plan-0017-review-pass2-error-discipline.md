# Plan 0017: Review pass 2 — error-path discipline, type tightening, constructor validation

## Context

Second review pass. Feb 24 04:30–05:11, twenty commits. P0014 caught the first layer of bounds/error issues; this pass went deeper: error paths that P0014 missed because they were in code paths not yet exercised, types that allowed invalid construction, tests that hardcoded old wire formats after P0016 changed them. The largest block of commits in the phase — twenty small fixes, no new features.

## Commits

- `14a28e3` — fix(rio-build): send STDERR_ERROR before all handler error returns
- `4658f43` — fix(rio-build): harden daemon subprocess lifecycle
- `507171a` — docs: sync gateway.md and phase1b.md with implementation
- `e92c810` — test(rio-build): add byte-level and error-path tests for Phase 1b opcodes
- `08c052d` — test(rio-nix): add proptest roundtrip for NAR parser
- `cd75a81` — fix(rio-build): wrap read_basic_derivation in STDERR_ERROR handler
- `5f26ca6` — fix(rio-nix): return WireError on malformed Realisation JSON in read_build_result
- `996054a` — fix(rio-build): correct wopRegisterDrvOutput wire format for protocol >= 1.31
- `2f807a4` — fix(rio-build): use from_utf8 instead of from_utf8_lossy in try_cache_drv
- `17e675d` — fix(rio-build): add per-entry context to parse_add_multiple_entry errors
- `7387932` — fix(rio-nix): add iteration bound to drain_stderr
- `4c5a2d6` — fix(rio-build): add debug logging for silent fallback paths
- `cbcc84f` — docs: fix wopAddMultipleToStore entry metadata and extract_single_file doc
- `b5362d1` — fix(rio-nix): add doc warnings to read_u8/write_u8 and protocol version comments
- `cdfe08b` — test(rio-build): add missing byte-level and error-path tests for Phase 1b opcodes
- `2585dc1` — test(rio-nix): add NAR unsorted-entries rejection and DerivedPath roundtrip tests
- `4c47e5c` — refactor(rio-nix): validate DerivationOutput::new() and NarInfoBuilder::build()
- `86a2b02` — refactor(rio-nix): make read/write_basic_derivation accept BasicDerivation directly
- `e2fed62` — refactor(rio-nix): use Option for NarInfo optional fields
- `c812834` — fix(rio-build): update test_ca_opcodes_accepted_as_noop for corrected wire format

## Files

```json files
[
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "more STDERR_ERROR wraps; daemon kill-on-stdio-failure; from_utf8 not lossy; per-entry error context; debug logging on fallback paths"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "byte-level + error-path coverage for opcodes 7/8/10/39/42/44; CA opcode wire-format update"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "WireError on Realisation JSON parse failure; BasicDerivation direct in read/write (not separate fields); doc warnings on read_u8"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "iteration bound on drain_stderr"},
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "read_u8/write_u8 doc warnings (not on Nix wire)"},
  {"path": "rio-nix/src/protocol/derived_path.rs", "action": "MODIFY", "note": "roundtrip proptest"},
  {"path": "rio-nix/src/derivation.rs", "action": "MODIFY", "note": "DerivationOutput::new() → Result (rejects empty name)"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "NarInfoBuilder::build() validates required fields; Option<String> for deriver/ca/file_hash"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "proptest roundtrip; unsorted-entries rejection test"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "sync opcode table + wopAddMultipleToStore entry format"}
]
```

## Design

Categorized findings:

**Error-path completion.** P0014 wrapped the obvious error returns; this pass caught the ones behind successful-looking calls. `read_basic_derivation` could fail if the wire data was malformed — it was called with `?`. `read_framed_stream` similarly. `try_cache_drv` used `from_utf8_lossy` which never fails but silently replaces bad UTF-8 with U+FFFD — the `.drv` content is then unparseable and the cache silently stays empty. Changed to `from_utf8`: now a non-UTF-8 NAR produces a diagnosable error.

**Constructor validation.** `DerivationOutput { name: "".into(), ... }` compiled fine. The ATerm parser rejected empty names, but `DerivationOutput::new("")` didn't — two enforcement paths, one of them was open. `new()` now returns `Result`. Same for `NarInfoBuilder::build()`: the parser required `StorePath`/`URL`/`Compression`/`NarHash`, but the builder would happily produce a narinfo with none of them. Now validates. `NarInfo` optional fields (`deriver`, `ca`, `file_hash`, `file_size`) went from sentinel-empty-string/zero to `Option<T>` — absence is now a distinct value from present-but-empty.

**Wire ergonomics.** `read_basic_derivation` took seven separate parameters (name, builder, args, ...) and constructed a `BasicDerivation` internally. `write_basic_derivation` destructured one back into seven. Changed both to take/return `BasicDerivation` directly — simpler call sites, fewer places to get the field order wrong.

**Daemon lifecycle.** `stdin.take()` returning `None` after spawn is rare but possible (pipe setup race). Previously: `?`-propagated, daemon process leaked. Now: kill the daemon first, then error. `daemon.kill()` failures are logged (they shouldn't happen, but silently discarding them hides OS-level problems).

**CA stub correction.** P0014's `wopRegisterDrvOutput` stub read the payload as two strings. Protocol ≥1.31 sends a single `Realisation` JSON string. Fixed — the stub was desyncing the stream.

**Test expansion.** NAR proptest (parse → serialize → parse, compare trees). NAR unsorted-entries rejection (directory entries must be byte-order sorted; a NAR with entries out of order is invalid). DerivedPath roundtrip proptest. Byte-level + error-path tests for the opcodes P0015 added.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Cross-cutting hardening; no distinct retro-scope.

## Outcome

Twenty small fixes, no surface-area change. Constructor invariants enforced. UTF-8 handling strict. Daemon lifecycle doesn't leak processes on rare-path failures. Test coverage caught up with the conformance fixes from P0015/P0016.
