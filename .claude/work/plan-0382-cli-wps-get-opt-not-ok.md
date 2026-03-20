# Plan 0382: rio-cli wps .ok() → get_opt — distinguish 403/500 from child-not-found

Reviewer (rev-p237) flagged [`rio-cli/src/wps.rs:151`](../../rio-cli/src/wps.rs) (p237 worktree ref — P0237 UNIMPL at this writing):

```rust
let child_status = wp_api.get(&child_name).await.ok().and_then(|wp| wp.status);
```

`.ok()` swallows ALL kube errors. An operator running `rio wps describe` to diagnose a misbehaving autoscaler sees `-/-` replicas for a class regardless of whether:
- the child WorkerPool genuinely doesn't exist yet (reconciler lag — benign, wait),
- the service account lacks `get workerpools` (RBAC 403 — fix ClusterRole),
- the apiserver returned 500 (cluster degraded — escalate), or
- the network blipped (transient — retry).

All four render identically. The comment at [`wps.rs:148-150`](../../rio-cli/src/wps.rs) SAYS "child may not exist yet (reconciler lag, or the child create failed — check the WPS .status.conditions for the latter)" — but 403 doesn't populate `.status.conditions`, so the comment's own escape-hatch doesn't work for the RBAC case.

Codebase convention is `get_opt` for "might not exist" lookups — [`workerpoolset/mod.rs:216`](../../rio-controller/src/reconcilers/workerpoolset/mod.rs) does the SAME child-WP lookup correctly: `Ok(Some(wp))` / `Ok(None)` / `Err(e)` distinguished. `kube::Api::get_opt()` returns `Result<Option<T>, kube::Error>` — 404 maps to `Ok(None)`, everything else is `Err`.

This is `correctness` severity not `trivial` — the silent-swallow makes CLI diagnostics actively misleading in the RBAC-misconfig case, which is a common post-deploy failure mode (helm chart RBAC missed the WorkerPool `get` verb on the operator's kubectl-SA).

## Entry criteria

- [P0237](plan-0237-rio-cli-wps.md) merged (`rio-cli/src/wps.rs` and the `describe` subcommand exist)

## Tasks

### T1 — `fix(cli):` wps describe — get_opt not .ok() for child WorkerPool lookup

Replace the `.ok()` swallow at [`wps.rs:151`](../../rio-cli/src/wps.rs) with `get_opt` + explicit error handling:

```rust
let child_status = match wp_api.get_opt(&child_name).await {
    Ok(Some(wp)) => wp.status,
    Ok(None) => {
        // Child not created yet (reconciler lag). Render -/-.
        None
    }
    Err(kube::Error::Api(ae)) if ae.code == 403 => {
        eprintln!(
            "warning: RBAC denied `get workerpools/{child_name}` ({}). \
             Child status unavailable — check service account permissions.",
            ae.message
        );
        None
    }
    Err(e) => {
        // 500/network/etc — surface, don't silently swallow.
        // Return Err rather than continuing with a misleading -/-
        // for the remaining classes: if the apiserver is degraded
        // for one child it's degraded for all.
        anyhow::bail!("get WorkerPool {ns}/{child_name}: {e}");
    }
};
```

Update the comment at `:148-150` to match: `// get_opt — child may not exist yet (Ok(None) → -/-); 403 → warn; 500/network → bail (apiserver degraded means all rows would be misleading)`.

### T2 — `test(cli):` wps describe distinguishes 404 from 403 from 500

Add to `rio-cli/tests/smoke.rs` (or a new `wps.rs` test file if [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T49 extracts one). Uses `rio_test_support::kube_mock::ApiServerVerifier` (same pattern as [`rio-controller/src/fixtures.rs`](../../rio-controller/src/fixtures.rs)):

```rust
#[tokio::test]
async fn wps_describe_403_warns_not_swallows() {
    let scenarios = vec![
        Scenario {
            verb: "GET",
            path_suffix: "workerpoolsets/test-wps",
            response_code: 200,
            response_body: /* WPS with one class */ ,
            body_contains: None,
        },
        Scenario {
            verb: "GET",
            path_suffix: "workerpools/test-wps-small",
            response_code: 403,
            response_body: json!({"code":403,"message":"forbidden: workerpools.rio.build"}),
            body_contains: None,
        },
    ];
    // capture stderr, assert contains "RBAC denied"
}

#[tokio::test]
async fn wps_describe_500_bails() {
    // same but response_code: 500 → expect Err, not silent -/-
}
```

## Exit criteria

- `/nixbuild .#ci` green (or nextest-standalone clause-4c)
- `grep '\.ok()' rio-cli/src/wps.rs` → 0 hits (swallow removed)
- `grep 'get_opt' rio-cli/src/wps.rs` → ≥1 hit (convention-aligned)
- `grep '403\|RBAC denied' rio-cli/src/wps.rs` → ≥1 hit (explicit RBAC branch)
- `cargo nextest run -p rio-cli wps_describe_403\|wps_describe_500` → 2 passed
- Manual sanity (optional, not CI-gated): `rio wps describe test-wps` against a cluster with WorkerPool-get denied → stderr warning + `-/-` row, NOT silent `-/-`

## Tracey

No new markers. This is a CLI diagnostic correctness fix; the `wps` subcommand has no spec marker (it's operator tooling, not protocol). The closest is `r[ctrl.wps.reconcile]` which describes the reconciler's child-creation; the CLI observes that, it doesn't implement it.

## Files

```json files
[
  {"path": "rio-cli/src/wps.rs", "action": "MODIFY", "note": "T1: :151 .ok()→get_opt with 404/403/500 branches; :148-150 comment update (p237 worktree refs — P0237 UNIMPL)"},
  {"path": "rio-cli/tests/smoke.rs", "action": "MODIFY", "note": "T2: +wps_describe_403_warns_not_swallows + wps_describe_500_bails (uses kube_mock ApiServerVerifier)"}
]
```

```
rio-cli/
├── src/wps.rs           # T1: .ok() → get_opt + 403 warn + 500 bail
└── tests/smoke.rs       # T2: +2 test fns
```

## Dependencies

```json deps
{"deps": [237], "soft_deps": [311], "note": "P0237 must merge first (wps.rs:151 exists only in p237 worktree). Soft-dep P0311-T49 (adds cli.nix wps subtest — VM-level coverage; this plan adds unit-level; orthogonal). Discovered_from=237 (rev-p237 correctness)."}
```

**Depends on:** [P0237](plan-0237-rio-cli-wps.md) — `wps.rs` and the `describe` subcommand arrive with it.

**Conflicts with:** `rio-cli/src/wps.rs` is P0237-new — no prior plan touches it. `smoke.rs` touched by [P0304](plan-0304-trivial-batch-p0222-harness.md)-T5 (delete stale `RIO_CONFIG_PATH` comment at `:44-47`) — non-overlapping, T2 here adds test fns at end-of-file. [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T49 adds `cli.nix` wps subtest (different file, VM-level complement to this plan's unit-level).
