# Plan 0018: Upload opcode wire format fix — framed stream, not STDERR_READ pull loop

## Context

This is the single most consequential fix in phase 1b. P0012 implemented `wopAddToStoreNar` using a STDERR_READ pull loop: server sends `STDERR_READ(n)`, client responds with up to `n` bytes, repeat until done. That is the protocol — for clients with protocol version ≤1.22 (Nix ≤2.3, 2019-era). Protocol ≥1.23 uses a framed byte stream: client sends the entire NAR as `u64(chunk_len) + bytes, ..., u64(0)`, server just reads it.

The bug was invisible in P0013's integration test because both ends of the connection were rio code — server pull loop matched client pull loop, wrong protocol round-tripped fine. It was also masked by P0015: the test derivation was small enough that uploads went through `wopAddToStore` (opcode 7, legacy CA path) instead of `wopAddToStoreNar` (opcode 39). Only when testing with larger closures did the real nix client send opcode 39, and then the server waited for a STDERR_READ response the client was never going to send.

## Commits

- `40d2264` — fix(rio-build): correct wopAddToStoreNar and wopAddMultipleToStore wire format

Single commit. Found mid-way through review pass 2, elevated here because the bug class is different: this isn't a bounds check or an error-path gap, it's a protocol mechanism mismatch that made two handlers completely incompatible with real clients.

## Files

```json files
[
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "AddToStoreNar: read_framed_stream instead of STDERR_READ loop; add dontCheckSigs read; drop spurious u64(1) result. AddMultipleToStore: drop spurious u64(1) result."},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "rewrite test_add_to_store_nar_direct, test_add_to_store_nar_hash_mismatch, test_add_to_store_nar_oversized, test_add_multiple_to_store_direct, test_query_derivation_output_map_direct — all were exercising the wrong protocol"}
]
```

## Design

**`wopAddToStoreNar` (39), three fixes against `libstore/daemon.cc`:**

Line 918–923: protocol ≥1.23 reads via `FramedSource`. Replace the `STDERR_READ(65536)` → `read_string` loop with `read_framed_stream`. The old loop was a valid implementation of a protocol version that no supported client speaks.

Line 912: after `repair` (bool), the real daemon reads a second bool `dontCheckSigs`. P0012's handler skipped it. Missing one `u64` read means every subsequent byte is interpreted 8 bytes shifted — the handler would try to parse part of the NAR framing as the next opcode.

After `stopWork()` (STDERR_LAST), the real daemon writes nothing. `remote-store.cc` line 453: `withFramedSink` then returns, no result read. P0012 wrote `u64(1)` as a success indicator. The client, having already moved on to the next opcode, would read that `u64(1)` as an opcode number (opcode 1 = `wopIsValidPath`) and then try to send a path where the server expected nothing.

**`wopAddMultipleToStore` (44):** Same spurious `u64(1)` after STDERR_LAST. `daemon.cc` lines 519–520: `stopWork()` then `break`, nothing written. Removed.

**Five byte-level tests rewritten.** They had been validating the STDERR_READ loop — constructing mock client responses to `STDERR_READ` requests. Now they construct framed stream data and assert no result value after STDERR_LAST. The tests passing before this commit was itself a test-design failure: they were testing that the code did what the code did, not what the protocol specified.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.opcode.wopAddToStoreNar.framed`, `gw.opcode.wopAddMultipleToStore.framed`.

## Outcome

Upload opcodes compatible with protocol ≥1.23 clients. `nix copy --to ssh-ng://localhost` works for arbitrary closures, not just small-enough-to-use-opcode-7 derivations. The daemon.cc line references in the commit body (918–923, 912, 519–520) were deliberate — the only reliable spec is the reference implementation. P0005's golden-conformance infra (live daemon comparison) catches this class of bug by construction; P0021 extended it to cover interactive-STDERR opcodes.
