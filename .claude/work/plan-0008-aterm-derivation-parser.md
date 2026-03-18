# Plan 0008: ATerm derivation parser for .drv files

## Context

Phase 1b's goal was end-to-end single-node build via `ssh-ng://`. Every build starts with a `.drv` file — the ATerm-encoded derivation that says "run this builder with these inputs to produce these outputs." Without a parser for that format, the gateway can't answer `wopQueryDerivationOutputMap` (what outputs does this derivation produce?) or forward `wopBuildDerivation` (here's the derivation, build it). This is the first of three foundational parsers (ATerm, NAR, narinfo) that the phase 1b doc lists as prerequisites to the build opcodes.

ATerm is a peculiar format: S-expressions with Annotated Terms, originally from the Stratego/XT program-transformation world. Nix uses a tiny fixed subset — just enough to encode the `Derive(...)` constructor with nested tuples and lists. No schema, no version field, no extensibility. Every `.drv` file starts with the literal string `Derive(` and ends with `)`. The parser is a hand-written recursive descent because the grammar is so rigid that a combinator library would add more surface area than it removes.

## Commits

- `81c8d0e` — feat(rio-nix): add ATerm derivation parser for .drv files

Single commit. New module `rio-nix/src/derivation.rs` (~600 lines with tests).

## Files

```json files
[
  {"path": "rio-nix/src/derivation.rs", "action": "NEW", "note": "ATerm recursive-descent parser + serializer; Derivation, BasicDerivation, DerivationOutput types"},
  {"path": "rio-nix/src/lib.rs", "action": "MODIFY", "note": "pub mod derivation"},
  {"path": "docs/src/phases/phase1b.md", "action": "MODIFY", "note": "mark ATerm task complete"}
]
```

## Design

Three types model the derivation structure. `Derivation` is the full on-disk form with `inputDrvs` — a map from input `.drv` paths to the set of output names consumed from each. `BasicDerivation` is the wire subset (used by `wopBuildDerivation`) that drops `inputDrvs` because the wire protocol sends input paths already resolved. `DerivationOutput` represents one output slot: a name, a store path, and optionally a fixed-output hash (for fetchers whose output hash is known in advance).

The ATerm string escaping rules are minimal: `\n`, `\r`, `\t`, `\\`, `\"`. No octal escapes, no unicode escapes. The parser tracks position for error messages but doesn't produce a full AST — it builds `Derivation` directly, one field at a time, with the grammar hardcoded into the function call structure. `parse_derivation` calls `parse_outputs`, which calls `parse_output`, which calls `parse_string` four times. If the format ever changed (it hasn't since 2003) the parser would need rewriting.

Serialization is the inverse: `to_aterm()` produces a byte-exact round-trip of what was parsed. Output ordering is preserved — the `.drv` format is order-sensitive because the hash of the ATerm bytes is what determines the derivation's store path. Two derivations with the same outputs in different orders have different store paths.

Golden tests compare the parser's output against real `.drv` files produced by `nix-instantiate`: a trivial `writeText` derivation and `nixpkgs#hello`. The `hello` case exercises multi-output and long environment-variable lists. Both tests re-serialize and verify byte identity with the original — any drift in escaping or ordering would break the store path computation downstream.

## Tracey

Predates tracey adoption. No `r[impl ...]` annotations present at `phase-1b` tag. Retro-tagged scope: `tracey query rule nix.drv.aterm` (ATerm format), `nix.drv.output` (DerivationOutput invariants).

## Outcome

ATerm parse + serialize round-trips byte-identical against `nix-instantiate` output. `BasicDerivation` type ready for wire serialization (P0011). `Derivation` type ready for the per-session `.drv` cache (P0012). The phase 1b Month 4 milestone ("ATerm derivation parser passes property-based tests against Nix-generated .drv files") is structurally satisfied; proptests arrive in P0014.
