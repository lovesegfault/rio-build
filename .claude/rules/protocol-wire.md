---
paths:
  - "rio-gateway/src/**"
  - "rio-nix/src/protocol/**"
  - "rio-nix/src/wire/**"
---

rio-build implements the Nix worker protocol. Protocol work has specific pitfalls:

### Validate against the real implementation early

Don't trust specs or design docs alone. Run integration tests against a real `nix` client (e.g., `nix store info --store ssh-ng://localhost`) as soon as the handshake compiles. Protocol bugs found through integration are cheaper than protocol bugs found in review.

### Wire encoding conventions

The Nix worker protocol sends `narHash` fields as **hex-encoded SHA-256 digests** — no algorithm prefix, no nixbase32. Use `hex::decode` + `NixHash::new`, not `NixHash::parse_colon`. The `sha256:nixbase32` format appears in narinfo text and colon-format hashes, not on the wire. When in doubt, check the real daemon with a golden conformance test before assuming an encoding.

### Wire-level testing

Every opcode and wire primitive needs a byte-level test that constructs raw bytes, feeds them through the parser, and asserts the result. High-level integration tests are not sufficient — they hide framing and encoding bugs.

Include:
- Proptest roundtrips for all wire primitives (u64, bytes, bool, strings, collections)
- Live-daemon golden conformance tests: each test starts an isolated nix-daemon and compares responses at the byte level (see `tests/golden/`)
- Fuzz targets for wire parsing (`cargo-fuzz` in `rio-nix/fuzz/`)

**Before marking an opcode as complete, verify:**
- [ ] `tracey query rule gw.opcode.<name>` shows both `impl` (in `opcodes_*.rs`) and `verify` (in `wire_opcodes/` + `golden/`)
- [ ] Byte-level test in `wire_opcodes/` (constructs raw wire bytes, no SSH)
- [ ] At least one error-path test (e.g., missing path, hash mismatch) verifying STDERR_ERROR is sent
- [ ] Proptest roundtrip for any new wire types introduced by the opcode
- [ ] Fuzz target for any new parser (ATerm, NAR, DerivedPath, etc.)
- [ ] Integration test asserts success (not just "exercises the protocol path")

### Safety bounds

Protocol parsers must enforce:
- Maximum collection sizes (prevent DoS via unbounded allocation)
- Maximum string/path lengths (Nix store paths are max 211 chars)
- Graceful handling of unknown opcodes (close the connection after STDERR_ERROR; don't try to skip unknown payloads)

**Every count-prefixed loop must enforce `MAX_COLLECTION_COUNT`** before entering the loop — not just in `wire::read_strings`/`read_string_pairs`, but in any custom reader (e.g., `read_basic_derivation` output loop, `read_build_result` built-outputs loop, STDERR trace/field readers). Use the same pattern: `if count > wire::MAX_COLLECTION_COUNT { return Err(WireError::CollectionTooLarge(count)); }`.

### Error propagation

Every handler error path that returns `Err(...)` must send `STDERR_ERROR` to the client **first**. Never use bare `?` to propagate errors from store operations, NAR extraction, or ATerm parsing — always wrap in a match that sends `STDERR_ERROR` before returning. For batch opcodes like `wopBuildPathsWithResults`, per-entry errors should push `BuildResult::failure` and `continue`, not abort the entire batch.

### Stub opcodes

When adding a recognized opcode as a stub/no-op, **read the expected wire payload** and return `STDERR_LAST` + a neutral result (e.g., `u64(1)` for success, `u64(0)` for empty set). Never fall through to the "unimplemented" catch-all — that closes the connection, which breaks modern Nix clients that may send the opcode opportunistically (e.g., CA-related opcodes after a build).

### Local daemon subprocesses

When spawning `nix-daemon --stdio` for local build execution:
- Never `.unwrap()` on `daemon.stdin.take()` / `daemon.stdout.take()` — use `.ok_or_else()`
- Wrap all daemon communication in `tokio::time::timeout` (default: `DEFAULT_DAEMON_TIMEOUT` = 2h, configurable via `RIO_DAEMON_TIMEOUT_SECS` / `--daemon-timeout-secs` / `worker.toml`)
- Always `daemon.kill().await` in both success and error paths
- See `spawn_daemon_in_namespace` + `run_daemon_build` in `rio-worker/src/executor/daemon.rs` for the canonical pattern
