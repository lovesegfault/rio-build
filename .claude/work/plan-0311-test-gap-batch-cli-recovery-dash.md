# Plan 0311: Test-gap batch — cli non-empty assertions, failure_count recovery, dash annotation

Five test gaps from the reviewer sink. No open test-gap batch existed. T1-T3 are [P0216](plan-0216-rio-cli-subcommands.md) review findings — the cli ships pretty-print and filter code that the VM test never exercises because `MockAdmin` returns empty collections and `cli.nix` only asserts empty-case output. T4 is a [P0219](plan-0219-per-worker-failure-budget.md) coverage gap (`failure_count` recovery-init is documented but untested). T5 is a one-line annotation guidance note for [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) (marker bundles client+server; P0276 implements server-half but has zero `r[dash.*]` refs).

**Line numbers are from plan worktrees**, not sprint-1 — `rio-cli/src/main.rs` references are from p216 (`fe1a79a6`); re-grep at dispatch.

## Entry criteria

- [P0216](plan-0216-rio-cli-subcommands.md) merged (`print_build`, `BuildJson::from`, `workers --status`, GC dirty-close warning all exist)
- [P0219](plan-0219-per-worker-failure-budget.md) merged (`failure_count` recovery-init at [`derivation.rs:366`](../../rio-scheduler/src/state/derivation.rs)) — **DONE**
- [P0223](plan-0223-seccomp-localhost-profile.md) merged (seccomp CEL rules + `build_seccomp_profile` exist for T6/T7)

## Tasks

### T1 — `test(cli):` print_build + BuildJson::from populated path

MODIFY [`nix/tests/scenarios/cli.nix`](../../nix/tests/scenarios/cli.nix) OR `rio-cli/tests/smoke.rs` (check which is the right home at dispatch — `cli.nix:167` comment in p216 claims "populated-state assertion lives in lifecycle.nix" but grep confirms nothing there):

[`main.rs:694`](../../rio-cli/src/main.rs) (p216) `print_build` and [`main.rs:592`](../../rio-cli/src/main.rs) `BuildJson::from` never execute in tests — `MockAdmin` returns `ListBuildsResponse { builds: vec![] }` and the test asserts `"(no builds)"` only. Add a populated case:

```python
# cli.nix — after the empty-case assertion, submit a real build via
# nix-build or grpcurl SubmitBuild, then:
with subtest("cli builds --json: populated"):
    out = client.succeed("rio-cli --json builds")
    data = json.loads(out)
    assert len(data["builds"]) >= 1, f"expected populated, got {out}"
    b = data["builds"][0]
    # Exercises BuildJson::from field mapping.
    assert "build_id" in b and "status" in b

with subtest("cli builds: human-readable populated"):
    out = client.succeed("rio-cli builds")
    # Exercises print_build formatting. Don't be too specific about
    # format — just prove the codepath ran (build_id appears somewhere).
    assert data["builds"][0]["build_id"] in out
```

If `cli.nix` uses `MockAdmin`, this needs a real scheduler — move to `lifecycle.nix` or extend `MockAdmin` to return a non-empty fixture. **Check at dispatch which is easier.**

### T2 — `test(cli):` workers --status filter non-empty

MODIFY same test file as T1. [`main.rs:320`](../../rio-cli/src/main.rs) (p216) `status_filter: status.unwrap_or_default()` is never exercised with a non-empty filter. The `--status` flag was added beyond the plan spec (reviewer note: "added beyond plan spec, never exercised non-empty").

```python
with subtest("cli workers --status alive: filter works"):
    # Assumes at least one alive worker exists (VM fixture has one).
    out = client.succeed("rio-cli --json workers --status alive")
    data = json.loads(out)
    assert all(w["status"] == "alive" for w in data["workers"])

with subtest("cli workers --status draining: empty when none draining"):
    out = client.succeed("rio-cli --json workers --status draining")
    data = json.loads(out)
    assert data["workers"] == []
```

This covers `r[sched.admin.list-workers]` at [`scheduler.md:129`](../../docs/src/components/scheduler.md): "The optional `status_filter` matches 'alive' (registered + not draining), 'draining', or empty/unknown (show all)."

### T3 — `test(cli):` GC dirty-close warning presence

