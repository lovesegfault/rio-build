# Plan 0001: SSH gateway + opcode handlers + in-memory store

## Context

P0000 gave us a vocabulary; this plan gave us something that listens on a socket. The phase 1a doc describes this as three separate tasks — "SSH server (`russh`)", "Read-only opcodes", "`rio-store`: in-memory backend" — but they shipped as one unit because the SSH server is useless without handlers, handlers are useless without a store, and the store is useless without something calling it. This is the first commit where `cargo run` does something observable: binds port 2222, accepts an SSH connection, negotiates a channel, and dispatches opcodes.

The "Basic metrics counters" and "Structured logging" tasks from the phase doc also landed here — but in a scaffolded-not-wired state. The Prometheus exporter was initialized and bound to `/metrics`, but zero metrics were registered. Tracing was initialized with a JSON subscriber, but no spans were attached to the protocol handlers. P0003 fixed the metrics gap; P0004 fixed the span gap.

## Commits

- `e991e5b` — feat: add SSH gateway, protocol handlers, in-memory store, and observability

Single commit. 1,064 insertions. Nine new files in `rio-build/src/`.

## Files

```json files
[
  {"path": "rio-build/src/gateway/server.rs", "action": "NEW", "note": "russh::server::Handler impl — auth, channel open, channel close"},
  {"path": "rio-build/src/gateway/session.rs", "action": "NEW", "note": "per-channel protocol state machine: handshake then opcode dispatch loop"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "NEW", "note": "opcode handlers: IsValidPath, QueryPathInfo, QueryValidPaths, SetOptions, AddTempRoot, NarFromPath"},
  {"path": "rio-build/src/gateway/mod.rs", "action": "NEW", "note": "module root + re-exports"},
  {"path": "rio-build/src/store/memory.rs", "action": "NEW", "note": "MemoryStore: HashMap<StorePath, PathInfo> + HashMap<StorePath, Vec<u8>> (NAR bytes)"},
  {"path": "rio-build/src/store/traits.rs", "action": "NEW", "note": "Store trait: query_path_info, is_valid_path, nar_from_path, add_temp_root"},
  {"path": "rio-build/src/store/mod.rs", "action": "NEW", "note": "module root"},
  {"path": "rio-build/src/observability.rs", "action": "NEW", "note": "tracing + Prometheus exporter init (exporter empty — no metrics registered)"},
  {"path": "rio-build/src/main.rs", "action": "MODIFY", "note": "wire up: init observability, create MemoryStore, start SSH server"},
  {"path": "rio-build/src/lib.rs", "action": "MODIFY", "note": "expose gateway and store modules"}
]
```

## Design

The SSH layer uses `russh` in server mode. Each incoming channel gets its own `Session` — the protocol is per-channel, so two `nix` commands over the same SSH connection get independent handshake state. Public key auth only; the `auth_password` method was initially unimplemented (explicit rejection came in P0004). Channel close triggers a `Drop` that aborts the protocol task, so a client disconnecting mid-opcode doesn't leave a dangling future.

The session state machine is: read handshake → loop { read opcode → dispatch to handler → handler writes STDERR_* frames → handler writes result }. This commit's version had a subtle I/O bug: it used `russh::Channel` directly for reads, which deadlocks under `ChannelTx` flow control when the protocol task is spawned. P0002 replaced this with a `Handler::data()` callback that pushes bytes into an `mpsc` channel, decoupling SSH flow control from protocol processing.

Six opcode handlers landed:
- `wopIsValidPath` (1) — bool: is this path in the store?
- `wopQueryPathInfo` (26) — return `PathInfo` (nar hash, nar size, references, deriver, sigs)
- `wopQueryValidPaths` (31) — batch `IsValidPath`
- `wopSetOptions` (19) — client preferences (keep-failed, verbosity, etc.); read and ack
- `wopAddTempRoot` (11) — mark a path as GC-protected for this session
- `wopNarFromPath` (38) — stream the NAR bytes for a path

The `MemoryStore` keys on `StorePath` (not `String`) to avoid an allocation per lookup — `StorePath` is `Hash + Eq` so it works directly in `HashMap`. Two separate maps (`path_info` and `nar_bytes`) under separate locks; P0006 consolidated these to a single `RwLock` for insert atomicity.

`observability.rs` sets up `tracing_subscriber` with a JSON formatter and binds the Prometheus exporter. The exporter was serving an empty `/metrics` page — the registration calls were omitted. This was the first "scaffolded but not wired" observability gap that P0003 addressed.

## Tracey

Predates tracey adoption. No `r[impl ...]` annotations present at the `phase-1a` tag. Retro-tagged scope: `tracey query rule gw.opcode.wopIsValidPath`, `gw.opcode.wopQueryPathInfo`, `gw.opcode.wopSetOptions`, `gw.ssh.channel`, `store.memory`.

## Outcome

`cargo run` binds port 2222 and accepts SSH connections. The protocol handshake is reachable but broken (wrong magic byte width) — `nix store info --store ssh-ng://localhost` fails. P0002 fixes that. The six opcode handlers are wired but untestable end-to-end until the handshake works. Unit tests for `MemoryStore` operations pass.
