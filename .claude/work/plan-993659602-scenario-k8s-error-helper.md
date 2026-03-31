# Plan 993659602: Scenario::k8s_error helper — consolidate 7 inline error-Status fixtures

Consolidator mc=85 finding: seven near-identical inline `Scenario { ... body_json: serde_json::json!({"kind": "Status", "status": "Failure", ...}) }` fixtures across rio-controller and rio-scheduler tests, **four of which landed in the mc81-85 window**. [`ephemeral_tests.rs:90-101`](../../rio-controller/src/reconcilers/builderpool/tests/ephemeral_tests.rs) is byte-compatible with [`manifest_tests.rs:1514-1527` `forbidden_scenario()`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs) — [P0522](plan-0522-warn-continue-escalation.md) factored a local helper without seeing that [P0526](plan-0526-ephemeral-spawn-bail-job-common.md) wrote the same 12 lines three merges earlier. Cross-merge invisibility: neither implementer's worktree had the other's file.

The shape is always the same — K8s error-Status responses have a fixed envelope:

```json
{"kind": "Status", "apiVersion": "v1", "status": "Failure", "reason": "<Reason>", "code": <NNN>, "message": "<free text>"}
```

Only `method`, `path_contains`, `code`, `reason`, `message` vary. [`Scenario::ok` at kube_mock.rs:49-57](../../rio-test-support/src/kube_mock.rs) already established the constructor-shorthand pattern for the happy path. Add the error-path sibling.

Call-site census (`"status": "Failure"` grep):

| File:line | code | reason | Landed |
|---|---|---|---|
| [`fixtures.rs:151`](../../rio-controller/src/fixtures.rs) | 404 | NotFound | pre-window |
| [`apply_tests.rs:351`](../../rio-controller/src/reconcilers/builderpool/tests/apply_tests.rs) | 409 | Conflict | pre-window |
| [`ephemeral_tests.rs:97`](../../rio-controller/src/reconcilers/builderpool/tests/ephemeral_tests.rs) | 403 | Forbidden | mc81-85 (P0526) |
| [`ephemeral_tests.rs:151`](../../rio-controller/src/reconcilers/builderpool/tests/ephemeral_tests.rs) | 409 | AlreadyExists | mc81-85 (P0526) |
| [`manifest_tests.rs:1522`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs) | 403 | Forbidden | mc81-85 (P0522) |
| [`manifest_tests.rs:1702`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs) | 409 | AlreadyExists | mc81-85 (P0522) |
| [`election.rs:470`](../../rio-scheduler/src/lease/election.rs) | 409 | Conflict | pre-window |
| [`election.rs:562`](../../rio-scheduler/src/lease/election.rs) | 404 | NotFound | pre-window |

