# Plan 995943542: Graph page corrections — STATUS_CLASS Skipped + Worker message race

Two rev-p280 correctness findings on the DAG visualization page. [P0280](plan-0280-dashboard-dag-viz-xyflow.md) forked before [P0252](plan-0252-ca-cutoff-propagate-skipped.md) landed `DerivationStatus::Skipped`; the `STATUS_CLASS`/`SORT_RANK` maps at [`graphLayout.ts:43-54,72-83`](../../rio-dashboard/src/lib/graphLayout.ts) don't have a `skipped` arm. Separately, the WebWorker message correlation has a race: overlapping polls can resolve the N+1 promise with the Nth response.

Third finding (rev-p280 trivial, poll-after-terminal) is folded in as T3 — it's the same `Graph.svelte` edit region as T2's inflight gate.

## Entry criteria

- [P0280](plan-0280-dashboard-dag-viz-xyflow.md) merged (`graphLayout.ts`, `Graph.svelte`, `graphLayout.worker.ts` exist)
- [P0252](plan-0252-ca-cutoff-propagate-skipped.md) merged (`skipped` status reaches the dashboard via `GetBuildGraph`) — **DONE**

## Tasks

### T1 — `fix(dashboard):` STATUS_CLASS + SORT_RANK — add skipped arm

MODIFY [`rio-dashboard/src/lib/graphLayout.ts`](../../rio-dashboard/src/lib/graphLayout.ts) at `:43-54` (`STATUS_CLASS`) and `:72-83` (`SORT_RANK`). After P0280's rebase onto sprint-1, `GetBuildGraph` returns `skipped` but the maps fall through to default (gray, rank-5 bottom).

```typescript
const STATUS_CLASS: Record<string, string> = {
  completed: "status-complete",    // green
  skipped: "status-complete",      // ALSO green — output-equivalent
                                   // to completed (store has the
                                   // output; CA cutoff verified).
                                   // Distinct icon/badge optional.
  running: "status-running",       // yellow pulse
  // ... failed/poisoned → red, queued/ready/assigned → gray ...
};

const SORT_RANK: Record<string, number> = {
  failed: 0, poisoned: 0, dependency_failed: 0,  // top — needs attention
  running: 1, assigned: 1,
  ready: 2, queued: 2,
  completed: 3,
  skipped: 3,    // same rank as completed — both mean "output
                 // available". merge.rs cached-count semantics treat
                 // Skipped as Completed-equivalent.
  cancelled: 4,
  // ... unknown: 5 (default) ...
};
```

MODIFY [`rio-dashboard/src/lib/__tests__/graphLayout.test.ts`](../../rio-dashboard/src/lib/__tests__/graphLayout.test.ts) at `:112-128` — the status-mapping test pins the pre-P0252 set. Add `skipped` to the expected-class map AND to the sort-rank assertion. The test SHOULD fail on sprint-1 without T1 (rebase introduces the new status via protobuf enum, map falls through).

### T2 — `fix(dashboard):` Worker message race — inflight gate

MODIFY [`rio-dashboard/src/pages/Graph.svelte`](../../rio-dashboard/src/pages/Graph.svelte) at `:89-110` (`layoutInWorker`) + `:186` (setInterval). The shared `Worker` instance + one-shot `addEventListener('message', ..., {once: true})` pattern means overlapping calls both install listeners; the FIRST response fires BOTH listeners. Promise N+1 resolves with response N's stale positions; response N+1 arrives to no listener.

Two fix routes:

