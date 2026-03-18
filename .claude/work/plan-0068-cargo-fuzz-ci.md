# Plan 0068: cargo-fuzz CI wiring + fuzzer-found fixes + CI aggregates

## Design

Fuzz targets existed (P0004 added them) but were **unusable**: no nightly toolchain, no `cargo-fuzz` in the dev shell, no `Cargo.lock` for the excluded fuzz crate, no seed corpus, no CI derivation. This plan fixed all of that — and the fuzzer immediately found two real bugs on its first run.

**Toolchain split** (`b2321db`): `rustStable` (from `rust-toolchain.toml`) remains the source of truth for CI builds — clippy/nextest/workspace use it via `craneLib`. `rustNightly` via `selectLatestNightlyWith` now backs the **default** dev shell (so `cargo fuzz` works with zero setup) and the fuzz-build derivations. New `devShells.stable` for CI-parity dev. The fuzz crate became its own workspace root (`[workspace]` in `fuzz/Cargo.toml`) with a separate committed `Cargo.lock` (69 packages). Seed corpus with `seed-` prefix in `fuzz/corpus/<target>/` (the prefix is load-bearing — `.gitignore` excludes everything *except* `seed-*` so `cargo fuzz run` discoveries aren't accidentally committed). Per-target CI derivations: `fuzz-<target>-smoke` runs 30s each (later bumped from 10s in `4d47d32`).

**The fuzzer immediately found a real DoS** (`0d4a1cc`): a ~50-byte crafted NAR claiming a ~4 GiB content length triggered `vec![0u8; len]` → ~4 GB allocation *before* `read_exact` could fail on EOF. libFuzzer reproducer: `==ERROR: libFuzzer: out-of-memory (malloc(4143972347))`. The old bound (4 GiB) was "sound in theory but unsafe in practice" for an eager-allocation parser. Lowered to 256 MiB — anything larger should use the streaming NAR API, not the in-memory parse. This is *why the fuzz infrastructure exists*.

**And a second crash** (`00c7b2b`): `StorePath::parse` did `rest.split_at(HASH_CHARS)` where `HASH_CHARS=32`. If byte offset 32 falls mid-UTF-8-character, `split_at` panics. A valid nixbase32 hash is always ASCII so multi-byte input is already invalid — but the validation happened *after* the panic. Added an `is_char_boundary` guard so malformed paths error out cleanly instead of crashing the process.

**Fuzz parallelization** (`4d47d32`): `-fork=$NIX_BUILD_CORES` so each fuzz-check derivation uses all allocated cores instead of running single-threaded. Workers write to `fuzz-*.log`; on failure those are dumped to the build log so crash stacks surface. Wall-time budget unchanged (controller respects `-max_total_time`); the extra cores buy more coverage per flake-check. Smoke bumped 10s→30s.

**CI aggregate targets** (`2081439`): `linkFarmFromDrvs` on a 2×2 matrix: `ci-{local-,}{fast,slow}` — fast vs slow fuzz (30s vs 10min), local vs with-VM. Building one forces Nix to build all ~18 constituents. Attribute normalization: drop redundant `rio-` prefix from checks (`clippy`, `nextest`, `doc`, `coverage`, `build`), add `vm-` prefix to phase tests, `-smoke` suffix to fuzz. Derivation `pname`s stay `rio-*` so `/nix/store` paths stay greppable.

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "nightly toolchain, fuzz-build derivations, fuzz-*-smoke checks, ci-* aggregate targets, devShells.stable"},
  {"path": "rio-nix/fuzz/Cargo.toml", "action": "MODIFY", "note": "[workspace] root, separate Cargo.lock"},
  {"path": "rio-nix/fuzz/fuzz_targets/opcode_parsing.rs", "action": "MODIFY", "note": "fuzz target wiring"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "FUZZER-FOUND DoS FIX: bound in-memory NAR content size 4 GiB→256 MiB"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "FUZZER-FOUND PANIC FIX: is_char_boundary guard before split_at(32)"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. The NAR bound would retroactively land under `r[nix.nar.limits]`; the StorePath guard under `r[nix.path.parse]`.

## Entry

- Depends on **P0004** (review1/fuzz): the fuzz targets existed but were unusable; this plan wires them.
- Depends on **P0067** (final hygiene): `nar.rs` and `store_path.rs` were modified in P0067 (`StorePath` full-string caching, `NarEntry` accessors removed).

## Exit

Merged as `b2321db..2081439` (6 commits). `.#ci` green at merge. Fuzz-found crashing inputs added to corpus as regression seeds. `nix build .#ci-fast` now the single-command validation gate.
