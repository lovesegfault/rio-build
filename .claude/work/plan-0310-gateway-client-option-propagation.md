# Plan 0310: Gateway client-option propagation — close the ssh-ng maxSilentTime gap

[P0215](plan-0215-max-silent-time.md) empirically proved the ssh-ng client **never sends `wopSetOptions`** (protocol 1.38). This means ALL client-side build options (`--max-silent-time`, `--build-timeout`, `--keep-failed`, etc.) are silently non-functional over ssh-ng. P0215's worker-side config ([`config.rs:102`](../../rio-worker/src/config.rs) `max_silent_time_secs`, p215 worktree) is the operator's **fleet-wide default** stopgap with a `TODO(P0215)` self-referential tag at `:101`.

The real fix lives in the gateway: intercept client options from the ssh-ng handshake's **`extraValues` / `overrides` KV map** (the `wopSetOptions` opcode itself isn't sent, but the ssh-ng multiplexed-channel protocol DOES carry client settings in a different frame — OR the gateway can parse them from the `nix-daemon --stdio` exec-request argv). The spec at [`gateway.md:62`](../../docs/src/components/gateway.md) `r[gw.opcode.set-options.propagation]` already describes the intended behavior: "The gateway extracts relevant overrides and propagates them through the build pipeline: gateway → scheduler (via gRPC) → workers."

**Prerequisite investigation:** Before writing code, confirm WHERE the client options actually appear on the wire in ssh-ng mode. Three candidates:
1. The `exec_request` command string (does `nix build --max-silent-time 300` pass `--max-silent-time` through to the remote `nix-daemon --stdio` invocation?)
2. A different opcode (not `wopSetOptions` but something ssh-ng-specific)
3. They don't appear at all — ssh-ng simply drops them client-side, and this plan becomes "document the limitation in `gateway.md` and close the TODO with a WONTFIX note"

The P0215 reviewer says "ClientOptions accessor + 5 tests + selection logic all wired-ready" — verify this means P0215 left stubs for gateway-side reading that this plan fills in.

## Entry criteria

- [P0215](plan-0215-max-silent-time.md) merged (`max_silent_time_secs` worker config + `TODO(P0215)` anchor at `config.rs:101`)

## Tasks

### T0 — `test(gateway):` wire capture — where do ssh-ng client options appear?

**Investigation task. Do this FIRST.** In a VM test or via `nix/tests/scenarios/protocol.nix`, capture the full byte stream from a client invoking:
```bash
nix build --store ssh-ng://gateway --max-silent-time 300 .#hello
```

