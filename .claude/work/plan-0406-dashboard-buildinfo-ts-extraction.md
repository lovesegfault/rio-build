# Plan 0406: Dashboard buildInfo.ts extraction — progress + timestamp helpers

consolidator-mc210 finding at [`rio-dashboard/src/pages/Builds.svelte:94-118`](../../rio-dashboard/src/pages/Builds.svelte) and [`rio-dashboard/src/components/BuildDrawer.svelte:29-43`](../../rio-dashboard/src/components/BuildDrawer.svelte). The `progress(b: BuildInfo)` function is byte-identical in both files (4-line body: `totalDerivations==0` guard + `Math.min(100, round(done/total*100))`). The proto-Timestamp→ms conversion (`Number(ts.seconds)*1000`) appears 4× across both files — twice in `Builds.svelte` (`:102`, `:112-113`), once in `BuildDrawer.svelte` (`:41`). [`Workers.svelte:47-55`](../../rio-dashboard/src/pages/Workers.svelte) has a THIRD relative-time formatter with a different ladder (`s<60→"Xs ago"`).

Worth extracting now: [P0400](plan-0400-graph-page-skipped-worker-race.md) (Worker race fix) and [P0404](plan-0404-dashboard-cursor-chained-paging.md) (cursor paging) both touch `Builds.svelte`; [P0392](plan-0392-logstream-spread-virtualization.md) adjacent. Each would otherwise re-copy or leave the duplication. Also: `Workers.svelte:7` already imports `timestampMs` from `@bufbuild/protobuf/wkt` — that's the canonical converter; the hand-rolled `Number(ts.seconds)*1000` pattern predates it. The extraction should standardize on bufbuild's helper.

