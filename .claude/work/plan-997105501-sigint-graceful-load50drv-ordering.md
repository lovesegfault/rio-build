# Plan 997105501: sigint-graceful↔load-50drv ordering race — 2-slot window after worker restart

Bughunter-mc119 correctness finding. [`nix/tests/default.nix:241-242`](../../nix/tests/default.nix) wires `sigint-graceful` BEFORE `load-50drv` in the `vm-scheduling-disrupt-standalone` subtests list. The [`sigint-graceful` fragment](../../nix/tests/scenarios/scheduling.nix) at `:1210-1217` restarts wsmall2 (`systemctl start` + `wait_for_unit` + FUSE remount wait) but does NOT wait for scheduler re-registration. The worker heartbeat is `HEARTBEAT_INTERVAL_SECS = 10` at [`rio-common/src/limits.rs:51`](../../rio-common/src/limits.rs); between restart and first heartbeat, the scheduler's `workers_active` gauge doesn't count wsmall2.

The [`load-50drv` fragment](../../nix/tests/scenarios/scheduling.nix) at `:1034` expects "4 slots (wsmall1:2 + wsmall2:2; wlarge idle)" for its 50-leaf fanout. If load-50drv dispatches inside the ~10s re-registration window, only 2 slots (wsmall1) are available → ~26 dispatch waves × `tickIntervalSecs=2` ≈ 100-150s instead of 13 waves ≈ 40-60s. Under the 900s `globalTimeout` at `:249` this may still fit — but it's a latent flake that will manifest once the KVM fleet is fixed and the test actually runs consistently (currently it's been fast-path-deferred via clause-4c).

**Two fix shapes:**
- **FIX-A (cheap, chosen):** Swap the two entries in [`default.nix:241-242`](../../nix/tests/default.nix). `load-50drv` runs with full 4 slots; `sigint-graceful` runs last. The fragment's final state (wsmall2 restarted, FUSE remounted) is acceptable as the terminal state — `:1213` comment says "subsequent fragments (none currently, but collectCoverage + future additions) see a consistent state." No subsequent fragment, so the re-registration wait becomes non-load-bearing.
- **FIX-B (robust, heavier):** Add a `sched_metric_wait` on `rio_scheduler_workers_active >= 3` at the end of `sigint-graceful` (same pattern as [`scheduling.nix:340`](../../nix/tests/scenarios/scheduling.nix)'s `sched_metric_wait`). Belt-and-suspenders but adds ~10-15s walltime to an already-long fragment.

FIX-A is chosen: 2-line swap, zero walltime cost, no new failure surface. FIX-B kept in the plan as a stretch goal — implementer should apply both if wsmall2's state might matter to a future fragment inserted after `sigint-graceful`.

## Entry criteria

- [P0289](plan-0289-port-specd-unlanded-test-trio.md) merged — **DONE** (`sigint-graceful` fragment exists at [`scheduling.nix:1092-1218`](../../nix/tests/scenarios/scheduling.nix); `load-50drv` exists at `:1028-1089`)

## Tasks

### T1 — `fix(nix):` swap sigint-graceful and load-50drv order in disrupt subtests

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix) at `:241-242`. Current:

```nix
          "sigint-graceful"
          "load-50drv"
```

Swap:

```nix
          "load-50drv"
          # sigint-graceful LAST: restarts wsmall2 (systemctl start) but
          # doesn't wait for scheduler re-registration (HEARTBEAT_INTERVAL
          # = 10s). If load-50drv ran AFTER it'd see 2 slots not 4 → ~2×
          # walltime. Placing sigint last makes the re-registration
          # window non-load-bearing (collectCoverage reads profraw from
          # host fs, doesn't need wsmall2 registered with scheduler).
          "sigint-graceful"
```

Also update the comment block at `:237-240` which currently says "sigint-graceful AFTER reassign: … Uses wsmall2 only — no cache-chain coupling." — preserve that rationale but add the ordering-vs-load-50drv note. And the budget comment at `:244-247` ("sizeclass ~30s + max-silent-time ~25s + cancel-timing ~40s + reassign ~60s + sigint ~35s + load-50drv ~60s ≈ 250s") — the sum is order-invariant so no change needed there, but the sequence listed should match the new order.

### T2 — `fix(nix):` (optional) sched_metric_wait for workers_active at end of sigint-graceful

MODIFY [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) after `:1217` (the FUSE remount wait). If a future fragment is inserted after `sigint-graceful`, the 2-slot window will recur. Add a re-registration wait:

```python
          # Wait for scheduler re-registration. Worker heartbeats every
          # HEARTBEAT_INTERVAL_SECS=10 (rio-common/limits.rs:51).
          # Without this, any fragment inserted after sigint-graceful
          # sees 2 slots (wsmall1 only) until wsmall2's first heartbeat.
          # Timeout 30s: 1 heartbeat interval + TCG slop.
          ${gatewayHost}.wait_until_succeeds(
              "curl -sf localhost:9091/metrics | "
              "grep '^rio_scheduler_workers_active ' | "
              "awk '{exit !($2 >= 3)}'",
              timeout=30,
          )
```

**Apply T2 iff** the implementer sees a clear future-fragment dependency OR the coordinator asks. T1 alone is sufficient for the current subtests list. T2 adds 10-15s walltime (one heartbeat cycle under TCG).

