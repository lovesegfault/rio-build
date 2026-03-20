# Plan 991713302: Dashboard — cursor-chained paging at Builds.svelte

[P0271](plan-0271-cursor-pagination-admin-builds.md) shipped `next_cursor` in `ListBuildsResponse` ([`admin_types.proto:71`](../../rio-proto/proto/admin_types.proto) on p271) and documented the offset→cursor handoff at [`builds.rs:96-99`](../../rio-scheduler/src/admin/builds.rs) ("start with offset=0, switch to cursor-chained for page 2"). But [`Builds.svelte:55`](../../rio-dashboard/src/pages/Builds.svelte) still sends `offset: p * PAGE_SIZE` — the cursor API has zero consumers. rev-p271.

Until the dashboard migrates, P0271's keyset path is dead code. The offset instability (newly submitted builds shift pages mid-walk) that P0271 was written to fix is still live in the only client.

## Entry criteria

- [P0271](plan-0271-cursor-pagination-admin-builds.md) merged (`next_cursor` in `ListBuildsResponse` + keyset query path exist)
- [P0278](plan-0278-builds-page-drawer-drilldown.md) merged (`Builds.svelte` `$effect` list-fetch at `:45-65` exists — post-P0278 shape)

## Tasks

### T1 — `feat(dashboard):` cursor-chain replaces offset arithmetic

MODIFY [`rio-dashboard/src/pages/Builds.svelte`](../../rio-dashboard/src/pages/Builds.svelte) at `:45-65`. Replace the `page: number` state + `offset: p * PAGE_SIZE` with a cursor stack:

```typescript
let cursors: (string | undefined)[] = $state([undefined]);  // cursors[0] = first page
let pageIdx = $state(0);

$effect(() => {
  const sf = statusFilter;
  const cur = cursors[pageIdx];
  (async () => {
    try {
      const r = await admin.listBuilds({
        statusFilter: sf,
        limit: PAGE_SIZE,
        offset: 0,               // ignored when cursor set; 0 for first page
        cursor: cur,             // undefined on first page → offset-mode page-1
        tenantFilter: '',
      });
      builds = r.builds;
      if (pageIdx === 0) total = r.totalCount;  // capture once — P991713301-T3 option-(a)
      // Stash next_cursor for "Next" button. Present iff page is full.
      if (r.nextCursor && cursors.length === pageIdx + 1) {
        cursors = [...cursors, r.nextCursor];
      }
      error = null;
    } catch (e) {
      error = String(e);
    }
  })();
});
```

"Previous" button: `pageIdx--` (cursor for that page is already in the stack). "Next": `pageIdx++` (enabled only when `cursors[pageIdx+1]` exists). Filter change resets: `cursors = [undefined]; pageIdx = 0;`.

This preserves the page-number UX (users see "page 3 of ~N") while using cursors under the hood. The `total` captured once on page 1 matches [P991713301](plan-991713301-keyset-index-float8-precision.md)-T3's first-page-only `count_builds`.

### T2 — `test(dashboard):` cursor chain — forward + back + filter-reset

MODIFY (or NEW) `rio-dashboard/src/pages/__tests__/Builds.test.ts`. Three assertions:

1. **Forward chain:** mock `listBuilds` to return `{builds: [100 items], nextCursor: "c1"}` on first call, `{builds: [100 items], nextCursor: "c2"}` on second (with `cursor: "c1"`). Click Next; assert second call received `cursor: "c1"`, NOT `offset: 100`.
2. **Back:** click Previous after two Nexts; assert the cursor sent is the stashed `c1` (from stack), not a fresh RPC or offset math.
3. **Filter reset:** change `statusFilter`; assert next RPC has `cursor: undefined` and the cursor stack was cleared.

Uses [`adminMock`](../../rio-dashboard/src/test-support/admin-mock.ts) from P0389. `adminMock.listBuilds.mockImplementation(...)` with call-count-dependent returns.

## Exit criteria

- `/nbr .#ci` green
- `grep 'p \* PAGE_SIZE\|page \* PAGE_SIZE' rio-dashboard/src/pages/Builds.svelte` → 0 hits (offset arithmetic removed)
- `grep 'nextCursor\|cursors\[' rio-dashboard/src/pages/Builds.svelte` → ≥2 hits (cursor stack present)
- `nix develop -c pnpm --filter rio-dashboard test -- Builds` → ≥3 passed (T2's three assertions)
- T2 load-bearing: the `cursor: "c1"` assert on second call proves cursor-chain (not offset fallback)

## Tracey

References existing markers:
- `r[sched.admin.list-builds]` — T1 is the first CLIENT consumer of keyset cursor mode; T2 verifies the chain

No new markers. Dashboard page-fetch wiring isn't spec'd separately from the RPC it calls.

## Files

```json files
[
  {"path": "rio-dashboard/src/pages/Builds.svelte", "action": "MODIFY", "note": "T1: :45-65 page→pageIdx + cursors stack; offset arithmetic → cursor chain"},
  {"path": "rio-dashboard/src/pages/__tests__/Builds.test.ts", "action": "MODIFY", "note": "T2: cursor-chain forward/back/filter-reset assertions (uses adminMock from P0389)"}
]
```

```
rio-dashboard/src/pages/
├── Builds.svelte              # T1: cursor stack replaces offset math
└── __tests__/Builds.test.ts   # T2: chain assertions
```

## Dependencies

```json deps
{"deps": [271, 278, 389], "soft_deps": [991713301, 377, 400], "note": "P0271 provides next_cursor field + keyset handler. P0278 provides Builds.svelte $effect shape. P0389 provides adminMock for T2. Soft-dep P991713301: its T3 option-(a) first-page-only total_count dovetails with T1's 'capture total once on page 1'. Soft-dep P0377 (Workers.svelte idx-race fix — same page-fetch race class; T1's cursor-stack approach may benefit from the same $effect dedup guard). Builds.svelte count~7 — P0295-T78 edits :36 comment (non-overlapping with T1's :45-65)."}
```

**Depends on:** [P0271](plan-0271-cursor-pagination-admin-builds.md) — `next_cursor` + keyset path. [P0278](plan-0278-builds-page-drawer-drilldown.md) — `Builds.svelte` shape. [P0389](plan-0389-dashboard-test-admin-mock-hoisted-extract.md) — `adminMock` helper.
**Conflicts with:** [`Builds.svelte`](../../rio-dashboard/src/pages/Builds.svelte) — P0295-T78 edits `:36` comment; T1 here edits `:45-65` `$effect`. Non-overlapping.
