// Centralized admin-RPC mock surface + Svelte-effect flush helpers.
//
// PROBLEM: every page/component test that renders a management-action
// child (DrainButton, ClearPoisonButton, the BuildDrawer's embedded
// LogViewer) needs that child's RPC stubbed even if the test never
// clicks it — otherwise `admin.drainExecutor is not a function` at
// render-time. Pre-P0389 every test file re-derived the same
// vi.hoisted/vi.mock/fake-timer/flush dance; each new page test started
// from a copy of DrainButton.test.ts.
//
// PATTERN: tests import adminMock + helpers from here, then keep a
// single depth-specific `vi.mock('../../api/admin', () => ({ admin:
// adminMock }))` line. Vitest hoists that vi.mock to the top of the
// *test* module; its factory runs lazily when the component under test
// first pulls `../../api/admin`. Vitest's transform ALSO hoists imports
// from a module referenced inside a vi.mock factory ahead of other
// imports ("placing the import from this file first"), so by the time
// the factory fires, the `adminMock` live binding is resolved.
//
// vi.hoisted() is deliberately NOT used here: vitest refuses to export
// a vi.hoisted binding ("Cannot export hoisted variable"). Plain module
// exports work because this file has no vi.mock calls of its own — the
// hoisting concern lives entirely in the importing test file.
import { tick } from 'svelte';
import { type Mock, vi } from 'vitest';

// One stub per AdminService RPC. Kept in sync with admin.proto via the
// parity test in admin-mock.test.ts — add a method to the proto and
// that test fails until this map grows the matching stub.
//
// Streaming RPCs (getBuildLogs, triggerGC) get an empty-generator
// default so `for await (const chunk of admin.getBuildLogs(...))`
// doesn't throw `undefined is not iterable` when a page renders a
// LogViewer child but the test doesn't care about the stream body.
// vitest's mockReset() restores the constructor-time implementation,
// so these defaults survive teardownStandardAfterEach().
//
// Every stub is typed `Mock` (untyped Procedure), not the inferred
// `Mock<() => AsyncGenerator<never, ...>>`: the empty-generator default
// yields nothing → TS infers the yield type as `never`, which then
// rejects mockImplementation calls that yield real chunk shapes. The
// blanket `Mock` loosening matches what per-file `vi.fn()` gave before.
const emptyStream = async function* () {};
export const adminMock = {
  clusterStatus: vi.fn(),
  listExecutors: vi.fn(),
  listBuilds: vi.fn(),
  getBuildLogs: vi.fn(emptyStream) as Mock,
  triggerGC: vi.fn(emptyStream) as Mock,
  drainExecutor: vi.fn(),
  clearPoison: vi.fn(),
  listPoisoned: vi.fn(),
  listTenants: vi.fn(),
  createTenant: vi.fn(),
  // Empty-default so pages embedding DagView/Graph (which calls
  // getBuildGraph at mount) don't crash on undefined.nodes. Per-test
  // overrides via .mockResolvedValueOnce(...) still work.
  getBuildGraph: vi
    .fn()
    .mockResolvedValue({ nodes: [], edges: [], truncated: false, totalNodes: 0 }),
  getSizeClassStatus: vi.fn(),
  // Extend as AdminService grows — one site, not N test files.
} satisfies Record<string, Mock>;

// toast.info/error stubs for components that surface RPC success/failure
// via the toast portal. Tests that exercise those paths vi.mock the
// toast module against this object (see DrainButton.test.ts). Tests
// that don't can ignore it — the real toast store is a harmless no-op
// under jsdom (pushes to an array nobody subscribes to).
export const toastMock = {
  info: vi.fn(),
  error: vi.fn(),
};

/**
 * Svelte effect flush. `tick()` runs Svelte's microtask scheduler so
 * $effect/$derived updates land in the DOM; `advanceTimersByTimeAsync(0)`
 * drains any fake-timer-queued promise continuations without tripping
 * a real interval (which `runOnlyPendingTimersAsync` would).
 *
 * Call after render/click/resolve when the component does `await
 * admin.X(...)` inside an effect and you want the next assertion to see
 * the post-await state.
 */
export async function flushSvelte(): Promise<void> {
  await tick();
  await vi.advanceTimersByTimeAsync(0);
}

/**
 * Standard beforeEach: fake timers, fixed system time, confirm
 * auto-accept. Fixed `now` keeps relative-time rendering (e.g.
 * Workers' heartbeat-ago cells) deterministic. jsdom's `confirm()` is
 * a real prompt that would block the test — the stub makes the
 * click-path proceed past the guard.
 */
export function setupStandardBeforeEach(
  opts: { systemTime?: Date } = {},
): void {
  vi.useFakeTimers();
  vi.setSystemTime(opts.systemTime ?? new Date('2026-01-01T00:01:00Z'));
  vi.stubGlobal('confirm', vi.fn(() => true));
}

/**
 * Standard afterEach: undo globals, reset every adminMock stub (which
 * for the streaming ones restores the empty-generator default), reset
 * toastMock, back to real timers. Call-order mirrors setup: globals
 * first so a stray `confirm` in a teardown effect sees the stub gone,
 * then mocks, then timers.
 */
export function teardownStandardAfterEach(): void {
  vi.unstubAllGlobals();
  for (const fn of Object.values(adminMock)) fn.mockReset();
  toastMock.info.mockReset();
  toastMock.error.mockReset();
  vi.useRealTimers();
}