Adjacent to [P0390](plan-0390-dashboard-gen-pb-barrel.md) (`api/types.ts` barrel) — both are "one place to change" extractions. Could fold in as a late T-item on P0390, but separate is cleaner: P0390 touches import paths across 7 consumers (refactor-only, zero runtime change), this touches function bodies in 3 consumers (refactor + bufbuild-standardize, a runtime-semantics-preserving change that nonetheless CAN have observable effects if the hand-rolled conversion and bufbuild's differ at a boundary). Keeping them separate isolates blast radius.

**Supersedes [P0304](plan-0304-trivial-batch-p0222-harness.md)-T127** (buildProgress extraction to `lib/build.ts`). T127 scoped only the `progress()` dup; this plan is broader: `progress()` + 4 timestamp helpers + `Workers.svelte` migration + standardizing on `@bufbuild/protobuf/wkt`. Extraction target is `lib/buildInfo.ts` (not `lib/build.ts`). At dispatch: check if T127 already landed — if so, this plan's T1 extends the existing file instead of creating new.

## Entry criteria

- [P0278](plan-0278-build-drawer-detail-page.md) merged (`BuildDrawer.svelte` with `progress()` and `fmtTs()` exist) — **DONE**
- [P0280](plan-0280-dashboard-dag-viz-xyflow.md) merged (`Graph.svelte` exists — soft, Graph page itself doesn't use progress but [P0400](plan-0400-graph-page-skipped-worker-race.md) may want the helpers)

## Tasks

### T1 — `feat(dashboard):` lib/buildInfo.ts — extract progress + timestamp helpers

NEW [`rio-dashboard/src/lib/buildInfo.ts`](../../rio-dashboard/src/lib/buildInfo.ts):

```typescript
import { timestampMs, type Timestamp } from '@bufbuild/protobuf/wkt';
import type { BuildInfo } from '../api/types';  // or ../gen/admin_types_pb pre-P0390

/** completed + cached both count as "done" — cached derivations
 *  short-circuit at merge time (scheduler's row_to_proto: cached =
 *  "completed with no assignment row"). */
export function progress(b: BuildInfo): number {
  if (b.totalDerivations === 0) return 0;
  const done = b.completedDerivations + b.cachedDerivations;
  return Math.min(100, Math.round((done / b.totalDerivations) * 100));
}

/** proto Timestamp → ms since epoch. Returns undefined for absent timestamp. */
export function tsToMs(ts: Timestamp | undefined): number | undefined {
  return ts ? timestampMs(ts) : undefined;
}

/** Absolute ISO-8601 string, or "—" for absent timestamp. */
export function fmtTsAbs(ts: Timestamp | undefined): string {
  const ms = tsToMs(ts);
  return ms !== undefined ? new Date(ms).toISOString() : '—';
}

/** Relative "Xs ago" / "Xm ago" / "Xh ago" / "Xd ago", or "—" / "never". */
export function fmtTsRel(
  ts: Timestamp | undefined,
  now = Date.now(),
): string {
  const then = tsToMs(ts);
  if (then === undefined) return '—';
  const delta = now - then;
  if (delta < 1000) return 'now';
  const s = Math.floor(delta / 1000);
  if (s < 60) return `${s}s ago`;
  const m = Math.floor(s / 60);
  if (m < 60) return `${m}m ago`;
  const h = Math.floor(m / 60);
  if (h < 24) return `${h}h ago`;
  return `${Math.floor(h / 24)}d ago`;
}

/** Human-duration "15s" / "2m30s" / "1h12m", or "—" when startedAt absent. */
export function fmtDuration(b: BuildInfo, now = Date.now()): string {
  const start = tsToMs(b.startedAt);
  if (start === undefined) return '—';
  const end = tsToMs(b.finishedAt) ?? now;
  const s = Math.round((end - start) / 1000);
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m${s % 60}s`;
  return `${Math.floor(s / 3600)}h${Math.floor((s % 3600) / 60)}m`;
}
```

**CARE — `@bufbuild/protobuf/wkt` vs hand-roll:** `timestampMs(ts)` is `Number(ts.seconds) * 1000 + ts.nanos / 1e6` — it includes the nanos component. The existing `Builds.svelte:102,112,113` code drops nanos (`Number(ts.seconds) * 1000` only). `BuildDrawer.svelte:41` includes them (`+ Math.floor(ts.nanos / 1e6)`). Standardizing on bufbuild's version means `Builds.svelte` gains sub-second precision — harmless for relative-time display, but TEST the `fmtTsRel`/`fmtDuration` outputs don't differ at boundaries (e.g., `59.8s` rounds the same). If the test-assertion churn is annoying, the `Math.floor` wrapper can preserve the drop-nanos behavior.

**CARE — import path:** use `../api/types` if [P0390](plan-0390-dashboard-gen-pb-barrel.md) landed first (check at dispatch: `test -f rio-dashboard/src/api/types.ts`); otherwise `../gen/admin_types_pb` and P0390 migrates it.

### T2 — `refactor(dashboard):` migrate Builds.svelte, BuildDrawer.svelte, Workers.svelte

MODIFY [`rio-dashboard/src/pages/Builds.svelte`](../../rio-dashboard/src/pages/Builds.svelte) — delete `:94-118` local `progress`/`relTime`/`duration`, import from `'../lib/buildInfo'`:

```svelte
<script lang="ts">
  import { progress, fmtTsRel, fmtDuration } from '../lib/buildInfo';
  // ... existing imports
</script>
```

MODIFY [`rio-dashboard/src/components/BuildDrawer.svelte`](../../rio-dashboard/src/components/BuildDrawer.svelte) — delete `:29-43` local `progress`/`fmtTs`, import `progress, fmtTsAbs`.

MODIFY [`rio-dashboard/src/pages/Workers.svelte`](../../rio-dashboard/src/pages/Workers.svelte) — delete `:47-55` local `rel` formatter, import `fmtTsRel`. The `ageMs` helper at `:43-45` can either stay local (it takes a `WorkerInfo` not a timestamp, composes `timestampMs` differently) or become `tsToMs(w.lastHeartbeat)` inline at the call site. Prefer the latter: one less local fn.

**Three consumers, ~40 lines net-negative.** `progress()` test at [`rio-dashboard/src/components/__tests__/BuildDrawer.test.ts`](../../rio-dashboard/src/components/__tests__/BuildDrawer.test.ts) (if P0311-T44 landed it) should keep passing since the body is byte-identical — just moved.

### T3 — `test(dashboard):` buildInfo.ts unit tests

NEW [`rio-dashboard/src/lib/__tests__/buildInfo.test.ts`](../../rio-dashboard/src/lib/__tests__/buildInfo.test.ts). Table-driven:

```typescript
import { describe, it, expect } from 'vitest';
import { progress, tsToMs, fmtTsAbs, fmtTsRel, fmtDuration } from '../buildInfo';
import type { BuildInfo } from '../../api/types';

const ts = (s: number, n = 0) => ({ seconds: BigInt(s), nanos: n });
const bi = (p: Partial<BuildInfo>) => p as BuildInfo;

describe('progress', () => {
  it.each([
    [{ totalDerivations: 0, completedDerivations: 0, cachedDerivations: 0 }, 0],
    [{ totalDerivations: 10, completedDerivations: 5, cachedDerivations: 2 }, 70],
    [{ totalDerivations: 3, completedDerivations: 3, cachedDerivations: 0 }, 100],
    // cached + completed can exceed total during a race window — clamped
    [{ totalDerivations: 3, completedDerivations: 3, cachedDerivations: 2 }, 100],
  ])('%j → %d%%', (b, pct) => expect(progress(bi(b))).toBe(pct));
});

describe('fmtTsRel', () => {
  const now = 1_700_000_000_000;
  it.each([
    [undefined, '—'],
    [ts(1_700_000_000 - 5), '5s ago'],
    [ts(1_700_000_000 - 90), '1m ago'],
    [ts(1_700_000_000 - 3700), '1h ago'],
    [ts(1_700_000_000 - 90_000), '1d ago'],
  ])('%j → %s', (t, out) => expect(fmtTsRel(t, now)).toBe(out));
});

// fmtTsAbs, tsToMs, fmtDuration similarly
```

**Mutation-check:** `Math.min(100, ...)` → `Math.max(100, ...)` in `progress()` → the clamp test row (`cached+completed > total → 100`) fails.

## Exit criteria

- `test -f rio-dashboard/src/lib/buildInfo.ts`
- `grep -c 'function progress' rio-dashboard/src/pages/Builds.svelte rio-dashboard/src/components/BuildDrawer.svelte` → 0 (local copies deleted)
- `grep -c "from '../lib/buildInfo'" rio-dashboard/src/pages/Builds.svelte rio-dashboard/src/components/BuildDrawer.svelte rio-dashboard/src/pages/Workers.svelte` → ≥3 (all three consumers import)
- `grep 'Number(ts.seconds)' rio-dashboard/src/pages/Builds.svelte rio-dashboard/src/components/BuildDrawer.svelte` → 0 hits (hand-roll gone, bufbuild-standardized)
- `pnpm --filter rio-dashboard test -- buildInfo` → ≥8 passed (T3's table rows)
- `pnpm --filter rio-dashboard test` → all existing tests pass (refactor is semantics-preserving)
- `/nbr .#ci` green

## Tracey

No new markers. Pure refactor — no spec-observable behavior change. `r[dash.journey.build-to-logs]` annotations at `Builds.svelte:2` / `Cluster.svelte:2` unaffected (those are at the import-block, not the deleted fn bodies).

## Files

```json files
[
  {"path": "rio-dashboard/src/lib/buildInfo.ts", "action": "NEW", "note": "T1: progress/tsToMs/fmtTsAbs/fmtTsRel/fmtDuration — standardize on @bufbuild/protobuf/wkt timestampMs"},
  {"path": "rio-dashboard/src/pages/Builds.svelte", "action": "MODIFY", "note": "T2: delete :94-118 local progress/relTime/duration, import from lib/buildInfo"},
  {"path": "rio-dashboard/src/components/BuildDrawer.svelte", "action": "MODIFY", "note": "T2: delete :29-43 local progress/fmtTs, import from lib/buildInfo"},
  {"path": "rio-dashboard/src/pages/Workers.svelte", "action": "MODIFY", "note": "T2: delete :47-55 local rel formatter + :43-45 ageMs helper, use fmtTsRel/tsToMs"},
  {"path": "rio-dashboard/src/lib/__tests__/buildInfo.test.ts", "action": "NEW", "note": "T3: table-driven progress/fmtTsRel/fmtTsAbs/fmtDuration tests; mutation-check clamp"}
]
```

```
rio-dashboard/src/
├── lib/
│   ├── buildInfo.ts              # NEW — T1
│   └── __tests__/
│       └── buildInfo.test.ts     # NEW — T3
├── pages/
│   ├── Builds.svelte             # T2: delete :94-118
│   └── Workers.svelte            # T2: delete :43-55
└── components/
    └── BuildDrawer.svelte        # T2: delete :29-43
```

## Dependencies

```json deps
{"deps": [278, 280], "soft_deps": [390, 400, 404, 392, 389], "note": "consolidator-mc210 feature. P0278 (DONE) shipped BuildDrawer.svelte with the duplicate progress/fmtTs. P0280 shipped Graph.svelte (soft — Graph page doesn't use progress directly, but P0400's T3 poll-stop-when-terminal is same region as potential fmtDuration consumer). Soft-dep P0390 (api/types.ts barrel — T1's BuildInfo import should use ../api/types if P0390 landed; otherwise ../gen/admin_types_pb and P0390 migrates it later). Soft-dep P0400+P0404+P0392 (Builds.svelte touchers — each would re-copy progress() if this doesn't land first; sequence THIS BEFORE them for net-negative outcome). Soft-dep P0389 (adminMock test-support — T3's buildInfo.test.ts is orthogonal, same __tests__/ dir). Builds.svelte count=low (P0278 + P0404 + this); BuildDrawer.svelte count=low (P0278 + P0311-T44); Workers.svelte count=low. All three edits are pure deletion + single-import-add — rebase-clean with any other toucher."}
```

**Depends on:** [P0278](plan-0278-build-drawer-detail-page.md) (BuildDrawer.svelte exists — DONE), [P0280](plan-0280-dashboard-dag-viz-xyflow.md) (Graph.svelte exists — soft).
**Conflicts with:** [`Builds.svelte`](../../rio-dashboard/src/pages/Builds.svelte) — [P0404](plan-0404-dashboard-cursor-chained-paging.md) rewrites the pagination at `:55`, this deletes `:94-118`; non-overlapping. [P0304-T116](plan-0304-trivial-batch-p0222-harness.md) touches `Builds.svelte` prop-syntax; different region. [`Workers.svelte`](../../rio-dashboard/src/pages/Workers.svelte) — [P0304-T117](plan-0304-trivial-batch-p0222-harness.md) adds `document.hidden` poll-pause; different region. [P0311-T47](plan-0311-test-gap-batch-cli-recovery-dash.md) tests Workers.svelte poll-cleanup; orthogonal. [`BuildDrawer.svelte`](../../rio-dashboard/src/components/BuildDrawer.svelte) — [P0311-T44](plan-0311-test-gap-batch-cli-recovery-dash.md) adds tab-switch tests; tests the moved fn via imports, unaffected. Low conflict risk.
