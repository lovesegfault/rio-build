# Recorded `nix derivation show -r` fixtures

`strncpy-subset.json` is a six-record subset of a `nix derivation show -r`
run recorded with nix 2.34 (JSON format `"version": 4`) during the
2026-05-26 design spike that preceded the rio-replay implementation. The
recorded run enumerated the dependency closure of a nixpkgs `nodejs`
build; the subset keeps the minimal-bootstrap `strncpy.drv` target plus
its five dependencies (four input-addressed derivations and one
fixed-output fetchurl derivation), and preserves the top-level
`"version"` wrapper key so the parser tests exercise the real wrapped
shape.

`structured-attrs.json` is the complete, byte-verbatim output of a
`nix derivation show -r` run (nix 2.34.7, JSON format `"version": 4`)
over a two-derivation `__structuredAttrs = true` closure: an
input-addressed target declaring `requiredSystemFeatures` and a
fixed-output dependency declaring `impureEnvVars`. It exists because
flat-env-only extraction passes every test whose corpus contains no
structuredAttrs derivation — in this format the user attrs live in the
top-level `structuredAttrs` object and the env carries only output
placeholder keys, so the per-derivation declaration extraction must read
the structured payload to see them.

## Extracted, not edited

Every record's values (store paths, `env`, `outputs`, …) are exactly as
nix emitted them; for `strncpy-subset.json` only the selection into a
subset and the JSON re-serialization (2-space indent, sorted keys)
differ from the raw recording, and `structured-attrs.json` is unedited
producer output. Do not hand-edit values — the unit tests in
`rio-replay/src/evalset/depclosure.rs` assert the literal store paths.

## Re-recording (only if ever needed)

No network or Hydra requests are involved; any local store works:

- Run `nix derivation show -r <some-small-drv>` with nix ≥ 2.32 (the
  wrapped `{"version": …, "derivations": {…}}` format).
- Keep a target plus a handful of its dependencies, including at least
  one fixed-output derivation so the env-fallback output resolution
  stays covered, and keep the top-level `"version"` key.
- For `structured-attrs.json`, instantiate two `__structuredAttrs = true`
  derivations (one fixed-output declaring `impureEnvVars`, one
  input-addressed declaring `requiredSystemFeatures` and depending on
  the first) and record the show output of the target unmodified.
- Update the literal store paths asserted in
  `rio-replay/src/evalset/depclosure.rs` to match, and note the
  re-recording in the commit message.
