# Plan 0015: Legacy opcodes via real-nix integration — AddToStore (7), AddTextToStore (8), EnsurePath (10)

## Context

P0014 made the integration test assert success. It failed. The first failure mode: unknown opcode 7. The gateway's opcode table had a gap — opcodes 7–10 weren't mapped. A quick glance at the Nix source suggested 7 was `EnsurePath` (a validity check), so `ee2d25b` added a stub that read a path and returned void. The test still failed, now with stream desynchronization: the client was sending a lot more bytes after opcode 7 than a single store path.

The real opcode map: `AddToStore` = 7, `AddTextToStore` = 8, `EnsurePath` = 10. Opcode 7 isn't a validity check; it's the legacy content-addressed upload path. The client was sending a framed NAR stream, and the stub was reading one string and stopping — the rest of the stream was being interpreted as the next opcode. This is the "don't trust the design doc, validate against the real implementation" lesson in purest form.

## Commits

- `ee2d25b` — feat(rio-build): add wopEnsurePath (7) stub handler
- `53e6dfb` — feat(rio-build): implement wopAddToStore (7) and fix EnsurePath opcode number
- `824c8b2` — fix(rio-nix): correct text store path computation to match Nix algorithm

Three commits. The first has the wrong opcode number; the second corrects it; the third fixes a downstream hash-computation bug the second introduced.

## Files

```json files
[
  {"path": "rio-nix/src/protocol/opcodes.rs", "action": "MODIFY", "note": "AddToStore=7, AddTextToStore=8, EnsurePath=10 (was EnsurePath=7)"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "StorePath::make_fixed_output, make_text — CA store path computation"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "handle_add_to_store (7, framed CA import), handle_add_text_to_store (8), handle_ensure_path (10, u64(1) result)"},
  {"path": "rio-build/tests/integration_build.rs", "action": "MODIFY", "note": "re-enable assertion (opcode path unblocked)"}
]
```

## Design

**Opcode 7: `wopAddToStore`.** Legacy content-addressed import. Client sends name, CA method (`text:sha256` or `fixed:r:sha256`), references, then a framed NAR stream. Server computes the store path from the content hash (this is what "content-addressed" means — the client doesn't know the store path until the server tells it) and stores the path. For method `text`, the computation is `StorePath::make_text(name, content_hash, refs)`; for `fixed`, it's `StorePath::make_fixed_output(name, hash_algo, content_hash)`.

**`StorePath::make_text` algorithm.** The fingerprint string is `text{:ref1:ref2:...}:sha256:<hex>:/nix/store:<name>`, SHA-256'd then truncated to 20 bytes and nixbase32-encoded. The first version (`53e6dfb`) used a two-level hash (inner content hash + outer `output:out:` wrapper), which is what input-addressed paths use. Text paths use a single-level hash — the fingerprint goes straight to the final SHA-256, no intermediate step. `824c8b2` fixed this after the integration test produced a different `.drv` store path than the nix client expected, tripping `Assertion path2 == path failed` inside nix.

**Opcode 8: `wopAddTextToStore`.** Even more legacy. Client sends name, content (plain string, not NAR-wrapped), references. Server computes a text-CA store path and stores. Used by `builtins.toFile`.

**Opcode 10: `wopEnsurePath`.** The actual validity check. Reads a store path, returns `STDERR_LAST` + `u64(1)` (success). The stub at `ee2d25b` didn't include the `u64(1)` result — `53e6dfb` added it.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.opcode.wopAddToStore`, `gw.opcode.wopAddTextToStore`, `gw.opcode.wopEnsurePath`, `nix.storepath.ca`.

## Outcome

Opcode map corrected. CA store-path computation matches Nix. The integration test got past the upload phase — then failed in `BuildResult` decoding (P0016's territory). One opcode-number mistake cascaded into three commits and introduced a content-addressed-path code path that phase 1b didn't originally plan for. The CLAUDE.md rule "Validate against the real implementation early" was written in direct response to this.
