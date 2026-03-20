# Plan 0377: DrainButton revert-on-error uses pre-await idx — stale-index race

[P0281](plan-0281-dashboard-management-actions.md) post-PASS review (rev-p281). [`DrainButton.svelte:46`](../../rio-dashboard/src/components/DrainButton.svelte) reverts optimistic state on error via `workers[idx].status = prev` where `idx` was captured at [`DrainButton.svelte:34`](../../rio-dashboard/src/components/DrainButton.svelte) — **before** the `await admin.drainWorker(...)` at `:43`. The parent [`Workers.svelte:27`](../../rio-dashboard/src/pages/Workers.svelte) reassigns `workers = resp.workers` every 5s from a `setInterval(refresh, 5000)` at `:37`. If a poll completes between `findIndex` and `catch`, `idx` indexes into a fresh array: best case it reverts the WRONG row (worker registered/deregistered → positions shifted), worst case `workers[idx]` is `undefined` → `TypeError: Cannot set properties of undefined (setting 'status')`.

The optimistic **set** at `:40` has the same pre-await race but the window is microseconds (sync code between `:34` findIndex and `:40` assignment) — harmless. The **revert** window is the full RPC round-trip (100ms–seconds on a slow drain) — one 5s poll tick easily lands inside it.

**Fix:** key all mutations on `workerId`, not array index. The component already receives `workerId` as a prop and the `$derived` at `:23` already does `workers.find(w => w.workerId === workerId)` — same lookup, post-await.

## Entry criteria

- [P0281](plan-0281-dashboard-management-actions.md) merged ([`DrainButton.svelte`](../../rio-dashboard/src/components/DrainButton.svelte) exists with the `$bindable()` optimistic-update pattern)

## Tasks

### T1 — `fix(dashboard):` DrainButton revert — re-findIndex post-await

MODIFY [`rio-dashboard/src/components/DrainButton.svelte`](../../rio-dashboard/src/components/DrainButton.svelte) at `drain()` (`:28-51`). Replace index-captured-before-await with post-await `findIndex` in both the optimistic set AND the revert:

```svelte
async function drain() {
  if (!confirm(`Drain ${workerId}?`)) return;

  // Capture `prev` by value — it's a string, safe to hold across the await.
  const found = workers.find((w) => w.workerId === workerId);
  if (!found) return;
  const prev = found.status;

  // Optimistic set: findIndex here is sync (no race window).
  const setStatus = (s: string) => {
    const i = workers.findIndex((w) => w.workerId === workerId);
    if (i >= 0) workers[i].status = s;
  };
  setStatus('draining');
  busy = true;
  try {
    await admin.drainWorker({ workerId, force: false });
    toast.info(`draining ${workerId}`);
  } catch (e) {
    // Re-find post-await: `workers` may have been reassigned by the
    // parent's 5s refresh(). The pre-await idx would index into a
    // fresh array — wrong row or undefined.
    setStatus(prev);
    toast.error(`drain ${workerId} failed: ${e}`);
  } finally {
    busy = false;
  }
}
```

The `setStatus` helper encapsulates the re-find so optimistic-set and revert are symmetric. If the post-await `findIndex` returns `-1` (worker was removed by the scheduler between poll ticks), the revert is a no-op — correct behavior, the row is gone anyway.

### T2 — `test(dashboard):` poll-reassign-during-await → revert hits right row

MODIFY [`rio-dashboard/src/components/__tests__/DrainButton.test.ts`](../../rio-dashboard/src/components/__tests__/DrainButton.test.ts). Add a test that reassigns the bound `workers` array mid-RPC (simulating a refresh tick landing during the drain await), then asserts the revert lands on the row matching `workerId`, not the stale index:

