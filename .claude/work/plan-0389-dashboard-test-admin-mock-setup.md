# Plan 0389: Dashboard test admin-mock setup — extract vi.hoisted/vi.mock/fake-timers boilerplate

consol-mc185 finding (unassigned until this plan). Six dashboard test files carry identical setup boilerplate: `vi.hoisted()` for admin RPC stubs, `vi.mock('../../api/admin', ...)` with path-depth-specific relativity, `vi.useFakeTimers()` + `vi.setSystemTime()` in `beforeEach`, `vi.stubGlobal('confirm', ...)` for jsdom's prompt bypass, and `await tick(); await vi.advanceTimersByTimeAsync(0)` flush sequences (13 call sites). Each file re-derives the same setup; each new management-action page starts from a copy-paste of [`DrainButton.test.ts`](../../rio-dashboard/src/components/__tests__/DrainButton.test.ts).

The six files: [`DrainButton.test.ts`](../../rio-dashboard/src/components/__tests__/DrainButton.test.ts), [`ClearPoisonButton.test.ts`](../../rio-dashboard/src/components/__tests__/ClearPoisonButton.test.ts), [`Workers.test.ts`](../../rio-dashboard/src/pages/__tests__/Workers.test.ts), [`Builds.test.ts`](../../rio-dashboard/src/pages/__tests__/Builds.test.ts), [`GC.test.ts`](../../rio-dashboard/src/pages/__tests__/GC.test.ts), [`Cluster.test.ts`](../../rio-dashboard/src/pages/__tests__/Cluster.test.ts). [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T44 through T47 will add four MORE of these (BuildDrawer, stream-error, unmount-aborts, poll-cleanup tests) — each adding the same preamble.

The boilerplate isn't purely cosmetic: the `vi.mock` path `'../../api/admin'` vs `'../api/admin'` depends on file depth (`pages/__tests__/` vs `components/__tests__/`), and missing a stub for a component the page USES (e.g., `drainWorker` when rendering `Workers.svelte` which embeds `DrainButton`) crashes the test on render — comment at [`Workers.test.ts:11-15`](../../rio-dashboard/src/pages/__tests__/Workers.test.ts) explains this gotcha every file re-learns.

## Entry criteria

- [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) merged (Cluster.svelte + test scaffolding exist)
- [P0278](plan-0278-dashboard-build-list-drawer.md) merged (Builds.svelte + test exist)
- [P0281](plan-0281-dashboard-management-actions.md) merged (DrainButton/ClearPoisonButton/GC pages + tests exist — the 6-file boilerplate set this plan extracts from)

## Tasks

### T1 — `refactor(dashboard):` extract test-support/admin-mock.ts — setupAdminMock + flushSvelte

NEW [`rio-dashboard/src/test-support/admin-mock.ts`](../../rio-dashboard/src/test-support/admin-mock.ts):

```ts
import { tick } from 'svelte';
import { vi } from 'vitest';

/**
 * Centralized admin-RPC mock surface. Tests call makeAdminMock() at
 * module top, destructure the RPC stubs they care about, and forget
 * about vi.mock path-depth relativity.
 *
 * GOTCHA this file solves: every page that renders a management-action
 * child (DrainButton, ClearPoisonButton) needs that child's RPC stubbed
 * even if the test never clicks it — otherwise `admin.drainWorker is
 * not a function` at render-time. The blanket-stub here covers all
 * known admin RPCs so page-level tests don't crash on transitive deps.
 */
export const adminMock = vi.hoisted(() => ({
  listWorkers: vi.fn(),
  drainWorker: vi.fn(),
  listBuilds: vi.fn(),
  clearPoison: vi.fn(),
  triggerGC: vi.fn(),
  clusterStatus: vi.fn(),
  listTenants: vi.fn(),
  // Extend as AdminService grows — one site, not N test files.
}));

export const toastMock = vi.hoisted(() => ({
  info: vi.fn(),
  error: vi.fn(),
}));

/** Svelte effect flush: tick() runs the scheduler, advanceTimersByTimeAsync(0)
 *  drains the fake-timer-queued promise microtasks. Call after render/click
 *  when the component does `await admin.X(...)` inside an effect. */
export async function flushSvelte() {
  await tick();
  await vi.advanceTimersByTimeAsync(0);
}

/** Standard beforeEach: fake timers, fixed system time, confirm auto-accept. */
export function setupStandardBeforeEach(opts: { systemTime?: Date } = {}) {
  vi.useFakeTimers();
  vi.setSystemTime(opts.systemTime ?? new Date('2026-01-01T00:01:00Z'));
  vi.stubGlobal('confirm', vi.fn(() => true));
}

/** Standard afterEach: undo globals, reset all adminMock stubs, real timers. */
export function teardownStandardAfterEach() {
  vi.unstubAllGlobals();
  for (const fn of Object.values(adminMock)) fn.mockReset();
  toastMock.info.mockReset();
  toastMock.error.mockReset();
  vi.useRealTimers();
}
```

**CARE — vi.hoisted at module scope**: `vi.hoisted()` must run before `vi.mock()`. The helper EXPORTS the hoisted object; test files import it, then do their own `vi.mock('../../api/admin', () => ({ admin: adminMock }))` — Vitest hoists `vi.mock` calls to top of module regardless of source position, but the path string is file-relative. Two options:

- **Option A (recommended)**: keep `vi.mock(...)` in each test file with its depth-specific path; the helper provides the shared `adminMock` object. ~2 lines per file (import + vi.mock), zero path-depth reasoning in the helper.
- **Option B (vitest `__mocks__` folder)**: create `rio-dashboard/src/api/__mocks__/admin.ts` that exports `{ admin: adminMock }`, tests call `vi.mock('../../api/admin')` with no factory (vitest resolves `__mocks__/` sibling). Cleaner but ties the mock to filesystem layout and makes the mock harder to find.

Prefer **Option A** — explicit is better, and the `vi.mock` path is the ONE depth-specific piece; everything else centralizes.

### T2 — `refactor(dashboard):` migrate 6 test files to admin-mock.ts helpers

MODIFY the six test files. Each replaces its local `vi.hoisted`/`vi.mock`/`beforeEach`/`afterEach`/flush-dance with:

```ts
import { adminMock, flushSvelte, setupStandardBeforeEach, teardownStandardAfterEach } from '../../test-support/admin-mock';
vi.mock('../../api/admin', () => ({ admin: adminMock }));
// Tests destructure what they use:
const { drainWorker } = adminMock;

describe('...', () => {
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);
  // ... test bodies use `await flushSvelte()` instead of `await tick(); await vi.advanceTimersByTimeAsync(0)`
});
```

Files + local-stub deletions (re-grep at dispatch — line refs from sprint-1 @ 4ebd385d):

| File | Drops | Notes |
|---|---|---|
| `DrainButton.test.ts` | `:10-16` hoisted+mock, `:26-38` beforeEach/afterEach | keeps `workers()` fixture local; 4× `advanceTimersByTimeAsync(0)`→`flushSvelte()` |
| `ClearPoisonButton.test.ts` | same shape | 3× flush-sites |
| `Workers.test.ts` | `:10-30` hoisted+mock+beforeEach/afterEach, `:11-15` gotcha comment moves to helper doc | 2× flush-sites; `mkWorker` stays local |
| `Builds.test.ts` | hoisted+mock+setup | — |
| `GC.test.ts` | `:11-12` hoisted+mock | `chunk()` stays local; tick-loop at `:48-50` stays (generator-drain, NOT the standard flush) |
| `Cluster.test.ts` | hoisted+mock+setup, `:47-51` flush-comment moves to helper doc | 3× flush-sites |

**CARE — GC.test.ts tick-loop**: [`GC.test.ts:48-50`](../../rio-dashboard/src/pages/__tests__/GC.test.ts) has a `for (let i = 0; i < 4; i++) { await tick(); await Promise.resolve(); }` generator-drain loop. This is NOT `flushSvelte()` (which uses `advanceTimersByTimeAsync`, not `Promise.resolve`). Keep as-is or add a separate `drainAsyncGenerator(n)` helper. The distinction matters: fake-timers vs microtask-drain are different mechanisms.

**CARE — toastMock** optionality: only DrainButton/ClearPoisonButton tests mock toast. Other tests don't need it. Either make it optional (tests that need it `vi.mock('../../lib/toast', () => ({ toast: toastMock }))` explicitly) or blanket-mock in all (harmless no-op for non-toast tests). Prefer explicit per-file — matches the `vi.mock('../../api/admin')` pattern.

### T3 — `test(dashboard):` admin-mock.ts self-test — adminMock surface parity with AdminService

NEW [`rio-dashboard/src/test-support/admin-mock.test.ts`](../../rio-dashboard/src/test-support/admin-mock.test.ts):

```ts
import { describe, expect, it } from 'vitest';
import { AdminService } from '../gen/admin_pb';
import { adminMock } from './admin-mock';

describe('adminMock surface parity', () => {
  it('stubs every AdminService method', () => {
    // GenService in connect-es v2 carries method descriptors. Iterate
    // and assert adminMock has a stub for each. Prevents the
    // "page renders child that calls unstubbed RPC → crash" gotcha
    // from recurring each time AdminService grows a method.
    const protoMethods = Object.keys(AdminService.method ?? AdminService.methods ?? {});
    for (const m of protoMethods) {
      // camelCase conversion if proto uses PascalCase method names
      const key = m[0].toLowerCase() + m.slice(1);
      expect(adminMock).toHaveProperty(key);
    }
  });
});
```

**CARE — GenService method shape**: connect-es v2's `GenService` type has `.method` (singular, map-keyed by localName). Check at dispatch: `Object.keys(AdminService.method)` → list of method localNames. If the shape differs (older protobuf-es), adjust introspection. The test's VALUE is the forcing function: add RPC to proto → this test fails until `adminMock` grows the stub → no more "renders child with unstubbed RPC" crashes.

## Exit criteria

- `/nbr .#ci` green
- `grep -rc 'vi.hoisted' rio-dashboard/src/components/__tests__/*.test.ts rio-dashboard/src/pages/__tests__/*.test.ts` → sum ≤2 (only the admin-mock.ts file itself + optional per-file toast hoisted — was ≥6)
- `grep -rc 'advanceTimersByTimeAsync(0)' rio-dashboard/src/components/__tests__/*.test.ts rio-dashboard/src/pages/__tests__/*.test.ts` → 0 in test files (all migrated to `flushSvelte()`); ≥1 in `test-support/admin-mock.ts`
- `grep 'from.*test-support/admin-mock' rio-dashboard/src/` → ≥6 hits (all six files import the helper)
- `test -f rio-dashboard/src/test-support/admin-mock.ts` (helper exists)
- `cargo nextest run -p rio-dashboard` or `pnpm vitest run` (whichever CI uses) → all existing tests pass unchanged
- T3: `admin-mock.test.ts` surface-parity test passes — adding a dummy `rpc TestFake` to `admin.proto` inside `service AdminService` → test fails until adminMock grows `testFake` stub → remove → passes (mutation proof)
- `git diff --stat -- rio-dashboard/src/` → net negative across the 6 migrated test files (helper adds ~60L, 6× ~15L drops ≈ -30L net; plus ~-25L for 13 flush-site→1-call migrations)

## Tracey

No new markers. Dashboard test-support infrastructure is not spec'd (no `r[dash.*]` marker covers test tooling). The tests being migrated carry their own `r[verify dash.*]` annotations (e.g., `r[verify dash.journey.build-to-logs]` in Builds.test.ts) — those stay unchanged, only the setup boilerplate around them moves.

## Files

```json files
[
  {"path": "rio-dashboard/src/test-support/admin-mock.ts", "action": "NEW", "note": "T1: adminMock vi.hoisted stub set + toastMock + flushSvelte + setupStandardBeforeEach + teardownStandardAfterEach"},
  {"path": "rio-dashboard/src/test-support/admin-mock.test.ts", "action": "NEW", "note": "T3: AdminService method-parity check — adminMock covers every proto RPC"},
  {"path": "rio-dashboard/src/components/__tests__/DrainButton.test.ts", "action": "MODIFY", "note": "T2: drop :10-16 hoisted+mock, :26-38 setup → helper imports; 4× flush→flushSvelte()"},
  {"path": "rio-dashboard/src/components/__tests__/ClearPoisonButton.test.ts", "action": "MODIFY", "note": "T2: drop hoisted+mock+setup → helper; 3× flush→flushSvelte()"},
  {"path": "rio-dashboard/src/pages/__tests__/Workers.test.ts", "action": "MODIFY", "note": "T2: drop :10-30 hoisted+mock+setup → helper; 2× flush→flushSvelte(); mkWorker stays local"},
  {"path": "rio-dashboard/src/pages/__tests__/Builds.test.ts", "action": "MODIFY", "note": "T2: drop hoisted+mock+setup → helper"},
  {"path": "rio-dashboard/src/pages/__tests__/GC.test.ts", "action": "MODIFY", "note": "T2: drop :11-12 hoisted+mock → helper; KEEP :48-50 generator-drain loop (NOT flushSvelte — different mechanism)"},
  {"path": "rio-dashboard/src/pages/__tests__/Cluster.test.ts", "action": "MODIFY", "note": "T2: drop hoisted+mock+setup → helper; 3× flush→flushSvelte(); :47-51 comment moves to helper doc"}
]
```

```
rio-dashboard/src/
├── test-support/           # NEW directory
│   ├── admin-mock.ts       # T1: helpers (~60L)
│   └── admin-mock.test.ts  # T3: parity test
├── components/__tests__/
│   ├── DrainButton.test.ts       # T2: -20L boilerplate
│   └── ClearPoisonButton.test.ts # T2: -15L
└── pages/__tests__/
    ├── Workers.test.ts  # T2: -20L
    ├── Builds.test.ts   # T2: -15L
    ├── GC.test.ts       # T2: -5L (only hoisted+mock; keeps generator-drain)
    └── Cluster.test.ts  # T2: -18L
```

## Dependencies

```json deps
{"deps": [277, 278, 281], "soft_deps": [311, 390, 279, 280], "note": "P0277/P0278/P0281 provide the 6 test files being migrated (all DONE). discovered_from=consol-mc185. Soft-dep P0311: T44-T47 add 4 MORE test files with the same boilerplate shape — if this plan lands first, those T-tasks use the helper from day one (re-grep P0311 T44-T47 sections at dispatch and point them at test-support/admin-mock.ts). Soft-dep P0390 (gen/*_pb barrel — same planning batch): T3's `import { AdminService } from '../gen/admin_pb'` uses the gen-path depth P0390 unifies; sequence-independent (both paths work, barrel is cleaner). Soft-dep P0279/P0280 (UNIMPL — dashboard placeholder-fill plans that may add tests): if they dispatch before this plan, they add boilerplate; if after, they use the helper. All 6 target test files are low-collision (rio-dashboard tests count<5 each)."}
```

**Depends on:** [P0277](plan-0277-dashboard-app-shell-clusterstatus.md), [P0278](plan-0278-dashboard-build-list-drawer.md), [P0281](plan-0281-dashboard-management-actions.md) — all DONE; provide the 6 test files.

**Conflicts with:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T44 through T47 touch `BuildDrawer.test.ts` (NEW), `GC.test.ts`, `Workers.test.ts` — same files, both additive (T2 here modifies existing tests' setup; P0311 adds NEW test fns). Sequence-independent, rebase-clean. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T124 through T127 are attr-adds to Svelte components (not test files) — no overlap.
