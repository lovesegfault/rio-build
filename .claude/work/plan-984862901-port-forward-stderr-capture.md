# Plan 984862901: port_forward stderr capture — silent 20s timeout on kubectl failure

[P0494](plan-0494-xtask-cli-tunnel-local-exec.md) review flagged [`xtask/src/k8s/k3s/smoke.rs:66-67`](../../xtask/src/k8s/k3s/smoke.rs): `port_forward` nulls both stdout and stderr on the spawned `kubectl port-forward`. When kubectl fails (wrong context, missing namespace, RBAC denial, service-not-found), the child exits immediately with a diagnostic on stderr — but the caller never sees it. Instead, the caller's readiness poll (`ui::poll` with SSH banner or TCP-accept) spins for its full 10×2s = 20s budget before timing out with a generic "poll exhausted" error.

This is the dev-loop-diagnostics failure mode [`feedback_error_messages_name_the_fix.md`](../../.claude/projects/memory/feedback_error_messages_name_the_fix.md) codifies: kubectl KNOWS exactly what went wrong and printed it; we threw it away. A user running `xtask k8s k3s smoke` against a kind cluster (wrong context) or with a typo'd namespace sees "scheduler+store TCP accept: timed out after 10 retries" — a riddle — instead of kubectl's own `Error from server (NotFound): services "rio-scheduler" not found`.

Narrow blast radius (dev-tooling, not production), but recurring annoyance. The fix is small: pipe stderr, check child liveness before each poll iteration, and surface the captured stderr on early-exit.

## Entry criteria

- [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) merged (`port_forward` + `tunnel_grpc` + `ProcessGuard` exist at their current locations)

## Tasks

### T1 — `fix(xtask):` port_forward — pipe stderr, surface on child early-exit

MODIFY [`xtask/src/k8s/k3s/smoke.rs:62-70`](../../xtask/src/k8s/k3s/smoke.rs). Change `.stderr(Stdio::null())` → `.stderr(Stdio::piped())`. `ProcessGuard` (or its callers) needs a way to check "did the child exit early?" and drain stderr if so.

Two-part shape:

1. **`port_forward`** keeps `.stdout(Stdio::null())` (kubectl's "Forwarding from 127.0.0.1:..." chatter is noise) but switches stderr to piped.

2. **`ProcessGuard`** gains a `try_diag(&mut self) -> Option<String>` (or similar) that calls `self.0.try_wait()` — if `Some(status)`, drain `self.0.stderr` into a String and return it. Non-blocking: if the child is still alive, returns `None`.

3. **Callers** (`tunnel`, `tunnel_grpc`) check `guard.try_diag()` inside their `ui::poll` closure BEFORE the connect attempt. If `Some(stderr)`, bail with a formatted error: `anyhow::bail!("kubectl port-forward exited early:\n{stderr}")`. The poll short-circuits immediately instead of burning 20s.

```rust
// Inside ui::poll closure, before TcpStream::connect:
if let Some(stderr) = guard.try_diag() {
    anyhow::bail!("kubectl port-forward exited early:\n{stderr}");
}
```

Stdout stays nulled — kubectl's happy-path "Forwarding from..." lines are not diagnostic.

### T2 — `fix(xtask):` tunnel_grpc — check both guards in poll closure

MODIFY [`xtask/src/k8s/k3s/smoke.rs:77-93`](../../xtask/src/k8s/k3s/smoke.rs). `tunnel_grpc` spawns TWO port-forwards (scheduler + store). The poll closure must check both guards — if either exited early, surface which one + its stderr. The error message should name the service (`rio-scheduler` vs `rio-store`) so the user knows which `-n` flag or service name to check.

```rust
if let Some(stderr) = sched.try_diag() {
    anyhow::bail!("kubectl port-forward svc/rio-scheduler exited early:\n{stderr}");
}
if let Some(stderr) = store.try_diag() {
    anyhow::bail!("kubectl port-forward svc/rio-store exited early:\n{stderr}");
}
```

### T3 — `fix(xtask):` tunnel (gateway SSH) — same early-exit check

MODIFY [`xtask/src/k8s/k3s/smoke.rs:45-57`](../../xtask/src/k8s/k3s/smoke.rs). `tunnel` (single port-forward for gateway:22) gets the same `try_diag()` check before `chaos::ssh_banner`. Same failure mode, same fix.

## Exit criteria

- `port_forward` pipes stderr (not null)
- `ProcessGuard` has a `try_diag` (or equivalent) non-blocking early-exit check
- `tunnel`, `tunnel_grpc` both short-circuit their poll on child early-exit with kubectl's stderr verbatim in the error
- Manual verify: `kubectl config use-context nonexistent && cargo run -p xtask -- k8s k3s smoke` → error appears in <3s (not 20s), contains kubectl's diagnostic text
- `cargo clippy --all-targets -- --deny warnings` green

## Tracey

No domain markers — xtask dev-tooling is below spec surface (no `r[xtask.*]` domain in `docs/src/components/`). This is pure diagnostics-quality, not a spec'd behavior.

## Files

```json files
[
  {"path": "xtask/src/k8s/k3s/smoke.rs", "action": "MODIFY", "note": "T1-T3: pipe stderr in port_forward, add try_diag to ProcessGuard, check in tunnel/tunnel_grpc poll closures"}
]
```

```
xtask/src/k8s/k3s/
└── smoke.rs          # T1-T3: stderr pipe + early-exit checks
```

## Dependencies

```json deps
{"deps": [494], "soft_deps": [984862902], "note": "discovered_from=494. Soft-dep P984862902 (RIO_* env consolidation) touches xtask/src/k8s/mod.rs + eks/smoke.rs but NOT k3s/smoke.rs port_forward — non-overlapping. If P984862902's port-const hoist (absorbed from trivial row) touches k3s/smoke.rs:14-15, that's a different section (const decls vs port_forward fn body)."}
```

**Depends on:** [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) — `port_forward`, `tunnel_grpc`, `ProcessGuard` exist.
**Soft-dep:** [P984862902](plan-984862902-rio-env-consolidation-cli-ctx.md) — touches adjacent xtask/k8s files but not `port_forward` itself; sequence-independent.
**Conflicts with:** none in top-20 collisions. `xtask/src/k8s/k3s/smoke.rs` is low-traffic.