Eight sites, ~10 lines each. Helper is ~15 lines; net ~70 lines removed. More importantly: the ninth site (next plan's error-path test) becomes a one-liner instead of a 12-line copy.

## Tasks

### T1 — `feat(test-support):` add Scenario::k8s_error constructor

At [`rio-test-support/src/kube_mock.rs` after `Scenario::ok` (`:57`)](../../rio-test-support/src/kube_mock.rs):

```rust
/// Shorthand: K8s error-Status response. The body follows the
/// `metav1.Status` envelope that `kube::Error::Api` deserializes
/// from — `reason` maps to `ErrorResponse.reason`, `code` to
/// `.code`, `message` to `.message`. Test code typically matches
/// on `kube::Error::Api(ae) if ae.code == <N>`, so get the code
/// right; reason/message are diagnostic.
///
/// ```
/// # use rio_test_support::kube_mock::Scenario;
/// # use http::Method;
/// let forbidden = Scenario::k8s_error(
///     Method::POST, "/namespaces/rio/jobs",
///     403, "Forbidden", "jobs.batch is forbidden: exceeded quota",
/// );
/// ```
pub fn k8s_error(
    method: http::Method,
    path_contains: &'static str,
    code: u16,
    reason: &'static str,
    message: &'static str,
) -> Self {
    Self {
        method,
        path_contains,
        body_contains: None,
        status: code,
        body_json: serde_json::json!({
            "kind": "Status",
            "apiVersion": "v1",
            "status": "Failure",
            "reason": reason,
            "code": code,
            "message": message,
        })
        .to_string(),
    }
}
```

`body_contains: None` matches every existing call site (none assert on the REQUEST body for error scenarios — they're testing what the code-under-test DOES with a failure response, not what it sent). If a future site needs `body_contains`, it can use the struct-literal form; don't plumb a sixth parameter for zero current users.

### T2 — `refactor(controller):` replace inline error-Status → Scenario::k8s_error

Replace five rio-controller sites. Each is a mechanical `Scenario { .. json!(..) }` → `Scenario::k8s_error(..)` swap:

**[`fixtures.rs:~145-155`](../../rio-controller/src/fixtures.rs):** 404 NotFound (check context — this is already in a fixtures module, may already be a helper; if so, delegate to `Scenario::k8s_error` inside the existing helper instead of deleting it).

**[`apply_tests.rs:~345-355`](../../rio-controller/src/reconcilers/builderpool/tests/apply_tests.rs):** 409 Conflict.

**[`ephemeral_tests.rs:90-101`](../../rio-controller/src/reconcilers/builderpool/tests/ephemeral_tests.rs):** 403 Forbidden → `Scenario::k8s_error(http::Method::POST, "/namespaces/rio/jobs", 403, "Forbidden", "jobs.batch is forbidden: exceeded quota")`.

**[`ephemeral_tests.rs:~145-155`](../../rio-controller/src/reconcilers/builderpool/tests/ephemeral_tests.rs):** 409 AlreadyExists.

**[`manifest_tests.rs:1514-1527`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs):** `forbidden_scenario()` body → delegate. **Keep the local `fn forbidden_scenario()` wrapper** — it's called four times in the same file (`:1574`, `:1639`, `:1694`, `:1707`). Body shrinks to one line:

```rust
fn forbidden_scenario() -> Scenario {
    Scenario::k8s_error(
        http::Method::POST, "/namespaces/rio/jobs",
        403, "Forbidden", "jobs.batch is forbidden: exceeded quota",
    )
}
```

The `:1511-1513` doc-comment ("403 Forbidden stands in for quota exceeded — P0516") stays — it's call-site rationale, not envelope mechanics.

**[`manifest_tests.rs:~1696-1710`](../../rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs):** 409 AlreadyExists inline.

### T3 — `refactor(scheduler):` replace inline error-Status → Scenario::k8s_error

Two rio-scheduler sites in [`election.rs`](../../rio-scheduler/src/lease/election.rs):

**`:~464-475`:** 409 Conflict (lease contention).
**`:~556-566`:** 404 NotFound (lease missing).

Both in `#[cfg(test)]` blocks. Add `use rio_test_support::kube_mock::Scenario;` if not already present (check existing imports — `ApiServerVerifier` is likely already imported from the same module).

## Exit criteria

- `grep -c '"status": "Failure"' rio-controller/src/ rio-scheduler/src/ -r` → 0 (all inline envelopes gone)
- `grep -c 'Scenario::k8s_error' rio-controller/src/ rio-scheduler/src/ -r` → ≥7 (every site converted)
- `grep 'fn forbidden_scenario' rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs` → ≥1 hit (local wrapper retained, body delegates)
- `nix develop -c cargo nextest run -p rio-controller -p rio-scheduler` green (pure refactor — test behavior unchanged)
- `nix develop -c cargo clippy --all-targets -- --deny warnings` green
- `/nixbuild .#ci` green

## Tracey

No spec markers. Test-support helper — consolidates fixture boilerplate, no behavior change, no `r[impl]` / `r[verify]`.

## Files

```json files
[
  {"path": "rio-test-support/src/kube_mock.rs", "action": "MODIFY", "note": "T1: add Scenario::k8s_error constructor after :57"},
  {"path": "rio-controller/src/fixtures.rs", "action": "MODIFY", "note": "T2: :151 NotFound → k8s_error (check if already helper — delegate inside if so)"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/apply_tests.rs", "action": "MODIFY", "note": "T2: :351 Conflict → k8s_error"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/ephemeral_tests.rs", "action": "MODIFY", "note": "T2: :90-101 Forbidden + :151 AlreadyExists → k8s_error"},
  {"path": "rio-controller/src/reconcilers/builderpool/tests/manifest_tests.rs", "action": "MODIFY", "note": "T2: forbidden_scenario() body :1514-1527 delegates; :1702 AlreadyExists → k8s_error"},
  {"path": "rio-scheduler/src/lease/election.rs", "action": "MODIFY", "note": "T3: :470 Conflict + :562 NotFound → k8s_error (cfg(test) block)"}
]
```

```
rio-test-support/src/
└── kube_mock.rs         # T1: Scenario::k8s_error
rio-controller/src/
├── fixtures.rs          # T2
└── reconcilers/builderpool/tests/
    ├── apply_tests.rs      # T2
    ├── ephemeral_tests.rs  # T2
    └── manifest_tests.rs   # T2
rio-scheduler/src/lease/
└── election.rs          # T3
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [522, 526], "note": "Pure additive helper + mechanical swap. No hard deps. Soft-dep P0522/P0526: they LANDED the mc81-85 sites; both DONE, listed for provenance only (discovered_from=consolidator-mc85). No ordering constraint — this refactors already-merged code."}
```

**Depends on:** none. `Scenario` struct, `ApiServerVerifier`, `serde_json::json!` — all present since pre-sprint-1.
**Soft-dep:** [P0522](plan-0522-warn-continue-escalation.md) + [P0526](plan-0526-ephemeral-spawn-bail-job-common.md) — both DONE; provenance refs (they landed the duplicated code).
**Conflicts with:** `manifest_tests.rs` — [P0311-T501](plan-0311-test-gap-batch-cli-recovery-dash.md) adds `is_floor_job` test, [P0311-T503](plan-0311-test-gap-batch-cli-recovery-dash.md) adds spawn-error mock (SUBSUMED by P0522 per its body note) — both additive test-fns at file tail, this plan edits `:1514-1527` + `:~1700` (mid-file), non-overlapping hunks. [P0295-T993659609](plan-0295-doc-rot-batch-sweep.md) edits the doc-comment at `:1558-1560` — adjacent to `:1514-1527` but non-overlapping lines; prefer T993659609 lands first (doc-fix before code-shuffle) or rebase-trivial either way. `ephemeral_tests.rs` low-traffic. `election.rs` cfg(test) block low-traffic. `kube_mock.rs` count<5 — additive constructor.
