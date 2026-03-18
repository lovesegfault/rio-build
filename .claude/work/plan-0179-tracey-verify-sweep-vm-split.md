# Plan 0179: Tracey r[verify] sweep + VM monolith split → 9 parallel fragments

## Design

Two related efforts: close tracey `r[verify]` gaps (8 rules untested) and split the VM monoliths for parallelism.

**Tracey sweep (`3330dd9`):** 8 rules had `r[impl]` but no `r[verify]`; added VM test annotations. Found 5 FALSE-PASS assertions in the process — tests that asserted a condition that was trivially true. Each fix is a test that now actually tests something.

**VM subtests (`0dbc383`, `baa0d1f`, `1929c5d`, `9362215`):** cancel-cgroup-kill, build-crd-errors, graceful-release, controller restart-mid-build, FUSE fault-inject. Each adds a subtest to a scenario file with a `# r[verify ...]` annotation.

**Col-0 header move (`647bb90`):** tracey's `.nix` parser only sees col-0 comments before `{`; indented `# r[verify ...]` inside `testScript` string literals are doc-only. Moved 3 markers to col-0.

**Monolith split (`9937257`):** `lifecycle.nix` + `scheduling.nix` + `leader-election.nix` now return `{fragments, mkTest}` instead of a single `runNixOSTest`. `default.nix` composes fragments into 9 parallel tests — critical path ~14min→~8min (`lifecycle-autoscale`: autoscaler+finalizer chain preserved for reverse-ordinal termination coverage). `gc-sweep` decoupled from recovery (builds its own `gcVictimDrv`). Chain assertions (eval-time) guard fragment ordering. Imports moved fragment-local.

**Supporting (`b42a0af`, `060096e`, `d874133`, `0dd97fd`, `96763b0`, `7c437e1`):** k3s bootstrap flake gate on rio-store + server-node-exists; remaining flake fixes.

## Files

```json files
[
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "cancel-cgroup-kill + build-crd-errors subtests; {fragments, mkTest} split"},
  {"path": "nix/tests/scenarios/leader-election.nix", "action": "MODIFY", "note": "graceful-release subtest; split"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "FUSE fault-inject; split; chain assertions"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "compose fragments → 9 parallel tests"},
  {"path": "rio-worker/src/fuse/ops.rs", "action": "MODIFY", "note": "fault-inject cache-vs-disk divergence for open/readlink/readdir slow-paths"},
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "gate on rio-store + server-node-exists (56× speedup)"},
  {"path": "docs/src/coverage-notes.md", "action": "NEW", "note": "explain unmeasurable paths: pre_exec, FUSE destroy/forget"}
]
```

## Tracey

- `# r[verify ctrl.build.sentinel]` ×2 — `3330dd9`
- `# r[verify ctrl.probe.named-service]` ×2 — `3330dd9`
- `# r[verify ctrl.autoscale.skip-deleting]` ×2 — `3330dd9`
- `# r[verify obs.metric.gateway]` — `3330dd9`
- `# r[verify worker.fuse.lookup-caches]` ×2 — `3330dd9`
- `# r[verify store.inline.threshold]` ×2 — `3330dd9`
- `# r[verify obs.metric.transfer-volume]` ×2 — `3330dd9`
- `// r[verify sched.classify.mem-bump]` — `3330dd9`
- `# r[verify worker.cancel.cgroup-kill]` — `0dbc383` → col-0 move in `647bb90`
- `# r[verify sched.lease.graceful-release]` — `baa0d1f` → col-0 move in `647bb90`
- `# r[verify sched.lease.deletion-cost]` — `baa0d1f` → col-0 move in `647bb90`

17 marker annotations (14 new in `3330dd9`, 3 new in subtest commits, col-0 moves don't add).

## Entry

- Depends on P0176: k3s bootstrap stabilization
- Depends on P0177: tonic reconnect class (the subtests exercise reconnect paths)

## Exit

Merged as `3330dd9`, `1929c5d`, `9362215`, `0dbc383`, `baa0d1f`, `0e9e7bc`, `b42a0af`, `060096e`, `647bb90`, `d874133`, `0dd97fd`, `96763b0`, `7c437e1`, `9937257` (14 commits). Critical path ~14min→~8min. tracey: 8 previously-untested rules now verified.
