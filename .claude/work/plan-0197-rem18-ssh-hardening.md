# Plan 0197: Rem-18 — SSH hardening: session-error visibility, channel limit, keepalive, nodelay, temp_roots delete

## Design

**P1 (HIGH).** russh defaults leave the SSH layer undiagnosable and unbounded.

**`handle_session_error`:** russh default is a no-op. Any `?`-propagated error from a Handler method dropped the connection with zero server-side signal. Now logs at `error!` + increments `rio_gateway_errors_total{type=session}`.

**Channel limit:** `MAX_CHANNELS_PER_CONNECTION=4` (matches Nix `max-jobs`). Gated on `sessions.len()` — only exec'd channels consume resources (2 tasks + 2×256KiB each). 5th open → `SSH_MSG_CHANNEL_OPEN_FAILURE`.

**russh Config hardening** (extracted as `build_ssh_config`): `keepalive_interval=30s` (×3 unanswered = ~90s half-open drop); `nodelay=true` (Nagle + small req/resp ping-pong = ~40ms/RTT); `methods: publickey only` (default advertises all → wasted RTTs); `auth_rejection_time_initial=10ms` (OpenSSH `none` probe fast-path).

**`auth_none`:** call `mark_real_connection` before reject. OpenSSH sends `none` first; without this override, probe-and-disconnect clients didn't trigger connection metrics.

**`temp_roots` deleted:** insert-only HashSet, never read. `wopAddTempRoot` is now read-validate-ack. Temp roots are a local-daemon GC concept; rio's GC is store-side with explicit pins. (This item was in rem-21's §Remainder "hold for plan 07" — it actually landed here, not in rem-07's fix. See P0200 notes.)

Remediation doc: `docs/src/remediations/phase4a/18-ssh-hardening.md` (705 lines).

## Files

```json files
[
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "handle_session_error logs + metric; build_ssh_config: keepalive/nodelay/methods/auth_rejection_time; auth_none marks real connection"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "MAX_CHANNELS_PER_CONNECTION=4 gate on sessions.len()"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "SessionContext.temp_roots DELETED"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "handle_add_temp_root: read-validate-ack only"},
  {"path": "rio-gateway/tests/ssh_hardening.rs", "action": "NEW", "note": "channel limit, keepalive, nodelay, session-error visibility tests"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "5 spec markers"}
]
```

## Tracey

- `r[impl gw.conn.session-error-visible]` — `9ef0fdc`
- `r[impl gw.conn.channel-limit]` — `9ef0fdc`
- `r[impl gw.conn.keepalive]` — `9ef0fdc`
- `r[impl gw.conn.nodelay]` — `9ef0fdc`
- `r[impl gw.conn.real-connection-marker]` — `9ef0fdc`
- `r[verify gw.conn.keepalive]` — `9ef0fdc`
- `r[verify gw.conn.nodelay]` — `9ef0fdc`
- `r[verify gw.conn.session-error-visible]` — `9ef0fdc`
- `r[verify gw.conn.channel-limit]` — `9ef0fdc`
- `r[verify gw.conn.real-connection-marker]` — `9ef0fdc`
- `r[verify gw.conn.lifecycle]` — `9ef0fdc`

11 marker annotations (5 impl, 6 verify — second-largest tracey footprint).

## Entry

- Depends on P0148: phase 3b complete (extends phase 2b SSH server)

## Exit

Merged as `4b0c79c` (plan doc) + `9ef0fdc` (fix). `.#ci` green. Half-open connections drop at ~90s; 5th channel rejected.