MODIFY same test file. [`main.rs:445`](../../rio-cli/src/main.rs) (p216) emits `"warning: GC stream closed without is_complete"` when the stream ends without a terminal `is_complete` frame. Only the **absence** of this warning is tested (happy path). Never the presence.

Triggering dirty-close requires the scheduler/store to disconnect mid-sweep. Options:
- **Unit test** in `rio-cli/tests/` with a mock stream that yields one progress frame then `Ok(None)` (EOF) without `is_complete=true`
- **VM test** that kills the scheduler mid-`rio-cli gc` — harder to orchestrate reliably

Prefer the unit test:
```rust
// r[verify sched.admin.gc-stream-complete]  -- IF this marker exists; check at dispatch
#[tokio::test]
async fn gc_stream_dirty_close_warns() {
    // Mock GetGcProgress stream: yield one GcProgress{is_complete:false},
    // then Ok(None). Capture stderr. Assert contains "closed without
    // is_complete".
}
```

### T4 — `test(scheduler):` failure_count recovery-init

MODIFY `rio-scheduler/src/state/derivation.rs` tests or `rio-scheduler/tests/`. [`derivation.rs:366`](../../rio-scheduler/src/state/derivation.rs) `failure_count: row.failed_workers.len() as u32` in `from_recovery_row` is documented (comment at `:364`: "failure_count: initialize from failed_workers.len()") but no test asserts the recovered value matches `failed_workers.len()`:

```rust
// r[verify sched.poison.ttl-persist]
// Recovery loads failure_count from PG failed_workers array length.
// If this breaks, a derivation that failed twice before restart gets
// failure_count=0 after recovery → poison threshold effectively resets.
#[tokio::test]
async fn recovery_row_initializes_failure_count_from_failed_workers() {
    let row = /* recovery row fixture with failed_workers = vec![w1, w2] */;
    let state = DerivationState::from_recovery_row(row, /* ... */);
    assert_eq!(state.failure_count, 2);
}
```

Also cover `from_poisoned_row` at [`derivation.rs:434`](../../rio-scheduler/src/state/derivation.rs) — same `failed_workers.len() as u32` pattern.

### T5 — `docs:` P0276 r[dash.*] annotation guidance

MODIFY [`.claude/work/plan-0276-getbuildgraph-rpc-pg-backed.md`](plan-0276-getbuildgraph-rpc-pg-backed.md) — add a line to its `## Tracey` section:

The marker `r[dash.graph.degrade-threshold]` at [`dashboard.md:34`](../../docs/src/components/dashboard.md) bundles client AND server: "Graph rendering MUST degrade... The server separately caps responses at 5000 nodes (`GetBuildGraphResponse.truncated`)." P0276 implements the **server half** (the 5000-node cap). It should carry `// r[impl dash.graph.degrade-threshold]` on the `LIMIT 5000` query. Currently P0276's `## Tracey` section has zero `r[dash.*]` refs.

One-line append to P0276's Tracey section:
```markdown
- `r[dash.graph.degrade-threshold]` — server-half (5000-node cap + `truncated` flag). Client-half (2000-node table fallback, 500-node Worker) is P0280.
```

### T6 — `test(controller):` cel_rules_in_schema +2 seccomp asserts

