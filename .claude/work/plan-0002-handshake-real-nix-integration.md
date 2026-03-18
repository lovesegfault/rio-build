# Plan 0002: Handshake correctness — real-Nix integration

## Context

This is the "the spec was wrong" cluster. P0000 and P0001 were built against the design doc's description of the Nix worker protocol, which was itself derived from reading Nix source and third-party documentation. When the first `nix store info --store ssh-ng://localhost` was attempted against real Nix 2.31, five separate protocol-level assumptions turned out to be wrong. This plan is the grind from "handshake compiles" to "handshake works end-to-end against a real client."

Phase 1a's Week 2-3 milestone ("SSH handshake completes successfully") and Week 6-8 milestone ("`nix path-info` returns correct info") both landed here. The commits read as a sequence of discoveries: each one fixes a bug that blocked the next integration step, finds the next bug, repeat.

## Commits

- `9c61af4` — fix: correct wire format to match real Nix protocol (u64 magic, 1.38 features, exec_request)
- `eecdd96` — fix: complete handshake with postHandshake phase and fix SSH channel I/O deadlock
- `e17a238` — fix: close connection on unimplemented opcodes and harden protocol safety
- `0d6cadc` — test: add byte-level protocol tests for all Phase 1a opcodes and proptest roundtrips
- `c9866fb` — feat: validate Week 6-8 milestone with nix path-info integration test
- `e5f69fb` — docs: correct gateway.md handshake spec to match real Nix protocol

Six commits. `e5f69fb` is chronologically after P0003's `ec41b13` but semantically belongs here — it's the design-doc update for the discoveries in `9c61af4`/`eecdd96`.

## Files

```json files
[
  {"path": "rio-nix/src/protocol/handshake.rs", "action": "MODIFY", "note": "u64 magic (was u32), 1.38 feature exchange, postHandshake phase"},
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "1M collection count DoS limit, proptest roundtrips"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "211-char name limit"},
  {"path": "rio-nix/src/hash.rs", "action": "MODIFY", "note": "SHA-1 computation (was declared but not implemented)"},
  {"path": "rio-build/src/gateway/session.rs", "action": "MODIFY", "note": "close connection on unimplemented opcode, drop SetOptions-first enforcement"},
  {"path": "rio-build/src/gateway/server.rs", "action": "MODIFY", "note": "Handler::data() callback + Handle::data() bridge (replaces Channel-based I/O), exec_request for nix-daemon --stdio"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "wopQueryMissing (opcode 40), wopQueryPathFromHashPart (29), wopAddSignatures (37, stub)"},
  {"path": "rio-build/src/store/memory.rs", "action": "MODIFY", "note": "import_from_nix_store() helper for dev/test seeding"},
  {"path": "rio-build/tests/protocol_conformance.rs", "action": "NEW", "note": "integration tests: real nix CLI against rio-build SSH server"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "NEW", "note": "byte-level tests: DuplexStream, no SSH, raw wire bytes per opcode"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "correct handshake spec: u64 magic, 1.38 features, postHandshake, connection-close-on-unknown"}
]
```

## Design

Five protocol-level discoveries, in order:

**Magic bytes are u64, not u32.** The Nix worker magic values (`0x6E697863` for client, `0x6478696F` for server) are written as 8-byte little-endian, not 4-byte. The design doc in `ffbbd8a` said u32; the real daemon says u64. First bug found, easiest to fix.

**Protocol 1.38 has a mandatory feature-list exchange.** Between version negotiation and the rest of the handshake, 1.38+ clients send a `StringList` of feature names and expect the server to echo back its supported subset. 1.37 clients skip this. The handshake must branch on negotiated version.

**`ssh-ng://` sends an `exec` request for `nix-daemon --stdio`, not a shell request.** The russh `Handler` needs to implement `exec_request` and check the command string. This commit accepts any exec command; P0006 tightened the validation.

**`initConnection` has an undocumented postHandshake phase.** After the magic/version/features exchange, the client sends CPU affinity (u64) and reserveSpace (bool), then expects the server to send its version string, trusted status (u64: 0=unknown, 1=trusted, 2=not-trusted), and an initial `STDERR_LAST` — all before the first `wopSetOptions` arrives. Omitting any of this makes the client hang.

**`russh::Channel`-based I/O deadlocks in spawned tasks.** The `ChannelTx` flow control blocks on the send side when backpressure kicks in, and if the receive side is in the same task, that's a deadlock. Replaced with `Handler::data()` callback that pushes into an unbounded `mpsc`, plus `Handle::data()` for outbound. This decouples SSH transport from protocol processing entirely.

One protocol-policy discovery: unimplemented opcodes must close the connection. The initial implementation sent `STDERR_ERROR` and continued reading, but the opcode payload is unread (format unknown) so the stream is desynchronized. The only safe move is error-then-close.

The commit `c9866fb` added `import_from_nix_store()` — a helper that shells out to `nix path-info --json` and `nix-store --dump` to seed the `MemoryStore` with real data. This made the Week 6-8 milestone test possible: build a tiny path with `writeTextFile`, import it, query it back via `nix path-info --store ssh-ng://localhost`, assert the metadata matches. Also dropped the "SetOptions must be the first opcode" enforcement — turns out real `nix-daemon` accepts any opcode immediately after handshake.

`0d6cadc` added 63 tests: byte-level opcode tests via `tokio::io::DuplexStream` (construct raw wire bytes, feed through dispatcher, assert response bytes), and proptest roundtrips for u64/bytes/bool/nixbase32. This is the test layer that catches wire-encoding bugs that integration tests miss.

## Tracey

Predates tracey adoption. The spec corrections in `docs/src/components/gateway.md` (`e5f69fb`) were later marked with `r[gw.handshake.magic]`, `r[gw.handshake.features]`, `r[gw.handshake.posthandshake]`, `r[gw.opcode.unknown-close]` when tracey was adopted. Run `tracey query rule gw.handshake` for the current marker set.

## Outcome

63 tests green. `nix store info --store ssh-ng://localhost` passes. `nix path-info --json --store ssh-ng://localhost /nix/store/...` returns correct metadata. Week 2-3 and Week 6-8 milestones complete. `docs/src/components/gateway.md` now matches reality instead of the original guess.
