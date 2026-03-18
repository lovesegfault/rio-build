# Plan 0003: Prometheus metrics + tracing spans

## Context

P0001 initialized the Prometheus exporter but registered zero metrics — `/metrics` returned an empty page. The observability infrastructure was scaffolded, not functional. This plan is the "actually wire it up" fix: register the seven gateway metrics that the phase 1a doc calls out under "Basic metrics counters", and attach tracing spans to each channel session so log lines correlate.

Small plan — one commit, three files — but it's the phase 1a "Structured logging + metrics" task item going from scaffolded to functional. Chronologically it landed between P0002's protocol fixes and P0004's design-review remediation, which immediately renamed every metric to match the `rio_gateway_` prefix convention.

## Commits

- `ec41b13` — feat: add Prometheus metrics and tracing span instrumentation

Single commit. 71 insertions, 13 deletions, 3 files.

## Files

```json files
[
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "opcode counters + duration histograms per opcode name"},
  {"path": "rio-build/src/gateway/server.rs", "action": "MODIFY", "note": "connections_total counter, connections_active gauge (inc on open, dec on close)"},
  {"path": "rio-build/src/gateway/session.rs", "action": "MODIFY", "note": "per-channel tracing span via Instrument trait, handshake counter"}
]
```

## Design

Seven metrics registered, all prefixed with the component name (the prefix was wrong here — `gateway_` instead of `rio_gateway_` — P0004 fixed that):

- `connections_total` (counter) — SSH connections accepted, lifetime
- `connections_active` (gauge) — currently open connections; incremented in `auth_publickey`, decremented in a `Drop` guard
- `handshakes_total` (counter, label: `result`=success/failure) — protocol handshakes completed
- `channels_active` (gauge) — currently open SSH channels
- `opcodes_total` (counter, label: `opcode`) — opcode dispatches by name
- `opcode_duration_seconds` (histogram, label: `opcode`) — wall time per handler
- `errors_total` (counter, label: `kind`) — `STDERR_ERROR` responses sent

The gauge decrement happens via `Drop` so it's correct even when the protocol task panics or the connection is forcibly closed. This is the pattern that `observability.md` calls out — gauges that only increment are worse than no gauge at all.

Tracing spans use the `Instrument` trait: each channel's protocol task is wrapped in a span with `channel_id` and `peer_addr` fields, so every `tracing::info!` call inside an opcode handler inherits that context. The root span with `component="gateway"` (per the observability spec) was missing — P0004 added it.

## Tracey

Predates tracey adoption. Retro-tagged scope: `tracey query rule obs.metrics.gateway`, `obs.tracing.span`.

## Outcome

`/metrics` now returns seven populated metrics. `curl localhost:9090/metrics | grep gateway` shows nonzero values after a `nix store info` round-trip. Tracing logs carry `channel_id` and `peer_addr` on every handler line. The metric naming was immediately revised in P0004 to match `docs/src/observability.md`.