MODIFY [`rio-controller/src/crds/workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — P0223 line refs (p223 worktree `:443-459`; re-grep post-P0223-merge).

`cel_rules_in_schema` at p223:`:443` asserts three CEL rules land in the generated schema. Comment at `:446` says "The three `#[x_kube(validation)]` rules". **There are now five** — P0223 added two seccomp rules at p223:`:291-292` (`type in [...]` and the Localhost-coupling ternary). The test is the **exact silent-drop guard** its docstring describes: "a `#[x_kube(validation)]` attribute silently dropped from the schema means the apiserver accepts invalid specs." The two new rules are unguarded.

The sibling `camel_case_renames` test at p223:`:469` **was** updated (+2 asserts for `seccompProfile` / `localhostProfile` at `:480+`). This test was missed.

```rust
// At p223 :446, fix the comment:
// The five #[x_kube(validation)] rules, verbatim.

// After the existing 3 asserts (p223 :447-458), add:
// r[verify worker.seccomp.localhost-profile]
// P0223 seccomp: type-in-set + localhost-coupling rules.
// CEL rule text must appear verbatim in the generated schema JSON —
// if kube-derive changes its x_kube processing and drops these,
// the apiserver accepts {type: "bogus"} or {type: "Localhost"}
// with no profile path.
assert!(
    json.contains("self.type in ['RuntimeDefault', 'Localhost', 'Unconfined']"),
    "seccomp type-in-set CEL rule missing from schema"
);
assert!(
    json.contains("self.type == 'Localhost' ? has(self.localhostProfile) : !has(self.localhostProfile)"),
    "seccomp localhost-coupling CEL rule missing from schema"
);
```

**Strongest test-gap in this batch** — this test exists *specifically* to catch exactly this failure mode, and it's blind to the two rules it was meant to guard.

**Interaction with [P0304](plan-0304-trivial-batch-p0222-harness.md) T11:** If T11 (CEL `.message()`) changes the rule text (e.g., wraps in `Rule::new(...)`), the verbatim `json.contains(...)` asserts here may need to match whatever kube-derive emits. Check at dispatch — T11 changes the attr SYNTAX, not necessarily the schema OUTPUT. If the schema output is unchanged (message goes in a separate JSON field), these asserts are stable. If P0304 lands first, read the regenerated CRD YAML to confirm the rule strings.

### T7 — `test(controller):` Unconfined arm in build_seccomp_profile

MODIFY [`rio-controller/src/reconcilers/workerpool/builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) — P0223 line refs (p223 worktree `:702-722`; re-grep post-P0223-merge).

`build_seccomp_profile` at p223:`:702` has four match arms. P0223's tests cover `None`→`RuntimeDefault`, `Some("Localhost")`, and the privileged-drops path. `Some("Unconfined")` at `:711-714` is **untested**. The doc-comment at `:691` says "debugging-only" and at `:697-700` "defensive... fail-closed" — the code exists to AVOID falling through to Unconfined on a typo, but the arm that intentionally RETURNS Unconfined is a shipped, reachable branch with zero coverage.

```rust
// r[verify worker.seccomp.localhost-profile]
// Unconfined is debugging-only per the spec (security.md:56 "never
// production"). The arm is trivial (sets type_, nothing else) but
// it IS a shipped match arm — cover it so refactors don't silently
// merge it into the wildcard.
#[test]
fn build_seccomp_profile_unconfined() {
    let kind = SeccompProfileKind {
        type_: "Unconfined".into(),
        localhost_profile: None,
    };
    let profile = build_seccomp_profile(Some(&kind));
    assert_eq!(profile.type_, "Unconfined");
    assert_eq!(profile.localhost_profile, None);
}
```

~10 lines. Placement: alongside the existing seccomp builder tests (find them via `grep 'fn.*seccomp.*test\|build_seccomp_profile' builders.rs` at dispatch).

## Exit criteria

- `/nbr .#ci` green
- T1: `cli builds --json` on populated state → JSON with ≥1 build, `build_id` + `status` fields present
- T2: `cli workers --status alive` → all returned workers have `status=="alive"`; `--status draining` → empty when none draining
- T3: mock-stream dirty-close unit test → stderr contains "closed without is_complete"
- T4: `recovery_row_initializes_failure_count_from_failed_workers` passes; asserts `failure_count == failed_workers.len()`
- T5: `grep 'dash.graph.degrade-threshold' .claude/work/plan-0276*.md` → ≥1 hit
- `nix develop -c tracey query rule sched.poison.ttl-persist` shows a `verify` site (T4)
- `nix develop -c tracey query rule sched.admin.list-workers` — check if T2 should add a `verify` annotation (the status_filter is spec'd)
- `grep -c 'self.type in\|has(self.localhostProfile)' rio-controller/src/crds/workerpool.rs` ≥ 4 (T6: 2 attrs + 2 asserts; post-P0223-merge)
- `grep 'five.*x_kube\|5.*x_kube' rio-controller/src/crds/workerpool.rs` → ≥1 hit (T6: comment updated from "three" to "five")
- `cargo nextest run -p rio-controller build_seccomp_profile_unconfined` → passes (T7)
- `nix develop -c tracey query rule worker.seccomp.localhost-profile` shows ≥2 verify sites (T6+T7)

## Tracey

References existing markers:
- `r[sched.admin.list-workers]` — T2 verifies (status_filter behavior at [`scheduler.md:129`](../../docs/src/components/scheduler.md))
- `r[sched.poison.ttl-persist]` — T4 verifies (recovery restores poison state including failure_count at [`scheduler.md:108`](../../docs/src/components/scheduler.md))
- `r[dash.graph.degrade-threshold]` — T5 annotation guidance (P0276 implements server-half)

- `r[worker.seccomp.localhost-profile]` — T6 verifies (CEL rules guard the Localhost-coupling spec'd at [`security.md:55`](../../docs/src/security.md)), T7 verifies (Unconfined arm coverage)

No new markers. T1/T3 test cli output formatting and stream-handling — no corresponding spec markers exist (cli output format is not spec'd).

## Files

```json files
[
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T1+T2: populated builds/workers assertions (OR lifecycle.nix if MockAdmin can't populate)"},
  {"path": "rio-cli/tests/smoke.rs", "action": "MODIFY", "note": "T3: gc dirty-close warning unit test (preferred over VM — reliable mock-stream)"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T4: failure_count recovery-init test (or in rio-scheduler/tests/)"},
  {"path": ".claude/work/plan-0276-getbuildgraph-rpc-pg-backed.md", "action": "MODIFY", "note": "T5: add r[dash.graph.degrade-threshold] server-half line to Tracey section"},
  {"path": "rio-controller/src/crds/workerpool.rs", "action": "MODIFY", "note": "T6: cel_rules_in_schema +2 seccomp asserts + comment three→five (p223 :443-459) — post-P0223-merge"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "T7: build_seccomp_profile_unconfined test (~10 lines, near p223 :702) — post-P0223-merge"}
]
```

```
nix/tests/scenarios/cli.nix       # T1+T2: populated-state assertions
rio-cli/tests/smoke.rs            # T3: dirty-close unit test
rio-scheduler/src/state/
└── derivation.rs                 # T4: recovery-init test
.claude/work/plan-0276*.md        # T5: Tracey annotation guidance
rio-controller/src/
├── crds/workerpool.rs            # T6: cel_rules_in_schema +2 asserts
└── reconcilers/workerpool/
    └── builders.rs               # T7: Unconfined arm test
```

## Dependencies

```json deps
{"deps": [216, 219, 223], "soft_deps": [276, 304], "note": "Fresh test-gap batch (no open one existed). T1-T3 from P0216 review (cli code exists, tests assert empty-case only). T4 from P0219 (failure_count recovery documented @ derivation.rs:364 but untested). T5 is P0276 annotation guidance (marker bundles client+server; P0276 = server-half). T6/T7 from P0223 review (seccomp CEL rules + Unconfined arm untested — T6 is strongest: test exists SPECIFICALLY to catch silent-drop, blind to 2 new rules). Soft-dep P0304: its T11 adds .message() to CEL attrs — if schema output changes, T6's verbatim json.contains() strings need re-check. Line refs are from plan worktrees — re-grep at dispatch."}
```

**Depends on:** [P0216](plan-0216-rio-cli-subcommands.md) — `print_build`/`BuildJson`/`--status`/dirty-close exist. [P0219](plan-0219-per-worker-failure-budget.md) merged (DONE).
**Soft-dep:** [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) — T5 edits its plan doc, so land before P0276 dispatches (so the implementer sees the Tracey guidance). If P0276 already dispatched, send the guidance via coordinator message instead.
**Conflicts with:** [`derivation.rs`](../../rio-scheduler/src/state/derivation.rs) also touched by [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) T1 (derive) — different sections. `cli.nix` low-traffic. [`workerpool.rs`](../../rio-controller/src/crds/workerpool.rs) — [P0304](plan-0304-trivial-batch-p0222-harness.md) T11 adds `.message()` to the derive attrs (struct top); T6 here adds asserts to `cel_rules_in_schema` (test fn bottom). Same file, non-overlapping sections. [`builders.rs`](../../rio-controller/src/reconcilers/workerpool/builders.rs) count=18 — T7 adds a test fn alongside existing seccomp tests, additive.
