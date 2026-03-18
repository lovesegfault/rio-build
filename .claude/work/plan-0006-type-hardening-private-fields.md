# Plan 0006: Type hardening — private fields, silent failures, write bounds

## Context

Second cross-cutting review pass, after P0005's golden tests were green. The findings clustered around three themes: types that could be constructed in invalid states, errors that were dropped on the floor instead of logged, and `debug_assert!` where runtime checks were needed. The phase 1a doc doesn't list this as a task item — it's the polish layer between "works" and "works correctly under adversarial inputs."

The private-fields discipline started in P0004 (`NixHash`, `StorePath`) and extends here to every other wire type: `PathInfo`, `ClientOptions`, `StderrError`, `HandshakeResult`. The pattern is the same — fields were public, callers could construct `PathInfo { nar_size: 0, ... }` directly, validation happened at parse time only. Private fields plus builder/constructor functions means invalid states are unrepresentable.

## Commits

- `c0413ce` — refactor: harden type design, fix silent failures, and remove dead code
- `5d0c0e1` — refactor: harden type design, fix silent failures, and expand test coverage
- `da5b68f` — refactor: enforce wire write-side bounds, add warn logging for dropped paths, simplify MemoryStore and StderrWriter

Three commits, contiguous. `c0413ce` and `5d0c0e1` share a subject prefix — both are review-response commits addressing different finding batches.

## Files

```json files
[
  {"path": "rio-nix/src/protocol/opcodes.rs", "action": "MODIFY", "note": "PathInfo/ClientOptions private fields, builder validates SHA-256 + nonzero nar_size"},
  {"path": "rio-nix/src/protocol/handshake.rs", "action": "MODIFY", "note": "HandshakeResult private fields + accessors, fix phase-number comments to match 4-phase spec"},
  {"path": "rio-nix/src/protocol/stderr.rs", "action": "MODIFY", "note": "StderrError private fields, StderrWriter tracks finished state (guard inner_mut pre-finish)"},
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "write-side runtime bounds checks (was debug_assert!), position-writing helper extraction"},
  {"path": "rio-nix/src/protocol/derived_path.rs", "action": "MODIFY", "note": "OutputSpec::Names enforces non-empty at construction, PartialEq/Eq derives"},
  {"path": "rio-nix/src/hash.rs", "action": "MODIFY", "note": "FromStr for HashAlgo, hex::encode (was manual loop)"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "TryFrom/Display impls, hex::encode"},
  {"path": "rio-build/src/store/memory.rs", "action": "MODIFY", "note": "single RwLock (was two separate locks), HashSet<StorePath> for temp_roots, import_from_nix_store returns Result with context, lock-recovery helper"},
  {"path": "rio-build/src/store/traits.rs", "action": "MODIFY", "note": "temp_roots signature takes &StorePath not String"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "warn! on dropped parse errors in filter_map chains, close connection on missing NAR, log STDERR_ERROR send failures"},
  {"path": "rio-build/src/gateway/session.rs", "action": "MODIFY", "note": "dead SSH channels cleaned up immediately (was deferred to next poll)"},
  {"path": "rio-build/src/gateway/server.rs", "action": "MODIFY", "note": "tighten exec command validation, log SSH pump I/O failures, fail fast on empty authorized_keys"},
  {"path": "rio-build/src/observability.rs", "action": "MODIFY", "note": "init_logging returns Result (was panic on invalid RUST_LOG filter)"},
  {"path": "rio-build/src/main.rs", "action": "MODIFY", "note": "remove unused rio-proto dep"},
  {"path": "rio-build/tests/golden_conformance.rs", "action": "MODIFY", "note": "tests updated for private-field constructors"},
  {"path": "rio-build/tests/golden/mod.rs", "action": "MODIFY", "note": "tests updated for private-field constructors"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "PathInfo validation tests, NarFromPath error tests"}
]
```

## Design

**Private fields with validated constructors.** `PathInfo` previously: `pub nar_hash: NixHash, pub nar_size: u64, ...`. After: private fields, a `PathInfo::builder()` that validates `nar_hash.algo() == Sha256` and `nar_size > 0` before constructing. Same pattern for `ClientOptions` (verbosity bounded), `StderrError` (message non-empty), `HandshakeResult` (negotiated version in supported range). Tests that previously constructed these with struct literals now use the builder, which means tests can't accidentally test against invalid states either.

**Eight silent-failure patterns fixed.** The recurring shape: `.filter_map(|x| x.parse().ok())` — parse failures are dropped with no log. Every one of these now has a `.inspect_err(|e| tracing::warn!(?e, "dropping malformed ..."))` before the `.ok()`. Other instances: `STDERR_ERROR` send failures (the client is probably gone, but log it anyway), SSH pump I/O failures (closed connection is normal, but EIO is a bug), missing NAR in `wopNarFromPath` (previously returned empty bytes, now closes with an error). The principle codified: dropping a value is fine, dropping *information about why* is not.

**Write-side runtime bounds.** P0000's wire module enforced 64 MiB / 1M-element limits on *reads* — parsing defends against malicious input. Writes used `debug_assert!` — fine in debug builds, no-op in release. But if `rio-build` ever constructs a 2M-element string list internally (bug in handler code), a release build would happily serialize it. `da5b68f` promotes these to runtime checks. Writing is trusted code, but trusted code has bugs.

**Single `RwLock` for `MemoryStore`.** P0001 had two maps under two locks: `RwLock<HashMap<StorePath, PathInfo>>` and `RwLock<HashMap<StorePath, Vec<u8>>>`. Insert acquired both sequentially, so between the two acquires another reader could see the `PathInfo` but not the NAR bytes. Consolidated into one `RwLock<MemoryStoreInner>` holding both maps — insert is now atomic. Also: poisoned-lock recovery helper (`recover_poisoned()`) so a panic in one handler doesn't brick the whole store.

**`StderrWriter` finished-state tracking.** A session that sends `STDERR_LAST` must not send more STDERR frames after — the client has moved on to reading the result. The original `StderrWriter` didn't track this; you could call `write_next()` after `write_last()` and desync the stream. Added a `finished: bool` flag, `inner_mut()` returns `None` after finish, callers get a clear "already finished" error instead of a silent stream corruption.

**Dead code removal.** Unused `bytes` crate dep. Unused `rio-proto` dep from `rio-build` (the gRPC stub wasn't referenced yet). Empty placeholder modules (`derivation.rs`, `nar.rs`, `narinfo.rs`) that P0004's `b1f7cf2` created as stubs per crate-structure.md but never filled — deleted (they came back properly in phase 1b).

39 new tests in `5d0c0e1` plus more in `da5b68f`: `PathInfo` builder rejection cases (wrong hash algo, zero size), `OutputSpec::Names` empty-vec rejection, `StderrWriter` post-finish access, wire write bounds rejection, `NarFromPath` error responses.

## Tracey

Predates tracey adoption. The type invariants enforced here are cross-cutting — they don't map neatly onto single spec markers. Retro-tagged scope is broad: `gw.wire.*` (write bounds), `store.memory.*` (single-lock atomicity), `gw.stderr.*` (finished-state tracking).

## Outcome

143 tests total (up from 104). Every wire type has private fields and validated constructors. Every `.ok()` that drops an error has a `warn!` in front of it. Write-side bounds are runtime-enforced. `MemoryStore` inserts are atomic. `StderrWriter` can't send post-finish. Dead code deleted. This is the state of `rio-nix` and `rio-build` at the `phase-1a` tag for the gateway track.