```ts
it('revert-on-error keys on workerId, not pre-await index', async () => {
  // 3 workers, target is at index 1
  const workers = $state<WorkerInfo[]>([
    mkWorker('w-a', 'alive'),
    mkWorker('w-target', 'alive'),
    mkWorker('w-c', 'alive'),
  ]);
  // Drain RPC hangs until we resolve/reject it manually
  let rejectDrain!: (e: Error) => void;
  vi.mocked(admin.drainWorker).mockImplementation(
    () => new Promise((_, rej) => { rejectDrain = rej; }),
  );
  vi.stubGlobal('confirm', () => true);

  const { getByTestId } = render(DrainHarness, {
    props: { workerId: 'w-target', get workers() { return workers; } },
  });
  await fireEvent.click(getByTestId('drain-btn'));
  expect(workers[1].status).toBe('draining'); // optimistic set

  // Simulate parent's refresh(): w-a deregistered, array reassigned.
  // w-target is now at index 0 (stale idx=1 would hit w-c).
  workers.splice(0, workers.length,
    mkWorker('w-target', 'draining'),
    mkWorker('w-c', 'alive'),
  );

  rejectDrain(new Error('scheduler 503'));
  await tick();

  // Revert lands on w-target (idx=0 NOW), not w-c (stale idx=1).
  expect(workers[0].workerId).toBe('w-target');
  expect(workers[0].status).toBe('alive'); // reverted
  expect(workers[1].status).toBe('alive'); // w-c untouched
});

it('revert is a no-op when worker removed mid-await', async () => {
  const workers = $state<WorkerInfo[]>([mkWorker('w-gone', 'alive')]);
  let rejectDrain!: (e: Error) => void;
  vi.mocked(admin.drainWorker).mockImplementation(
    () => new Promise((_, rej) => { rejectDrain = rej; }),
  );
  vi.stubGlobal('confirm', () => true);

  const { getByTestId } = render(DrainHarness, {
    props: { workerId: 'w-gone', get workers() { return workers; } },
  });
  await fireEvent.click(getByTestId('drain-btn'));

  workers.splice(0, workers.length); // scheduler swept it
  rejectDrain(new Error('already dead'));
  await tick();
  // No TypeError thrown; toast.error still fires.
  expect(vi.mocked(toast.error)).toHaveBeenCalled();
});
```

**Check at dispatch:** [`DrainHarness.svelte`](../../rio-dashboard/src/components/__tests__/DrainHarness.svelte) may not expose `workers` as a reactive getter prop — adjust the harness or use `bind:` semantics as the existing tests do. The essential assertion is the post-await re-find; harness plumbing is implementer's discretion.

## Exit criteria

- `/nbr .#ci` green
- `nix develop -c pnpm --filter rio-dashboard test -- DrainButton` → all pass including the 2 new tests
- `grep 'workers\[idx\]' rio-dashboard/src/components/DrainButton.svelte` → 0 hits (T1: index-variable removed)
- `grep 'findIndex.*workerId === workerId' rio-dashboard/src/components/DrainButton.svelte` → ≥1 hit inside a post-await context (T1: re-find on revert)
- T2 regression proof: revert T1 (apply the pre-await `idx` capture), run `pnpm test -- DrainButton`, confirm the "revert-on-error keys on workerId" test FAILS with `expect(workers[1].status).toBe('alive')` receiving `'draining'` (wrong row reverted)

## Tracey

No new markers. Write-action optimistic-UI correctness — not spec'd. The `r[dash.*]` markers at [`dashboard.md`](../../docs/src/components/dashboard.md) cover read-path UX contracts (journey, streaming, graph-degrade); there is no `r[dash.drain.*]` and this plan doesn't introduce new behavior to spec.

## Files

```json files
[
  {"path": "rio-dashboard/src/components/DrainButton.svelte", "action": "MODIFY", "note": "T1: :34-46 idx-captured-before-await → setStatus(id-keyed) re-find post-await"},
  {"path": "rio-dashboard/src/components/__tests__/DrainButton.test.ts", "action": "MODIFY", "note": "T2: +2 tests — revert-keys-on-workerId + revert-noop-when-removed-mid-await"}
]
```

```
rio-dashboard/src/components/
├── DrainButton.svelte                # T1: re-find post-await
└── __tests__/DrainButton.test.ts     # T2: stale-index race regression
```

## Dependencies

```json deps
{"deps": [281], "soft_deps": [], "note": "discovered_from=281-review. P0281 UNIMPL at planning time (rev-p281 is post-PASS advisory, pre-merge) — the worktree /root/src/rio-build/p281 has the code at DrainButton.svelte:34-46. If P0281 merges before this dispatches, the line numbers hold (DrainButton.svelte is new in P0281, no other plan touches it). T2's DrainHarness.svelte also arrives with P0281 — adjust harness as needed for the workers-reassign scenario."}
```

**Depends on:** [P0281](plan-0281-dashboard-management-actions.md) — `DrainButton.svelte` + `DrainHarness.svelte` + `Workers.svelte` 5s poll all arrive there.
**Conflicts with:** [`DrainButton.svelte`](../../rio-dashboard/src/components/DrainButton.svelte) — P0281 creates it; this plan modifies. Serial after P0281 (dep enforces). No other UNIMPL plan touches `DrainButton.svelte`. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T122 (DrainButton-flicker) also modifies this file — that's a `busy`-state UI polish; T1 here rewrites the `drain()` body. Sequence this plan FIRST (correctness > polish); T122 then rebases onto the fixed `drain()` shape.