Dump every frame the gateway receives before the first `wopBuildDerivation`. Specifically look for:
- The `exec_request` command + argv — does `--max-silent-time 300` appear?
- Any opcode carrying a KV map with `max-silent-time` → `300`
- The `overrides` field if `wopSetOptions` IS sent in some ssh-ng flow (P0215 found it absent in the tested flow; maybe there's a variant)

Write the finding into this doc as a `> **T0-OUTCOME:**` blockquote. Scope of T1/T2 depends on it.

**Pre-T0 cleanup — stale docstring that will misdirect you:** [`rio-gateway/src/handler/mod.rs:82-93`](../../rio-gateway/src/handler/mod.rs) `max_silent_time()` docstring claims empirically-verified behavior that was **disproven within P0215 itself**. The comment (commit [`2c6e385e`](https://github.com/search?q=2c6e385e&type=commits), 09:12): "Empirically: `nix-build --option max-silent-time 5 --store ssh-ng://` sends positional=0, overrides=[(max-silent-time, 5)]." Ten minutes later ([`a7f17447`](https://github.com/search?q=a7f17447&type=commits), 09:22): gateway info-level `wopSetOptions` log **never fires** during an ssh-ng session. The docstring describes an intermediate understanding that the same implementer refuted. **Fix the docstring before T0** so you're not chasing a ghost — it currently promises overrides will be populated.

### T1 — `feat(gateway):` extract client options (scope per T0)

**IF T0 finds options in exec-request argv:**

MODIFY [`rio-gateway/src/server.rs`](../../rio-gateway/src/server.rs) — near `r[gw.conn.exec-request]` at [`gateway.md:518`](../../docs/src/components/gateway.md). The `exec_request` handler receives the full command string. Parse `--max-silent-time`, `--timeout`, `--keep-failed` from it:

```rust
// r[impl gw.opcode.set-options.propagation]
// ssh-ng client does NOT send wopSetOptions (P0215). Client options
// arrive via exec_request argv instead: `nix-daemon --stdio` is the
// base; additional `--option K V` or `--max-silent-time N` flags are
// appended by the client wrapper. Parse them here.
fn parse_client_options(exec_cmd: &str) -> ClientOptions {
    // ... scan for known flags
}
```

**IF T0 finds options in a different opcode:**

Handle that opcode in [`rio-gateway/src/handler/mod.rs`](../../rio-gateway/src/handler/mod.rs) or wherever opcode dispatch lives. Follow the `wopSetOptions` handling pattern (which exists but is never exercised for ssh-ng).

**IF T0 finds options are absent:**

This plan becomes documentation-only. Skip to T3.

### T2 — `feat(gateway):` propagate via BuildOptions proto

MODIFY [`rio-gateway/src/handler/build.rs`](../../rio-gateway/src/handler/build.rs) — where `SubmitBuildRequest.build_options` is populated (proto [`types.proto:300`](../../rio-proto/proto/types.proto) `BuildOptions build_options = 5`). The `BuildOptions` message already has `max_silent_time` at [`types.proto:314`](../../rio-proto/proto/types.proto). Populate it from T1's parsed `ClientOptions`:

```rust
build_options: Some(BuildOptions {
    max_silent_time: session_ctx.client_options.max_silent_time.unwrap_or(0),
    // ... other fields
}),
```

The worker already reads `BuildOptions.max_silent_time` (P0215 wired that side). This closes the loop.

### T3 — `fix(worker):` retag TODO(P0215) → TODO(P0310) or delete

MODIFY [`rio-worker/src/config.rs`](../../rio-worker/src/config.rs) at `:101` (post-P0215-merge line; verify at dispatch):

**IF T1/T2 implemented propagation:** Delete the TODO and update the comment to reflect reality:
```rust
/// Why this exists: ssh-ng client options propagate via exec_request
/// argv → gateway ClientOptions parser → BuildOptions proto (see
/// P0310). This config is the fallback when BuildOptions.max_silent_time
/// is 0/unset (client didn't specify).
```

**Also under T3 scope — dead-code decision:** [`rio-gateway/src/translate.rs:534-542`](../../rio-gateway/src/translate.rs) `Some(opts) =>` match arm in `build_submit_request()` reads `opts.max_silent_time()`, `opts.build_timeout()`, `opts.build_cores`, `opts.keep_going`. Five unit tests at [`handler/mod.rs:429-461`](../../rio-gateway/src/handler/mod.rs) (`max_silent_time_reads_from_overrides`, `max_silent_time_override_wins_over_positional`, etc.) exercise the `ClientOptions` accessors. If T0 confirms `wopSetOptions` is never sent over ssh-ng, this entire code path is **dead** — `options` is always `None` at the `translate.rs:534` callsite.

Decide: (a) **keep as future-proofing** — `ssh://` (legacy daemon protocol) DOES send `wopSetOptions`, so the code is live for non-ssh-ng clients; add a comment explaining which transport exercises it; OR (b) **delete** if the gateway only serves ssh-ng (check whether legacy `ssh://` is in scope). If (a), retitle the tests to mention `ssh://` so it's clear they test a legacy-only path.

**IF T0 found options are absent:** Retag to close the loop with a WONTFIX note:
```rust
/// WONTFIX(P0310): ssh-ng client options are dropped client-side
/// (Nix libstore ssh-ng code doesn't forward them — verified T0). This
/// config is the only mechanism. Clients wanting per-build maxSilentTime
/// must use `--store ssh://` (legacy) or gateway-side per-tenant config.
```

### T4 — `docs:` update r[gw.opcode.set-options.propagation]

MODIFY [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) at `:62` — the marker currently says "The gateway extracts relevant overrides" in present tense, implying it works via `wopSetOptions`. Update to reflect T0's finding:

**IF propagation implemented:** "The ssh-ng client does not send `wopSetOptions`; client options arrive via <T0 mechanism>. The gateway parses them and propagates..."

**IF WONTFIX:** "The ssh-ng client does not send `wopSetOptions` and does not forward client-side `--option` flags via any other mechanism (verified P0310 T0). Per-build client options are NOT supported over ssh-ng. Operators configure fleet-wide defaults via `worker.toml`."

Run `tracey bump` after editing — this is a meaningful spec change.

### T5 — `test(gateway):` propagation e2e (IF T1/T2 implemented)

```rust
// r[verify gw.opcode.set-options.propagation]
#[tokio::test]
async fn ssh_ng_max_silent_time_reaches_build_options() {
    // Mock exec_request with argv containing --max-silent-time 300.
    // Assert gateway's outbound SubmitBuildRequest.build_options
    //   .max_silent_time == 300.
}
```

## Exit criteria

- `/nbr .#ci` green
- T0-OUTCOME blockquote present in this doc with the wire-capture finding
- `grep 'TODO(P0215)' rio-worker/src/config.rs` → 0 hits (retagged or deleted)
- `grep 'sends positional=0, overrides=\[' rio-gateway/src/handler/mod.rs` → 0 hits (T0: stale empirical claim removed from docstring :82-93)
- `nix develop -c tracey query rule gw.opcode.set-options.propagation` — shows impl+verify (IF T1/T2) OR shows spec text with ssh-ng caveat (IF WONTFIX; no stale claim)
- IF T1/T2: `ssh_ng_max_silent_time_reaches_build_options` passes

## Tracey

References existing markers:
- `r[gw.opcode.set-options.propagation]` — T1 implements (IF propagation possible), T5 verifies. T4 updates the marker text either way (run `tracey bump`).
- `r[worker.silence.timeout-kill]` — referenced context only (the worker side is already implemented by P0215; this plan feeds it the client value).

## Files

```json files
[
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "T0: fix stale docstring :82-93 BEFORE investigation (disproven 10min after it was written); T3: dead-code decision on ClientOptions accessors + 5 tests :429-461"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T3: Some(opts) match arm :534-542 — keep-for-ssh:// or delete, per T0 finding"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "T1: parse client options from exec_request (IF T0 finds them there)"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T2: populate BuildOptions.max_silent_time from session_ctx"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "T3: retag/delete TODO(P0215) at :101"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T4: update r[gw.opcode.set-options.propagation] at :62 per T0 finding + tracey bump"}
]
```

```
rio-gateway/src/
├── handler/mod.rs                # T0: stale docstring :82  T3: dead-code decision :429-461
├── translate.rs                  # T3: Some(opts) :534 — keep-for-ssh:// or delete
├── server.rs                     # T1: exec_request parse (maybe)
└── handler/build.rs              # T2: BuildOptions populate
rio-worker/src/config.rs          # T3: TODO retag
docs/src/components/gateway.md    # T4: marker update
```

## Dependencies

```json deps
{"deps": [215], "soft_deps": [], "note": "P0215 proved ssh-ng never sends wopSetOptions; left TODO(P0215) self-tag at config.rs:101. T0 is wire-capture investigation — scope of T1/T2 depends on finding. IF options absent from wire entirely → WONTFIX + doc-only. discovered_from=P0215. CONSUMES the trivial TODO(P0215)-retag followup (folded into T3)."}
```

**Depends on:** [P0215](plan-0215-max-silent-time.md) — `max_silent_time_secs` worker config + the self-referential TODO.
**Conflicts with:** [`gateway.md`](../../docs/src/components/gateway.md) count=21 — coordinate with [P0295](plan-0295-doc-rot-batch-sweep.md) which also touches it (T4 here edits `:62`, P0295's appends target `:45`). [`handler/build.rs`](../../rio-gateway/src/handler/build.rs) count=24 — check dispatch window. [`server.rs`](../../rio-gateway/src/server.rs) also touched by [P0258](plan-0258-jwt-issuance-gateway.md) (JWT mint near `:354`) — non-overlapping sections.
