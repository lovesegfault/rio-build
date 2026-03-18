# Plan 0020: Review pass 4 — UTF-8 strictness, NAR edge cases, proptest scale

## Context

Fourth and final hardening pass. Feb 24 19:28–19:59, seventeen commits in thirty minutes. Focus shifted from structural (bounds, error paths) to semantic: encoding correctness against Nix's actual behavior. Store path name character sets, NAR empty-file edge cases, `from_utf8_lossy` silent corruption in production code paths. Also cranked proptest iteration count from 256 to 4096 — the review passes had found enough real bugs that more coverage seemed worth the CI time.

## Commits

- `986b33c` — fix(rio-nix): accept '?' and '=' in store path names and fix dot-prefix handling
- `2dfcb27` — fix(rio-nix): reject non-UTF-8 in NAR filesystem serialization
- `58d837d` — fix(rio-build): use from_utf8 instead of from_utf8_lossy in SSH exec and daemon stderr
- `32da69e` — fix(rio-build): validate NAR size in wopAddMultipleToStore entries
- `b492a24` — fix(rio-build): reject unparseable reference paths instead of silently dropping
- `784ad97` — fix(rio-nix): add duplicate detection for CA and FileHash in narinfo parser
- `f31fb50` — fix(rio-nix): handle empty regular files without contents token in NAR parser
- `e11f0c1` — fix(rio-nix): use correct error variants for name and target length in NAR parser
- `5572446` — fix(rio-build): elevate try_cache_drv logging to error level
- `253f98d` — fix(rio-build): parse RIO_DAEMON_TIMEOUT_SECS once via OnceLock
- `4bfd960` — fix(rio-nix): add message count bound to client_build_derivation STDERR loop
- `b88f4ce` — fix(rio-nix): document with_outputs_from_drv hash limitation as TODO
- `fdd0e4b` — test(rio-build): add NAR size mismatch and missing error-path tests
- `9ecd9dc` — test(rio-nix): add narinfo unit tests and proptest roundtrip
- `f07c162` — docs: fix phase1b STDERR_READ description and improve comments
- `428f67c` — perf(rio-build): parse NAR structure in golden test instead of 30s timeout
- `aa5a46d` — test(rio-nix): increase proptest cases from 256 to 4096

## Files

```json files
[
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "allow '?' '=' in names; dot-prefix logic matches Nix checkName() (only '.', '..', '.-*', '..-*' rejected)"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "reject non-UTF-8 fs names (no lossy replace); accept empty regular files without 'contents'; distinct NameTooLong/TargetTooLong errors"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "CA + FileHash dup detection; proptest roundtrip"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "message-count bound on client_build_derivation STDERR loop"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "TODO(phase2): with_outputs_from_drv hash limitation documented"},
  {"path": "rio-nix/src/protocol/derived_path.rs", "action": "MODIFY", "note": "proptest dup-name flake fix; iterations 256→4096"},
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "proptest iterations 256→4096"},
  {"path": "rio-nix/src/derivation.rs", "action": "MODIFY", "note": "proptest iterations 256→4096"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "NAR size validation in opcode 44; reject unparseable refs (was filter_map skip); try_cache_drv error-level log; RIO_DAEMON_TIMEOUT_SECS via OnceLock"},
  {"path": "rio-build/src/gateway/server.rs", "action": "MODIFY", "note": "SSH exec from_utf8 (channel_failure on non-UTF-8, not lossy match)"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "NAR size mismatch tests for opcode 44; reference-path rejection tests"},
  {"path": "rio-build/tests/golden/daemon.rs", "action": "MODIFY", "note": "parse NAR structure to detect completion (was sleep 30s)"},
  {"path": "rio-build/tests/golden_conformance.rs", "action": "MODIFY", "note": "use new golden daemon completion detection"},
  {"path": "docs/src/phases/phase1b.md", "action": "MODIFY", "note": "fix STDERR_READ description"}
]
```

## Design

**Store path name character set (`986b33c`).** `is_valid_name_char` matched the documented Nix charset but not the implemented one. Nix's `checkName()` in `libstore/path.cc` allows `?` and `=` — which appear in real store paths like `perl-5.38.2?man`. The dot-prefix logic was also wrong: rio rejected all names starting with `.`, but Nix only rejects `.`, `..`, `.-<anything>`, `..-<anything>`. `.gitignore` is a valid store path name. The fix matches `path.cc` line by line.

**UTF-8 strictness (`2dfcb27`, `58d837d`).** `to_string_lossy()` never fails — it silently replaces non-UTF-8 bytes with U+FFFD. When NAR `dump_path` walked a filesystem with a non-UTF-8 filename, it serialized a different filename than `nix-store --dump` would (nix errors; rio mangled). The resulting NAR hash diverges. Fixed: `OsString::into_string()` returns `Result`, non-UTF-8 is an error. Same fix in SSH `exec_request` (a non-UTF-8 command string was being lossy-converted then pattern-matched — a crafted command could produce a different match after mangling).

**Silent-drop elimination (`b492a24`, `5572446`).** `PathInfo` reference parsing used `.filter_map(|s| StorePath::from_str(s).ok())` — an unparseable reference just vanished from the list. The store path was accepted with incomplete reference metadata. Changed to `collect::<Result<Vec<_>>>` — one bad reference fails the whole upload with a diagnosable error. `try_cache_drv` debug-level logging meant cache-population failures were invisible in production — elevated to error level after a debugging session where the cache was silently empty.

**NAR empty-file edge case (`f31fb50`).** The NAR format permits `( type regular )` with no `contents` token for empty files. The parser required `contents` and errored. Real nix produces these for empty files in some cases. Fixed: `)` after `regular` returns an empty non-executable file.

**Proptest scale (`aa5a46d`).** 256 iterations had been the default since P0002. After four review passes found bugs that proptests theoretically could have caught with more iterations, bumped to 4096 across all seven proptest modules. CI time cost ~20 seconds; found one pre-existing `DerivedPath` proptest flake (duplicate output names could be generated) which `f31fb50` also fixed.

**Golden test perf (`428f67c`).** Golden tests waited 30 seconds for the daemon to finish writing a NAR response before comparing. Wasteful — the response is a NAR stream with a well-defined terminator. Now parses the stream structure and stops when the root entry closes. ~30s → ~100ms per golden test.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Cross-cutting hardening; no distinct retro-scope.

## Outcome

Last small-fix pass. Store path charset matches `libstore/path.cc`. No `from_utf8_lossy` in production code paths. No `filter_map(...).ok()` silent drops. NAR parser handles empty-file edge case. Proptests at 4096 iterations. Golden tests ~30s faster. After this: tail features (P0021–P0025) and phase boundary.
