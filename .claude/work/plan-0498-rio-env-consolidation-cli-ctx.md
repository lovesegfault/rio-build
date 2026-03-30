# Plan 498: RIO_* env consolidation — CliCtx vs with_cli_tunnel duplication

Two reviewers independently flagged the same shape: [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) review noted the PATH-probe + RIO_* env-push duplication between [`CliCtx::run` at eks/smoke.rs:71-84`](../../xtask/src/k8s/eks/smoke.rs) and [`with_cli_tunnel` at mod.rs:374-392`](../../xtask/src/k8s/mod.rs); the consolidator flagged it with a fuller schema-contract analysis.

Both sites push the identical five env vars (`RIO_SCHEDULER_ADDR`, `RIO_STORE_ADDR`, `RIO_TLS__CERT_PATH`, `RIO_TLS__KEY_PATH`, `RIO_TLS__CA_PATH`) in the identical order against an `xshell::Shell`, and both probe PATH for `rio-cli` with a fallback to `cargo run -p rio-cli`. The duplication is the env-push block (5 lines × 2 sites = 10 lines) plus the PATH-probe (6 lines × 2 sites ≈ 12 lines; `CliCtx::run` inlines it, `with_cli_tunnel` defers to the caller's `f`).

**Conditional worth-it:** consolidator's analysis says this triggers on the 6th env var OR the 3rd call site. As of P0494 there are exactly 2 sites and exactly 5 vars. Adding a 6th var (e.g., `RIO_LOG_FORMAT` for cli-side JSON output, or `RIO_TIMEOUT` for long-running status queries) is plausible but not scheduled. A 3rd call site is more likely — `xtask k8s kind smoke` doesn't yet use the local-cli pattern but probably should.

**Schema-contract risk the consolidator named:** rio-cli's config loader reads these env vars by exact name. If one site adds `RIO_TLS__CA_BUNDLE_PATH` (new var) and the other doesn't, the two smoke tests diverge silently — one authenticates with the CA, the other doesn't, and the failure manifests as an mTLS handshake error with no hint that the env-push is incomplete. A single `push_rio_cli_env(&sh, sched, store, &tls)` helper makes the contract explicit.

**Decision:** ship the helper now. It's ~15 lines net-negative and the schema-contract argument is real even at 2 sites. If the 6th-var / 3rd-site trigger never fires, we've still removed a divergence footgun.

## Entry criteria

- [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) merged (`CliCtx::run` + `with_cli_tunnel` both exist with the current env-push duplication)

## Tasks

### T1 — `refactor(xtask):` extract `push_rio_cli_env` helper

NEW helper in [`xtask/src/k8s/mod.rs`](../../xtask/src/k8s/mod.rs) (near `with_cli_tunnel`, ~:374). Signature takes the shell, the two ports, and a TLS-paths triple (or a small struct):

```rust
/// Push the 5 RIO_* env vars rio-cli reads for scheduler/store addr +
/// mTLS paths. Returns the push-guards — drop to unset.
///
/// Shared by `with_cli_tunnel` and `CliCtx::run` — both drive rio-cli
/// locally against a port-forwarded cluster. Keep the var list here in
/// lockstep with rio-cli's config loader (`rio-cli/src/config.rs`).
pub(crate) fn push_rio_cli_env<'a>(
    sh: &'a xshell::Shell,
    sched: u16,
    store: u16,
    cert: &str,
    key: &str,
    ca: &str,
) -> [xshell::PushEnv<'a>; 5] {
    [
        sh.push_env("RIO_SCHEDULER_ADDR", format!("localhost:{sched}")),
        sh.push_env("RIO_STORE_ADDR", format!("localhost:{store}")),
        sh.push_env("RIO_TLS__CERT_PATH", cert),
        sh.push_env("RIO_TLS__KEY_PATH", key),
        sh.push_env("RIO_TLS__CA_PATH", ca),
    ]
}
```

The `[PushEnv; 5]` return keeps the drop-guard semantics — caller binds to `let _env = ...;` and the vars unset at scope exit, same as the current `let _e1 = ...; let _e2 = ...;` pattern.

### T2 — `refactor(xtask):` with_cli_tunnel uses helper

MODIFY [`xtask/src/k8s/mod.rs:385-390`](../../xtask/src/k8s/mod.rs). Replace the five `let _eN = sh.push_env(...)` lines with one call to `push_rio_cli_env`.

### T3 — `refactor(xtask):` CliCtx::run uses helper

MODIFY [`xtask/src/k8s/eks/smoke.rs:73-77`](../../xtask/src/k8s/eks/smoke.rs). Replace the five `let _eN = sh.push_env(...)` lines with one call to `push_rio_cli_env`. Import from `crate::k8s` (or wherever T1 places it).

### T4 — `refactor(xtask):` extract `rio_cli_cmd` PATH-probe helper

OPTIONAL (consolidator called this lower-value than the env-push — the PATH-probe is a self-contained 6-line block with no schema contract). If doing it: extract the `command -v rio-cli` → `rio-cli {args}` / `cargo run -p rio-cli -- {args}` branch into a helper that takes the shell + args and returns the `Cmd`. `CliCtx::run` and any future `with_cli_tunnel` caller that invokes rio-cli directly share it.

Skip if the implementer judges it over-abstracted at 1 confirmed call site (`with_cli_tunnel`'s `f` closure is caller-provided — it may or may not run rio-cli).

## Exit criteria

- `push_rio_cli_env` exists, returns `[PushEnv; N]` drop-guards
- `with_cli_tunnel` + `CliCtx::run` both call it (grep `push_env.*RIO_SCHEDULER_ADDR` → 1 hit, inside the helper)
- `cargo clippy --all-targets -- --deny warnings` green
- `xtask k8s eks smoke` + `xtask k8s cli -- status` both still work (manual; no CI coverage for live-cluster commands)
- Net line delta ≤ 0 (helper adds ~15, two sites drop ~10 each; if positive, T4 was probably skipped — acceptable)

## Tracey

References existing markers:
- `r[sec.image.control-plane-minimal]` — `with_cli_tunnel` carries `r[impl]` at [`mod.rs:360`](../../xtask/src/k8s/mod.rs); this refactor preserves the behavior (local rio-cli via tunnel instead of in-pod exec). No annotation change.

## Files

```json files
[
  {"path": "xtask/src/k8s/mod.rs", "action": "MODIFY", "note": "T1: new push_rio_cli_env helper ~:374. T2: with_cli_tunnel uses it :385-390"},
  {"path": "xtask/src/k8s/eks/smoke.rs", "action": "MODIFY", "note": "T3: CliCtx::run uses push_rio_cli_env :73-77. T4 (optional): rio_cli_cmd PATH-probe helper"}
]
```

```
xtask/src/k8s/
├── mod.rs            # T1+T2: push_rio_cli_env helper, with_cli_tunnel uses it
└── eks/smoke.rs      # T3(+T4): CliCtx::run uses helper
```

## Dependencies

```json deps
{"deps": [494], "soft_deps": [497], "note": "discovered_from=494 (review) + consolidator (independent flag, merged per dedup note). Conditional-worth-it: triggers on 6th env var OR 3rd call site; shipping preemptively for schema-contract safety. Soft-dep P0497 (port_forward stderr) touches k3s/smoke.rs port_forward fn — DIFFERENT file from this plan's mod.rs + eks/smoke.rs; zero overlap."}
```

**Depends on:** [P0494](plan-0494-xtask-cli-tunnel-local-exec.md) — both duplication sites exist.
**Soft-dep:** [P0497](plan-0497-port-forward-stderr-capture.md) — adjacent xtask/k8s work, non-overlapping files.
**Conflicts with:** none in top-20 collisions. P0304-T497 (port-const hoist) touches `mod.rs:199,202` (clap `default_value_t`) — different section from this plan's `:374-392` (`with_cli_tunnel` body); non-overlapping.
