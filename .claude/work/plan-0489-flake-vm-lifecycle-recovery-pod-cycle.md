# Plan 489: Fix vm-lifecycle-recovery-k3s pod-cycle assertion flake

`vm-lifecycle-recovery-k3s` fails intermittently at
[`nix/tests/scenarios/lifecycle.nix:2905`](../../nix/tests/scenarios/lifecycle.nix)
— 6 hits across mc83-mc90 backgrounded coverage runs. The assertion checks
`new_store != old_store` by pod *name* after `kubectl rollout restart` +
`rollout status --timeout=90s`. But `rollout status` completing means the new
ReplicaSet is Available — it does NOT guarantee the old terminating pod has
been deleted. `kubectl get pod ... .items[0]` can still return the old
(Terminating) pod, making the assertion see `old==new`.

Current sprint-1 is green (`v7yamw8lbks8ydkgv09gkss2fhd3xjiv`) so this is
timing-dependent, not a hard break. Under CI load the old-pod-termination
lag widens.

Fix options:
- **Option A (UID not name):** compare `.metadata.uid` instead of
  `.metadata.name`. Pod UID is unique per-instance; even if the name
  hasn't changed yet (StatefulSet semantics), the UID will differ.
  But rio-store is a Deployment, not StatefulSet — name DOES change.
  UID comparison is still more robust.
- **Option B (wait for old gone):** after `rollout status`, add
  `wait_until_succeeds("! k3s kubectl get pod $old_store 2>/dev/null")`
  before fetching `new_store`. Explicit wait for old-pod deletion.
- **Option C (filter Running):** add `--field-selector=status.phase=Running`
  to the `get pod` query so Terminating pods are excluded. Simplest.

**Option C is preferred** — one-line fix, directly addresses the race.
Option B as fallback if field-selector doesn't reliably exclude Terminating
(check: k3s kubectl semantics for `status.phase` during graceful termination).

## Entry criteria

- P0481 merged (ad-hoc rsb fix, no plan doc — commit [`76696d55`](https://github.com/search?q=76696d55&type=commits))

## Tasks

### T1 — `fix(vm-test):` lifecycle.nix — filter Running pods in cycle assertion

MODIFY [`nix/tests/scenarios/lifecycle.nix:2900-2907`](../../nix/tests/scenarios/lifecycle.nix).
Add `--field-selector=status.phase=Running` to the `kubectl get pod` call
that fetches `new_store`:

```python
new_store = kubectl(
    "get pod -l app.kubernetes.io/name=rio-store "
    "--field-selector=status.phase=Running "
    "-o jsonpath='{.items[0].metadata.name}'",
    ns="${nsStore}",
).strip()
```

If `--field-selector` doesn't reliably exclude Terminating under k3s (test
at dispatch), fall back to Option B: insert before `:2900`:

```python
k3s_server.wait_until_succeeds(
    f"! k3s kubectl -n ${{nsStore}} get pod {old_store} 2>/dev/null",
    timeout=60,
)
```

### T2 — `fix(flake-registry):` add known-flake entry for vm-lifecycle-recovery-k3s

This plan's implementation adds the entry via `onibus flake add` in the
worktree and commits it alongside T1. See `## Known-flake entry` below for
the JSON to add.

## Exit criteria

- `/nixbuild .#checks.x86_64-linux.vm-lifecycle-recovery-k3s` green 3× consecutive
- `grep 'field-selector=status.phase=Running' nix/tests/scenarios/lifecycle.nix` → ≥1 hit (or Option-B `wait_until_succeeds` equivalent)
- `grep 'vm-lifecycle-recovery-k3s' .claude/known-flakes.jsonl` → ≥1 hit with `fix_owner` = this plan's final number

## Tracey

References existing markers:
- `r[sched.store-client.reconnect]` — the test at [`lifecycle.nix:2911-2924`](../../nix/tests/scenarios/lifecycle.nix) verifies this; T1 deflakes its precondition (pod-actually-cycled assertion)

No new markers — this is test-harness robustness, not new spec behavior.

## Files

```json files
[
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T1: field-selector or wait-for-old-gone at :2900"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T2: add vm-lifecycle-recovery-k3s entry"}
]
```

```
nix/tests/scenarios/lifecycle.nix  # T1: deflake pod-cycle assertion
.claude/known-flakes.jsonl         # T2: flake registry entry
```

## Known-flake entry

```json
{"test":"vm-lifecycle-recovery-k3s","symptom":"AssertionError: store pod should have cycled: old=X new=X (same pod name)","root_cause":"timing — rollout status completes before old pod terminates; get pod .items[0] returns Terminating old pod","fix_owner":"P489","fix_description":"add --field-selector=status.phase=Running to get-pod query (or wait for old pod deletion)","retry":"Once"}
```

## Dependencies

```json deps
{"deps": [481], "soft_deps": [311], "note": "P0481 is ad-hoc (no plan doc, dag row only). Soft-dep P0311-T93 (touches lifecycle.nix TAIL-append for FetcherPool — T93 at tail, this at :2900, non-overlapping). lifecycle.nix count=23 HOT per P0311 files-fence. discovered_from=coverage-sink."}
```

**Depends on:** P0481 (ad-hoc rsb fix, dag-only) — the `connect_lazy` reconnect test this deflakes.
**Conflicts with:** [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) count=23 HOT — P0311-T93 tail-appends FetcherPool subtest; this edits `:2900` mid-file. Non-overlapping hunks.
