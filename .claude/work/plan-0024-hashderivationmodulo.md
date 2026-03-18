# Plan 0024: hashDerivationModulo for correct DrvOutput IDs

## Context

P0016 fixed `BuildResult`'s `builtOutputs` encoding to use `DrvOutput` keys. A `DrvOutput` is `<drv-hash>!<output-name>`. P0016 computed `drv-hash` as `SHA-256(to_aterm())` — hash the serialized ATerm. That's wrong. The correct hash is `hashDerivationModulo` — a recursive algorithm that replaces each `inputDrv` key with the modular hash of that input, re-sorts, and hashes the result. Two semantically-identical derivations with different input ordering should have the same modular hash; the naive SHA-256 approach gave them different hashes.

P0020 documented this as a TODO (`b88f4ce`). This plan resolves it. It's the most algorithmically complex code in phase 1b — recursive hashing with cycle detection, memoization, and three distinct derivation-type cases.

## Commits

- `fe7862f` — feat(rio-nix): implement hashDerivationModulo for correct DrvOutput IDs

Single commit. 18 unit tests. Matches Nix C++ `derivations.cc`.

## Files

```json files
[
  {"path": "rio-nix/src/derivation.rs", "action": "MODIFY", "note": "hash_derivation_modulo (pure sync, cycle detection, depth limit 512, memoization); to_aterm_modulo (re-sort inputDrvs by replacement hash keys)"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "with_outputs_from_drv simplified — takes pre-computed drv_hash_hex (was naive SHA-256(to_aterm()))"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "skip output population on resolution/hash failure (don't emit wrong DrvOutput IDs)"},
  {"path": "rio-build/src/gateway/session.rs", "action": "MODIFY", "note": "resolve_transitive_inputs async BFS (DoS limit 10K), feeds hash_derivation_modulo"}
]
```

## Design

**The algorithm.** `hashDerivationModulo(drv)` has three cases by derivation type:

*Fixed-output (FOD):* the output hash is declared in the derivation (fetchers — `fetchurl`, `fetchgit`). Modular hash is `SHA-256("fixed:out:{hash_algo}:{hash}:{path}")`. Base case — doesn't recurse into inputs, because the output is determined by the declared hash regardless of how it was fetched.

*Input-addressed (IA):* the normal case. For each `inputDrv`, recursively compute `hashDerivationModulo(input)`. Replace the `inputDrv` key (a store path) with the hex of that modular hash. Re-sort `inputDrvs` by the new keys. Serialize to ATerm. SHA-256 the result. The re-sort is crucial — it's what makes the hash canonical regardless of input ordering.

*CA floating/impure:* same as IA but with output paths masked to empty string before hashing (the output path depends on the content, which isn't known until after the build).

**Implementation split.** `hash_derivation_modulo` in `rio-nix` is pure and sync — takes a pre-resolved map of `StorePath → Derivation` for all transitive inputs, does the recursive hash with cycle detection (depth limit 512, scope-guard for the visiting-set cleanup), memoizes via `HashMap<StorePath, String>`. The async part — actually fetching those transitive inputs from the store — lives in `rio-build`'s `resolve_transitive_inputs`, a BFS with a 10K-node DoS limit. This split keeps `rio-nix` free of store/async dependencies.

**`to_aterm_modulo`.** The ATerm serializer variant used inside `hash_derivation_modulo`. Same as `to_aterm` except `inputDrvs` keys are replaced by hex hash strings and the map is re-sorted by those strings. Byte-exact reproduction of what Nix's `derivations.cc` hashes.

**Failure handling.** If transitive input resolution fails (cycle, depth limit, store error), the handler skips `builtOutputs` population rather than emitting DrvOutput IDs with wrong hashes. An empty `builtOutputs` is recoverable (client queries separately); a wrong `DrvOutput` ID silently breaks content-addressed derivation resolution downstream.

**Test coverage.** 18 unit tests: FOD base case, leaf IA (no inputs), IA with FOD input (tests the recursion boundary), chained depth-2, multi-output, CA floating, recursive FOD (`r:sha256`), diamond DAG (memoization — same input reached by two paths, hashed once), indirect cycle detection (A→B→A), `to_aterm_modulo` re-sort correctness.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule nix.drv.hashmodulo`, `gw.build.drvoutput`.

## Outcome

`DrvOutput` IDs match Nix. Content-addressed builds can correctly reference input-addressed derivation outputs. The `TODO(phase2)` that `b88f4ce` documented is resolved earlier than planned. Most algorithmically dense code in phase 1b; 18 tests is commensurate.