- **(a) Inflight gate** (simpler): track `let inflight = false;` in the component. `fetchAndLayout` early-returns when `inflight`. Set before the `await admin.getBuildGraph`, clear after `layout = ...`. The 5s poll skips iterations while a slow request is in flight.
- **(b) Sequence-number correlation**: `WorkerRequest`/`WorkerResponse` gain a `seq: number` field. `layoutInWorker` increments a module-local counter, attaches it to the request, and the `onmessage` resolver checks `ev.data.seq === mySeq` before resolving (otherwise ignores — the next listener in line will pick it up, or it's dropped if no listener matches).

Prefer **(a)** — simpler, matches the existing polling pattern elsewhere in the dashboard (GC.svelte, Workers.svelte don't have re-entrancy guards because they don't spawn Workers, but the same shape applies). Option (b) is necessary only if concurrent layout requests are genuinely wanted (they aren't — the poll is a single logical stream).

```svelte
<script lang="ts">
  let inflight = false;

  $effect(() => {
    const id = setInterval(fetchAndLayout, 5000);
    fetchAndLayout();
    return () => clearInterval(id);
    async function fetchAndLayout() {
      if (inflight) return;   // T2: skip overlapping polls
      inflight = true;
      try {
        const r = await admin.getBuildGraph({ buildId });
        // ... layout = ... (T3's terminal-check goes here)
      } finally {
        inflight = false;
      }
    }
  });
</script>
```

### T3 — `refactor(dashboard):` stop polling once all nodes terminal

MODIFY [`rio-dashboard/src/pages/Graph.svelte`](../../rio-dashboard/src/pages/Graph.svelte) at `:186` (same edit region as T2). Once every node is terminal (`completed|skipped|poisoned|dependency_failed|cancelled|failed`), statuses never change — the 5s poll wastes QPS for drawers left open on finished builds.

```typescript
const TERMINAL = new Set([
  "completed", "skipped", "poisoned",
  "dependency_failed", "cancelled", "failed",
]);

// inside fetchAndLayout, after `const r = await ...`:
if (r.nodes.length > 0 && r.nodes.every(n => TERMINAL.has(n.status))) {
  clearInterval(id);  // terminal — no more status updates coming
}
```

`TERMINAL` set goes in `graphLayout.ts` alongside `STATUS_CLASS` (single source of truth for terminal-ness on the dashboard). Mirrors `DerivationStatus::is_terminal()` at [`derivation.rs`](../../rio-scheduler/src/state/derivation.rs).

### T4 — `test(dashboard):` STATUS_CLASS exhaustive + inflight gate

MODIFY [`rio-dashboard/src/lib/__tests__/graphLayout.test.ts`](../../rio-dashboard/src/lib/__tests__/graphLayout.test.ts) — extend T1's test with an exhaustiveness check: import the protobuf `DerivationStatus` enum, iterate all variants, assert each has a `STATUS_CLASS` entry. Catches future variant additions at test-time rather than runtime-gray-fallthrough. Same pattern as [`metrics_registered.rs`](../../rio-scheduler/tests/metrics_registered.rs) (enumerate-and-assert-membership).

NEW `rio-dashboard/src/pages/__tests__/Graph.test.ts` (or extend existing) — mock `admin.getBuildGraph` with a slow (100ms) resolve, fire two `fetchAndLayout` calls back-to-back via `vi.advanceTimersByTime(5000)` twice, assert `getBuildGraph` called ONCE (T2's gate skipped the second poll).

## Exit criteria

- `/nbr .#ci` green
- `grep "skipped.*status-complete\|skipped:.*3" rio-dashboard/src/lib/graphLayout.ts` → ≥2 hits (T1: both maps have the skipped arm)
- `grep "inflight" rio-dashboard/src/pages/Graph.svelte` → ≥3 hits (T2: declare + check + set/clear)
- `grep "TERMINAL\|clearInterval" rio-dashboard/src/lib/graphLayout.ts rio-dashboard/src/pages/Graph.svelte` → ≥3 hits (T3: set defined + used + interval cleared)
- T4 exhaustiveness test — `for (const s of Object.values(DerivationStatus))` every variant maps (proto-enum iteration may need `Object.keys(DerivationStatus).filter(isNaN)` or equivalent for connect-es GenService — check at dispatch)
- T4 inflight test — slow mock + double-advance → `expect(adminMock.getBuildGraph).toHaveBeenCalledTimes(1)`

## Tracey

References existing markers:
- `r[dash.graph.degrade-threshold]` — T1+T2+T3 refine the page carrying this marker's `r[impl]`; no annotation changes (thresholds unchanged).
- `r[dash.journey.build-to-logs]` — T1 indirectly serves (node-click → correct color affordance; gray-for-skipped was misleading).

No new markers — Skipped status rendering + poll-race are dashboard implementation details, not spec'd behavior.

## Files

```json files
[
  {"path": "rio-dashboard/src/lib/graphLayout.ts", "action": "MODIFY", "note": "T1: +skipped arm to STATUS_CLASS:43-54 + SORT_RANK:72-83. T3: export TERMINAL set"},
  {"path": "rio-dashboard/src/pages/Graph.svelte", "action": "MODIFY", "note": "T2: inflight gate :89-110,:186. T3: clearInterval once all-terminal :186"},
  {"path": "rio-dashboard/src/lib/__tests__/graphLayout.test.ts", "action": "MODIFY", "note": "T1: add skipped to status-map test :112-128. T4: proto-enum exhaustive check"},
  {"path": "rio-dashboard/src/pages/__tests__/Graph.test.ts", "action": "MODIFY", "note": "T4: inflight-gate test (slow-mock + double-poll → 1 call). NEW if file doesn't exist yet"}
]
```

```
rio-dashboard/src/
├── lib/
│   ├── graphLayout.ts                 # T1: skipped arms; T3: TERMINAL
│   └── __tests__/graphLayout.test.ts  # T1+T4: status-map tests
└── pages/
    ├── Graph.svelte                   # T2: inflight gate; T3: terminal-stop
    └── __tests__/Graph.test.ts        # T4: inflight test
```

## Dependencies

```json deps
{"deps": [280, 252], "soft_deps": [389, 276], "note": "rev-p280 correctness (P0280 forked pre-P0252 — STATUS_CLASS has no skipped arm, falls through to gray). Worker-race: overlapping 5s polls w/ slow network or heavy-graph dagre → stale positions. T3 folded in (same file+region). P0280 is UNIMPL — the p280 worktree (being reviewed) should rebase onto sprint-1 and absorb P0252's Skipped variant BEFORE merge; this plan then becomes a post-merge sweep. If P0280's implementer picks up T1 pre-merge (via STEER note on P0280's plan doc), this plan shrinks to T2+T3+T4 only."}
```

**Depends on:** [P0280](plan-0280-dashboard-dag-viz-xyflow.md) — `graphLayout.ts` + `Graph.svelte` exist. [P0252](plan-0252-ca-cutoff-propagate-skipped.md) — `skipped` status reaches protobuf (DONE).
**Soft-dep:** [P0389](plan-0389-dashboard-test-admin-mock-extraction.md) — T4's inflight test uses `adminMock.getBuildGraph`; P0304-T167 tracks adding an empty-default mock for it. If P0389 hasn't landed, T4 inlines the mock. [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md) — `DerivationStatus` proto enum used by T4's exhaustive check (transitive via P0280's dep).
**Conflicts with:** [`graphLayout.ts`](../../rio-dashboard/src/lib/graphLayout.ts) — P0304-T169 deletes the stale `sourcePosition` comment at `:118-122`; T1 here at `:43-54,:72-83` — non-overlapping. [`Graph.svelte`](../../rio-dashboard/src/pages/Graph.svelte) — T2+T3 at `:89-186` (poll effect). Low-collision file (dashboard pages are single-owner per-page).