### T3 — `docs(nix):` plan-0289 erratum — source-doc conflict resolution backwards

[`plan-0289`](plan-0289-port-specd-unlanded-test-trio.md) at `:17` says "Source-doc conflict resolved: the 15-doc at `:370` says `sigint-graceful` slots into `scheduling.nix`. The retrospective says `lifecycle.nix`. We go with `lifecycle.nix`." But the fragment LANDED in `scheduling.nix` (at `:1092`), NOT `lifecycle.nix` — the 15-doc was RIGHT, the retrospective was WRONG, and the implementer followed the 15-doc. The plan doc's "we go with lifecycle.nix" claim is now stale. Add an `[IMPL NOTE]` erratum after `:17`:

```markdown
> **[IMPL NOTE — resolved backwards]:** fragment landed in `scheduling.nix`
> (at `:1092`), not `lifecycle.nix`. The 15-doc's rationale ("scheduling.nix
> runs rio-worker as systemd on real VMs — k3s pods are distroless, no
> shell, no systemctl") was correct and load-bearing. lifecycle.nix's k3s
> fixture can't deliver SIGINT to a worker PID. See `scheduling.nix:1121-1123`
> for the impl's explanation.
```

discovered_from=bughunter(mc119).

## Exit criteria

- `/nbr .#ci` green
- `grep -A1 '"reassign"' nix/tests/default.nix | head -2 | grep '"load-50drv"'` → 1 hit (T1: load-50drv now follows reassign, sigint-graceful last)
- `grep 'rio_scheduler_workers_active\|re-registration' nix/tests/default.nix` → ≥1 hit (T1: ordering-rationale comment present)
- T2 conditional: `grep 'workers_active.*>= 3\|re-registration' nix/tests/scenarios/scheduling.nix` → ≥1 hit iff T2 applied
- T3: `grep 'resolved backwards\|15-doc.*was correct' .claude/work/plan-0289-*.md` → ≥1 hit (erratum present)
- **Observable outcome:** `/nixbuild .#checks.x86_64-linux.vm-scheduling-disrupt-standalone` on a KVM-capable builder → `load-50drv` subtest completes in ≤90s wall (4-slot dispatch, ~13 waves). Not CI-gating (VM builder allocation still flaky); confirmatory if opportunity arises.

## Tracey

References existing markers:
- `r[worker.shutdown.sigint]` — [`worker.md:438`](../../docs/src/components/worker.md). The `sigint-graceful` fragment already has `# r[verify worker.shutdown.sigint]` at [`scheduling.nix:52`](../../nix/tests/scenarios/scheduling.nix). T1 here doesn't change the fragment's assertions — only its position in the subtests list. No annotation change.

No new markers. Fragment-ordering is a test-harness concern, not a spec'd behavior.

## Files

```json files
[
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T1: swap sigint-graceful↔load-50drv at :241-242; add re-registration ordering comment"},
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T2 (optional): sched_metric_wait workers_active>=3 after :1217 FUSE-remount wait"},
  {"path": ".claude/work/plan-0289-port-specd-unlanded-test-trio.md", "action": "MODIFY", "note": "T3: [IMPL NOTE] erratum after :17 — fragment landed in scheduling.nix per 15-doc, not lifecycle.nix"}
]
```

```
nix/tests/
├── default.nix                   # T1: 2-line swap + comment
└── scenarios/scheduling.nix      # T2 (optional): re-reg wait
.claude/work/plan-0289-*.md       # T3: erratum
```

## Dependencies

```json deps
{"deps": [289], "soft_deps": [240, 311], "note": "bughunter-mc119 correctness. P0289 (DONE) landed sigint-graceful at scheduling.nix:1092 and load-50drv at :1028 (via P0240 originally). discovered_from=bughunter(mc119). Soft-dep P0240 (DONE — load-50drv fragment origin). Soft-dep P0311-T13/T14 (reminder tasks to RUN vm-scheduling-disrupt-standalone on KVM — T1's reorder makes that run less flake-prone). default.nix is low-traffic for subtests-list edits (additive/reorder only); scheduling.nix count=13 but T2 is tail-append to a fragment. The observable-outcome criterion ('load-50drv ≤90s') is confirmatory-not-gating — same risk profile as P0311-T10/T13-T16. CLAUSE-4 TIER: T1 is nix/tests/-only default.nix edit (subtests LIST reorder + comment) → clause-4(a) behavioral-drv-identity candidate. BUT the reorder changes test-execution ORDER, so the drv BUILDS same, RUNS different — NOT behavioral-identity. Proof-tier = `.#checks.x86_64-linux.vm-scheduling-disrupt-standalone` or nextest-standalone rc=0 (whichever the implementer can reach)."}
```

**Depends on:** [P0289](plan-0289-port-specd-unlanded-test-trio.md) — `sigint-graceful` fragment at `:1092-1218` exists; `load-50drv` at `:1028-1089` exists.
**Conflicts with:** [`default.nix`](../../nix/tests/default.nix) subtests list — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T27 adds `conn_cap` to security.nix subtests (different test attr, non-overlapping). [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) count=13 — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T11 tail-appends per-build-timeout subtest, [P0329](plan-0329-build-timeout-reachability-wopSetOptions.md)-T1 PROBE subtest — both TAIL-append; T2 here is tail-of-sigint-graceful-fragment (~:1217), non-overlapping.
