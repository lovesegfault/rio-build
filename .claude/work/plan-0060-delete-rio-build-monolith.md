# Plan 0060: Port golden tests + delete rio-build monolith

## Design

**Crate identity change**: the phase-1 `rio-build/` gateway monolith (18 files, ~8000 LOC) is deleted. `rio-gateway/` (gRPC-based, used by all VM tests) is now the sole gateway implementation. From this point on, `rio-build` is a workspace name, not a crate name.

Before deletion, unique test coverage was audited and ported. The monolith had three test suites:

1. **Golden conformance** (`tests/golden_conformance.rs`, 18 tests): each test starts a real `nix-daemon`, issues the same wire bytes to both the daemon and our gateway, and compares responses byte-for-byte. Thirteen ported; the four structural-only tests (no live daemon, just wire shape) were already covered by `wire_opcodes.rs`. Porting required rewiring `seed_mock_store_from` to seed the gRPC `MockStore` instead of the monolith's local `dyn Store` trait object — architecturally different backends, same test assertions.

2. **Daemon lifecycle harness** (`tests/golden/daemon.rs`, 717 LOC): `LazyLock`-managed `nix-daemon` subprocess, protocol exchange helpers, fixture builders (`build_test_path`, `build_ca_test_path`, `query_path_info_json`, `dump_nar`), STDERR-aware byte readers. Ported verbatim. Also `golden/mod.rs` (693 LOC): field parsers, byte-level comparison, client-byte builders. One parser variant dropped (`STDERR_WRITE`-specific `NarFromPath`) — `rio-gateway` uses raw-NAR-after-LAST like the real daemon, not the monolith's STDERR_WRITE mode.

3. **Error-path tests** (`tests/direct_protocol.rs`): `test_version_too_old_sends_stderr_error` (handshake rejection) and `test_multi_opcode_sequence` (session stays open across opcodes) ported to `wire_opcodes.rs` (+119 LOC). Hash/size mismatch tests **not** ported — the monolith validated hashes itself; `rio-gateway` passes declared hashes through, and validation is `rio-store`'s responsibility (already covered by its own `validate.rs` + `grpc_integration.rs`). Architecturally different contract.

No unique workspace deps lost — `rio-gateway`/`rio-common` already pull everything `rio-build` used.

## Files

```json files
[
  {"path": "rio-build/Cargo.toml", "action": "DELETE", "note": "phase-1 monolith manifest"},
  {"path": "rio-build/src/lib.rs", "action": "DELETE", "note": "monolith root"},
  {"path": "rio-build/src/main.rs", "action": "DELETE", "note": "monolith binary"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "DELETE", "note": "superseded by rio-gateway/src/handler.rs"},
  {"path": "rio-build/src/gateway/mod.rs", "action": "DELETE", "note": "superseded by rio-gateway"},
  {"path": "rio-build/src/gateway/server.rs", "action": "DELETE", "note": "superseded by rio-gateway/src/server.rs"},
  {"path": "rio-build/src/gateway/session.rs", "action": "DELETE", "note": "superseded by rio-gateway/src/session.rs"},
  {"path": "rio-build/src/observability.rs", "action": "DELETE", "note": "superseded by rio-common"},
  {"path": "rio-build/src/store/memory.rs", "action": "DELETE", "note": "local dyn Store — architecturally replaced by gRPC MockStore"},
  {"path": "rio-build/src/store/mod.rs", "action": "DELETE", "note": "local store trait"},
  {"path": "rio-build/src/store/traits.rs", "action": "DELETE", "note": "local store trait"},
  {"path": "rio-build/src/store/validate.rs", "action": "DELETE", "note": "hash validation — now rio-store's job"},
  {"path": "rio-build/tests/golden_conformance.rs", "action": "DELETE", "note": "ported (13/18) to rio-gateway"},
  {"path": "rio-build/tests/golden/daemon.rs", "action": "DELETE", "note": "ported verbatim"},
  {"path": "rio-build/tests/golden/mod.rs", "action": "DELETE", "note": "ported (minus STDERR_WRITE NarFromPath variant)"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "DELETE", "note": "2 tests ported to wire_opcodes.rs; hash-mismatch tests NOT ported (architectural difference)"},
  {"path": "rio-build/tests/protocol_conformance.rs", "action": "DELETE", "note": "structural-only; covered by wire_opcodes.rs"},
  {"path": "rio-build/tests/integration_build.rs", "action": "DELETE", "note": "superseded by VM tests"},
  {"path": "rio-gateway/tests/golden_conformance.rs", "action": "NEW", "note": "13 live-daemon tests, rewired for gRPC MockStore"},
  {"path": "rio-gateway/tests/golden/daemon.rs", "action": "NEW", "note": "717 LOC daemon lifecycle harness (verbatim port)"},
  {"path": "rio-gateway/tests/golden/mod.rs", "action": "NEW", "note": "693 LOC field parsers + byte comparison"},
  {"path": "rio-gateway/tests/wire_opcodes.rs", "action": "MODIFY", "note": "+119 LOC: version-too-old, multi-opcode-sequence"},
  {"path": "rio-gateway/Cargo.toml", "action": "MODIFY", "note": "golden test deps"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "remove rio-build workspace member"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. The ported golden tests would retroactively carry `r[verify gw.opcode.*]` markers.

## Entry

- Depends on **P0005** (golden-conformance): the 18 golden tests being ported were created in phase-1a.
- Depends on **P0059** (test-support + proto helpers): `MockStore`/`MockScheduler` gRPC mocks and `connect_*` helpers are used by the ported tests.

## Exit

Merged as `5a2a020..04ab1d5` (2 commits). `.#ci` green at merge; 13 golden tests pass against real `nix-daemon` in `rio-gateway/tests/`.
