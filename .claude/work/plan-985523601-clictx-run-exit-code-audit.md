# Plan 985523601: CliCtx::run exit-code semantics — audit remaining callers

[P0494](plan-0494-xtask-cli-tunnel-local-exec.md) migrated xtask k8s smoke steps from `run_in_scheduler` (kubectl exec) to `CliCtx::run` (local rio-cli via port-forward). The migration changed exit-code semantics: `run_in_scheduler` returned stdout regardless of exit code; `CliCtx::run` uses `sh::read()` which propagates non-zero exit as `Err`. `step_tenant` broke in prod at [`5b98e311`](https://github.com/search?q=5b98e311&type=commits) — `cli.run(&["create-tenant", ...])?` bailed on `AlreadyExists` before the idempotent-re-run check could see the output.

The fix at [`5b98e311`](https://github.com/search?q=5b98e311&type=commits) patched `step_tenant` alone. **Two more callers use `CliCtx::run` and may have the same assumption:** [`step_status` at eks/smoke.rs:448-462](../../xtask/src/k8s/eks/smoke.rs) and [`gather` at status.rs:128-134](../../xtask/src/k8s/status.rs). Quick audit + potential fixes — this is the second prod regression from P0494 (third is the HA tunnel break, tracked separately at [P985523602](plan-985523602-prod-parity-vm-fixture.md)).

## Entry criteria

- [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) merged (`CliCtx::run` exists, all three callers migrated)

## Tasks

### T1 — `fix(xtask):` audit step_status for exit-code-Err assumption

Read [`step_status` at eks/smoke.rs:448-462](../../xtask/src/k8s/eks/smoke.rs). Current shape:

```rust
let out = cli.run(&["status"])?;
// ... check out.contains("worker "), out.contains("build ")
```

`rio-cli status` exits 0 on success regardless of cluster state (empty cluster is not an error). So the `?` here is *probably* correct — `Err` means "couldn't reach the scheduler" (tunnel broke, TLS mismatch), which SHOULD bail. But verify: grep `rio-cli/src/` for the status subcommand's exit paths. If `status` ever exits non-zero for a recoverable state (e.g., "scheduler reachable but no workers registered yet" → exit 2), `step_status` needs the same match-on-Err treatment as `step_tenant`.

**Expected outcome:** likely no change (status is a pure read). If a non-zero-but-recoverable path exists, apply the `step_tenant` pattern: match `Err`, check error text, decide propagate-vs-retry-vs-ok. Document the audit result in a code comment either way — `// CliCtx::run ? is correct here: rio-cli status exits 0 on any reachable state` — so the next P0494-class migration doesn't re-audit.

### T2 — `fix(xtask):` audit status.rs gather for exit-code-Err assumption

Read [`gather` at status.rs:128-134](../../xtask/src/k8s/status.rs). Current shape already matches on `Err`:

```rust
rio_cli: match CliCtx::open(client, 19001, 19002).await {
    Ok(cli) => match cli.run(&["status"]) {
        Ok(out) => RioCli::Output(out),
        Err(e) => RioCli::Error(format!("{e:#}")),
    },
    Err(e) => RioCli::Error(format!("tunnel: {e:#}")),
},
```

This is **already correct** — `gather` is best-effort (every section in `Report` degrades to an error line, never propagates). Verify the `Err(e)` arm renders the rio-cli stderr usefully: `sh::read` wraps the stderr in the error chain, so `{e:#}` (alternate Debug) should show it. If not, the `RioCli::Error` branch shows "command failed" with no context — add `.context("rio-cli status")` or extract stderr explicitly.

**Expected outcome:** likely no change (already matched). If stderr is swallowed, add context. Document with a one-line comment — `// Err arm: sh::read propagates rio-cli stderr in the error chain`.

### T3 — `refactor(xtask):` doc-comment CliCtx::run exit-code contract

MODIFY [`CliCtx::run` doc-comment at eks/smoke.rs:68-70](../../xtask/src/k8s/eks/smoke.rs). Current comment says "capture combined output" — doesn't mention that non-zero exit → `Err`. Add one sentence:

```rust
/// Run rio-cli locally with RIO_SCHEDULER_ADDR/RIO_STORE_ADDR/
/// RIO_TLS__* set, capture combined output. Prefers an installed
/// `rio-cli` on PATH; falls back to `cargo run -p rio-cli`.
///
/// **Exit-code contract:** non-zero exit propagates as `Err` via
/// `sh::read`. Callers that tolerate expected-failure exits (e.g.,
/// `create-tenant` on `AlreadyExists`) must match the `Err` arm and
/// inspect the error text before propagating. See `step_tenant`.
```

This is the cheap documentation fix that prevents the fourth regression.

## Exit criteria

- `/nbr .#ci` green
- T1: `step_status` either unchanged (with audit-comment) OR carries match-on-Err for recoverable non-zero exit; `grep -A1 'cli.run.*status' xtask/src/k8s/eks/smoke.rs` shows either `?;` + comment OR `match`
- T2: `gather`'s `RioCli::Error` branch renders rio-cli stderr; manual test: stop scheduler, run `xtask k8s status` → error line contains diagnostic (not bare "command failed")
- T3: `grep 'Exit-code contract' xtask/src/k8s/eks/smoke.rs` → ≥1 hit

## Tracey

No new markers. xtask smoke-test tooling is not spec-covered (no `r[xtask.*]` domain). The underlying `r[sched.grpc.leader-guard]` behavior (standbys reject writes) is what `CliCtx` works around, but this plan touches the caller side only — no `r[impl]`/`r[verify]` change.

## Files

```json files
[
  {"path": "xtask/src/k8s/eks/smoke.rs", "action": "MODIFY", "note": "T1: audit step_status ? vs match-Err; T3: CliCtx::run doc-comment exit-code contract"},
  {"path": "xtask/src/k8s/status.rs", "action": "MODIFY", "note": "T2: verify gather Err-arm renders stderr; comment or .context()"}
]
```

```
xtask/src/k8s/
├── eks/smoke.rs          # T1: step_status audit; T3: doc-comment
└── status.rs             # T2: gather Err-arm audit
```

## Dependencies

```json deps
{"deps": [494], "soft_deps": [498], "note": "HARD-DEP P0494 (CliCtx::run + callers exist). SOFT-DEP P0498 (RIO_* env consolidation — touches CliCtx env-setup at :73-77, orthogonal to exit-code semantics; sequence-independent). discovered_from=494+coverage."}
```

**Depends on:** [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) — `CliCtx::run` and the three callers exist.
**Soft-dep:** [P0498](plan-0498-rio-env-consolidation.md) — touches `CliCtx` env-setup lines; this plan touches the doc-comment and callers. Non-overlapping edits within the same impl block.
**Conflicts with:** [P0304](plan-0304-trivial-batch-p0222-harness.md)-T497 touches `eks/smoke.rs:30-31` (port consts) and `status.rs:128` (inline ports → const). This plan touches `:68-70` (doc-comment), `:448-462` (step_status), `:128-134` (gather match arms). T497's `:128` edit is inside this plan's `:128-134` range — whoever lands second re-reads. Both additive/orthogonal.
