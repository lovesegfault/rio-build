# Plan 0054: Wire-format conformance — wopAddMultipleToStore + wopNarFromPath (VM-test caught)

## Context

Two protocol-level wire-format bugs. Both had **passing byte-level tests** (P0045). The tests were written to match the buggy handler, not the Nix spec. Only running a real Nix client against the gateway exposed them.

**`wopAddMultipleToStore` (opcode 44):** The handler was missing the leading `num_paths: u64` count inside the framed stream, and reading NAR data as a nested framed stream when it's actually `narSize` plain bytes. Both bugs together caused a one-field desync where `read_string` for the `deriver` read path-string bytes as a length — **the error `string length 8031170683229269551` is ASCII `/nix/sto` as little-endian u64.**

Caught by the VM milestone test running real `nix copy --to ssh-ng://`. The existing byte-level test was constructed to match the buggy parser, not the Nix spec (`Store::addMultipleToStore(Source &)` in `store-api.cc`) — a reminder that golden conformance tests against a real daemon catch what byte-level tests written from our own format assumptions do not.

**`wopNarFromPath` (opcode 38):** Nix client's `narFromPath` does `processStderr(ex)` WITHOUT a sink, then `copyNAR(conn->from, sink)` reads raw bytes. Our handler was sending `STDERR_WRITE` chunks (like `wopExportPaths` does), but `narFromPath`'s `processStderr` loop has no sink → `error: no sink`. The fix: buffer the full NAR first (so we can still send `STDERR_ERROR` on mid-stream gRPC failure), then `STDERR_LAST`, then raw NAR bytes.

This is the exact same lesson as P0018 (phase 1b: "framed NOT STDERR_READ") — the wire format is what Nix actually sends/expects, not what looks symmetrical.

## Commits

- `b1d3447` — fix(rio-gateway): correct wopAddMultipleToStore wire format (num_paths prefix + plain NAR bytes)
- `132de90` — fix(rio-gateway): wopNarFromPath must send raw NAR after STDERR_LAST not STDERR_WRITE

## Files

```json files
[
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "handle_add_multiple_to_store: read num_paths u64 prefix, read narSize plain bytes (not nested framed); handle_nar_from_path: buffer NAR fully, then STDERR_LAST, then raw bytes (not STDERR_WRITE chunks)"},
  {"path": "rio-gateway/tests/wire_opcodes.rs", "action": "MODIFY", "note": "test_add_multiple_to_store_batch rewritten to match REAL wire format (num_paths prefix, plain NAR); test_nar_from_path: read raw bytes after LAST, not STDERR_WRITE frames"},
  {"path": "nix/modules/worker.nix", "action": "MODIFY", "note": "add pkgs.fuse3 to PATH (fusermount3 needed by fuser AutoUnmount); remove too-narrow CapabilityBoundingSet (nix-daemon sandbox needs CAP_SETUID/CHOWN/SYS_CHROOT/MKNOD)"}
]
```

## Design

**`wopAddMultipleToStore` correct format:**
```
[framed stream start]
  num_paths: u64
  repeat num_paths times:
    ValidPathInfo (deriver, narHash, references, ...)
    narSize plain bytes of NAR (NOT nested framed)
[framed stream end]
```

The old handler started reading `ValidPathInfo` immediately (no `num_paths`) and wrapped each NAR in a nested `FramedStreamReader`. The one-field desync: handler reads first field expecting a `ValidPathInfo`'s `deriver` string, but the actual bytes are `num_paths: u64` followed by store-path bytes. `read_string`'s length prefix reads those store-path bytes → `8031170683229269551` = `0x6F74732F78696E2F` = `/nix/sto` little-endian.

**`wopNarFromPath` correct format:**
```
STDERR_LAST
raw NAR bytes (no framing, no length prefix — client reads via copyNAR which parses NAR magic)
```

The old handler sent `STDERR_WRITE{len, chunk}` frames (symmetric with `wopExportPaths`, but wrong — `narFromPath`'s client loop has no sink). `TODO(phase2b)` for incremental streaming via length-prefixed NAR framing.

**VM test first pass:** `132de90` is the commit where "VM milestone test now PASSES: all 5 builds succeed across 2 workers, all metric assertions pass." The test caught this because `nix-build`'s remote store queries the output NAR after build completion even with `--no-out-link`.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[gw.opcode.add-multiple.wire-format]`, `r[gw.opcode.nar-from-path.raw-after-last]`. Both marked with cross-refs to P0018's precedent.

## Outcome

Merged as `b1d3447` + `132de90` (2 commits). The byte-level tests were *rewritten* to match the real format — they still pass, but now for the right reason. This is why `.#ci` includes VM tests as a gate, not just byte-level tests.
