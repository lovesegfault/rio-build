# Plan 0019: Review pass 3 — parser bounds, STDERR_READ client handling, fuzz expansion

## Context

Third review pass. Feb 24 07:56–17:18, sixteen commits spread across the day. Deeper than P0017: the NAR and ATerm parsers got DoS bounds (they'd been missed — P0014 only covered wire.rs loops), the client protocol got a fix for a 1-hour hang mode, and fuzz coverage expanded to `NarInfo` and `BuildResult`.

## Commits

- `8e8a16c` — fix(rio-build): harden daemon subprocess lifecycle
- `cc4431e` — fix(rio-build): send STDERR_ERROR before all handler error returns
- `e66bd5f` — fix(rio-nix): add bounds to NAR directory entries and ATerm collection sizes
- `f8e6d21` — test(rio-build): add byte-level and error-path tests for Phase 1b opcodes
- `cd71438` — docs: sync gateway.md and phase1b.md with implementation
- `6333ca5` — refactor(rio-build): extract resolve_derivation helper for drv cache lookups
- `90ac94d` — test(rio-nix): add NarInfo fuzz target
- `0f4bba9` — refactor(rio-build): minor code simplifications
- `80690bf` — fix(rio-build): wrap read_framed_stream in STDERR_ERROR handler
- `b9567a6` — fix(rio-nix): handle STDERR_READ in client protocol and reap daemon processes
- `7ca8001` — fix(rio-nix): add NarInfo Deriver duplicate check and fix ATerm parser construction
- `3570ce8` — fix(rio-nix): reject duplicate output names in OutputSpec and validate BasicDerivation
- `ac0d34b` — test(rio-nix): add BuildResult and BasicDerivation fuzz target
- `de79ba0` — test(rio-build): add missing byte-level and error-path tests
- `25ca85d` — docs: fix comment accuracy and remove stale C++ line references
- `97cb9bf` — fix(rio-build): reject NAR size mismatch in wopAddToStoreNar

## Files

```json files
[
  {"path": "rio-nix/src/derivation.rs", "action": "MODIFY", "note": "MAX_ATERM_LIST_ITEMS (1M) on parse_string_list/parse_outputs/parse_input_drvs/parse_env; use DerivationOutput::new() in parser"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "MAX_DIRECTORY_ENTRIES (1M) in parse_directory"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "Deriver duplicate detection"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "drain_stderr + client_build_derivation explicitly handle STDERR_READ (was silent drop → 1h hang)"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "BasicDerivation::new() → Result (rejects empty outputs)"},
  {"path": "rio-nix/src/protocol/derived_path.rs", "action": "MODIFY", "note": "OutputSpec::names() rejects duplicate names via HashSet"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "resolve_derivation helper (dedup ~130 lines from 3 handlers); daemon.wait() after kill(); NAR size mismatch → STDERR_ERROR"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "NAR size mismatch test; more error-path coverage"},
  {"path": "rio-nix/fuzz/Cargo.toml", "action": "MODIFY", "note": "add narinfo_parsing, build_result_parsing fuzz bins"},
  {"path": "rio-nix/fuzz/fuzz_targets/narinfo_parsing.rs", "action": "NEW", "note": "fuzz narinfo text parser"},
  {"path": "rio-nix/fuzz/fuzz_targets/build_result_parsing.rs", "action": "NEW", "note": "fuzz read_build_result + read_basic_derivation"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "sync opcode status"}
]
```

## Design

**Parser bounds (`e66bd5f`).** P0014 bounded every loop in `wire.rs`. It missed the parsers: NAR `parse_directory` looped `while token == "entry"` with no count limit — a crafted NAR with a million empty entries OOMs the parser before any single allocation trips a size check. ATerm had the same issue in all four list-parsing methods. Both now enforce 1M-item bounds (`MAX_DIRECTORY_ENTRIES`, `MAX_ATERM_LIST_ITEMS`) matching the wire.rs convention.

**Client STDERR_READ hang (`b9567a6`).** `drain_stderr` and `client_build_derivation` read STDERR frames from the daemon in a loop until `STDERR_LAST`. They had a `match` arm for every STDERR type except `STDERR_READ` — which fell through to the catch-all "unknown message type, continue". When the daemon sent `STDERR_READ` (asking the client to send data), the client read it, didn't respond, and waited for the next message. The daemon waited for the response. Both sides blocked until the 1-hour timeout. Now `STDERR_READ` produces an explicit error — the client protocol never sends data on request in phase 1b's design, so receiving `STDERR_READ` is a bug to report, not silently drop.

Also in `b9567a6`: `daemon.wait()` after `daemon.kill()`. Without wait, the kernel keeps the process table entry around as a zombie until the parent exits. One build = one zombie; a long-running gateway accumulates them.

**`resolve_derivation` helper (`6333ca5`).** Three handlers (`query_derivation_output_map`, `build_paths`, `build_paths_with_results`) had ~45 lines each of identical "check cache → fetch NAR → extract file → parse ATerm → cache" logic, differing only in error handling. Extracted into one helper that returns `Result<Derivation>`; each caller matches the `Err` case its own way (STDERR_ERROR / return Err / push failure). ~130 lines deleted.

**Validation constructors (`7ca8001`, `3570ce8`).** More of P0017's constructor-validation work. `BasicDerivation::new()` rejects empty output lists (every Nix derivation has ≥1 output). `OutputSpec::names()` rejects duplicate output names (`path!out,out` is meaningless). `NarInfo` rejects duplicate `Deriver:` lines (was the only single-value field without a dup check).

**NAR size validation (`97cb9bf`).** `wopAddToStoreNar` received `nar_size` as metadata, then read the framed stream. If the actual stream length didn't match `nar_size`, the handler happily stored it with the wrong metadata. Now compares and STDERR_ERRORs on mismatch — a client lying about size is a client to distrust.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Cross-cutting hardening; no distinct retro-scope.

## Outcome

NAR and ATerm parsers have DoS bounds. Client protocol can't silently hang for an hour on unexpected STDERR_READ. Daemon processes are reaped. Three handlers deduplicated. Five fuzz targets total (wire, derivation, nar, narinfo, build_result). All constructor paths validate.
