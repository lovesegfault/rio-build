// r[verify dash.journey.build-to-logs]
// Proves "click build → drawer opens" — step 1 of the killer journey.
// Later plans (P0279 LogViewer, P0280 Graph) extend the same marker
// with the node-click → log-stream legs.
import { fireEvent, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import {
  adminMock,
  teardownStandardAfterEach,
} from '../../test-support/admin-mock';

// The drawer embeds LogViewer (P0279), which fires getBuildLogs on
// mount — adminMock's empty-generator default keeps the `for await`
// from crashing on `undefined is not iterable`. The log stream itself
// is covered in lib/__tests__/logStream.test.ts.
vi.mock('../../api/admin', () => ({ admin: adminMock }));

import Builds from '../Builds.svelte';

const { listBuilds } = adminMock;

// Helper — the generated BuildInfo is a branded `Message<...>` intersection
// that vitest fixtures can't satisfy without `create(BuildInfoSchema, ...)`.
// Tests only exercise the render path, so a structurally-compatible plain
// object works; the `state` field is the raw wire enum value.
function mkBuild(over: Partial<Record<string, unknown>> = {}) {
  return {
    buildId: 'aaaa-bbbb-cccc-dddd',
    tenantId: 'tenant-1',
    priorityClass: 'normal',
    state: 2, // ACTIVE
    totalDerivations: 10,
    completedDerivations: 3,
    cachedDerivations: 2,
    submittedAt: undefined,
    startedAt: undefined,
    finishedAt: undefined,
    errorSummary: '',
    ...over,
  };
}

describe('Builds', () => {
  afterEach(teardownStandardAfterEach);

  it('renders one row per build with mixed states', async () => {
    listBuilds.mockResolvedValue({
      builds: [
        mkBuild({ buildId: 'pending-1', state: 1 }),
        mkBuild({ buildId: 'active-2', state: 2 }),
        mkBuild({ buildId: 'failed-3', state: 4 }),
      ],
      totalCount: 3,
    });

    render(Builds);
    // $effect fires post-mount on the microtask queue; two ticks flush
    // the IIFE's resolved promise AND the resulting state assignment.
    await tick();
    await tick();

    const rows = screen.getAllByTestId('build-row');
    expect(rows).toHaveLength(3);
    expect(rows[0]).toHaveTextContent('pending');
    expect(rows[1]).toHaveTextContent('active');
    expect(rows[2]).toHaveTextContent('failed');
    // Progress: (3+2)/10 = 50%
    expect(rows[1]).toHaveTextContent('50%');
  });

  it('opens the drawer on row click', async () => {
    listBuilds.mockResolvedValue({
      builds: [mkBuild({ buildId: 'click-target-id', state: 3 })],
      totalCount: 1,
    });

    render(Builds);
    await tick();
    await tick();

    expect(screen.queryByTestId('build-drawer')).not.toBeInTheDocument();

    const row = screen.getByTestId('build-row');
    await fireEvent.click(row);

    const drawer = screen.getByTestId('build-drawer');
    expect(drawer).toHaveTextContent('click-target-id');
    expect(drawer).toHaveTextContent('succeeded');
    // Both tab buttons render; Logs is active by default and hosts the
    // live LogViewer.
    expect(drawer).toHaveTextContent('Logs');
    expect(drawer).toHaveTextContent('Graph');
    expect(screen.getByTestId('log-viewer')).toBeInTheDocument();

    // Close via backdrop click.
    await fireEvent.click(screen.getByTestId('drawer-backdrop'));
    expect(screen.queryByTestId('build-drawer')).not.toBeInTheDocument();
  });

  it('sends statusFilter in the RPC when a filter pill is clicked', async () => {
    listBuilds.mockResolvedValue({ builds: [], totalCount: 0 });

    render(Builds);
    await tick();
    await tick();

    // Initial fetch: unfiltered.
    expect(listBuilds).toHaveBeenCalledTimes(1);
    expect(listBuilds).toHaveBeenLastCalledWith({
      statusFilter: '',
      limit: 100,
      offset: 0,
      tenantFilter: '',
    });

    // Click the "failed" pill → new fetch with the scheduler's lowercase
    // status string (ListBuildsRequest.status_filter is the raw PG enum
    // string, not the proto BuildState numeric).
    const failedPill = screen.getByRole('button', { name: 'failed' });
    await fireEvent.click(failedPill);
    await tick();
    await tick();

    expect(listBuilds).toHaveBeenCalledTimes(2);
    expect(listBuilds).toHaveBeenLastCalledWith({
      statusFilter: 'failed',
      limit: 100,
      offset: 0,
      tenantFilter: '',
    });
  });

  // r[verify sched.admin.list-builds]
  // Proves the dashboard CHAINS off the server's next_cursor instead of
  // re-deriving offset = page * PAGE_SIZE. This is the first client-side
  // consumer of P0271's keyset path — without this test the keyset
  // handler is covered only by scheduler-internal unit tests that never
  // exercise the "client feeds next_cursor back as cursor" round-trip.
  it('forward-chains cursors: page 2 sends cursor=c1 not offset=100', async () => {
    // Full page of PAGE_SIZE rows → server sets next_cursor. Call-count
    // dependent return: first call (cursor undefined) → c1; second call
    // (cursor=c1) → c2. The assert below proves the second call's cursor
    // arg IS c1, i.e. the value from call #1's response — not offset
    // arithmetic, not a fresh RPC to "get a cursor".
    const fullPage = Array.from({ length: 100 }, (_, i) =>
      mkBuild({ buildId: `b-${i}` }),
    );
    listBuilds.mockImplementation((req: { cursor?: string }) => {
      if (req.cursor === undefined) {
        return Promise.resolve({
          builds: fullPage,
          totalCount: 250,
          nextCursor: 'c1',
        });
      }
      if (req.cursor === 'c1') {
        return Promise.resolve({
          builds: fullPage,
          totalCount: 0, // ignored past page 1 (capture-once)
          nextCursor: 'c2',
        });
      }
      return Promise.resolve({ builds: [], totalCount: 0 });
    });

    render(Builds);
    await tick();
    await tick();

    expect(listBuilds).toHaveBeenCalledTimes(1);
    // First page: cursor-less, offset 0.
    expect(listBuilds).toHaveBeenNthCalledWith(1, {
      statusFilter: '',
      limit: 100,
      offset: 0,
      cursor: undefined,
      tenantFilter: '',
    });

    // The response stashed c1 → Next button enables. Click it.
    const next = screen.getByRole('button', { name: 'next' });
    expect(next).not.toBeDisabled();
    await fireEvent.click(next);
    await tick();
    await tick();

    // Second call: the load-bearing assert. cursor === 'c1' (from the
    // prior response's nextCursor), NOT offset: 100. offset stays 0 —
    // the scheduler ignores it when cursor is set (admin/builds.rs:93),
    // but we also assert it here to catch any regression back to
    // `p * PAGE_SIZE` arithmetic.
    expect(listBuilds).toHaveBeenNthCalledWith(2, {
      statusFilter: '',
      limit: 100,
      offset: 0,
      cursor: 'c1',
      tenantFilter: '',
    });
  });

  it('back navigation re-uses stashed cursor from the stack', async () => {
    // Three pages deep then Previous → the call for page 2 should use
    // the SAME cursor ('c1') that was stashed during the forward walk,
    // not a re-derived offset or a fresh "give me page 2" RPC. Keyset
    // cursors are forward-only; the stack is what makes Back possible.
    const fullPage = Array.from({ length: 100 }, (_, i) =>
      mkBuild({ buildId: `b-${i}` }),
    );
    const cursorChain: Record<string, string | undefined> = {
      __none: 'c1',
      c1: 'c2',
      c2: undefined, // page 3 is last → no next_cursor
    };
    listBuilds.mockImplementation((req: { cursor?: string }) =>
      Promise.resolve({
        builds: fullPage,
        totalCount: 300,
        nextCursor: cursorChain[req.cursor ?? '__none'],
      }),
    );

    render(Builds);
    await tick();
    await tick();

    const next = screen.getByRole('button', { name: 'next' });
    const prev = screen.getByRole('button', { name: 'prev' });

    // Forward: page 1 → 2 → 3.
    await fireEvent.click(next);
    await tick();
    await tick();
    await fireEvent.click(next);
    await tick();
    await tick();

    // Three calls so far: undefined, c1, c2.
    expect(listBuilds).toHaveBeenCalledTimes(3);
    expect(listBuilds.mock.calls[2][0].cursor).toBe('c2');

    // Back → page 2. The cursor sent MUST be c1 (stashed on the way up),
    // proving the stack does the work — no offset math, no fresh lookup.
    await fireEvent.click(prev);
    await tick();
    await tick();

    expect(listBuilds).toHaveBeenCalledTimes(4);
    expect(listBuilds.mock.calls[3][0].cursor).toBe('c1');
    expect(listBuilds.mock.calls[3][0].offset).toBe(0);
  });

  it('filter change resets the cursor stack', async () => {
    // Walk forward one page, then click a filter pill. The next fetch
    // must have cursor: undefined — the old filter's cursors don't point
    // at rows in the new result set. Also proves the stack was cleared:
    // after the filter, Next should be disabled until the fresh page-1
    // response re-seeds c1.
    const fullPage = Array.from({ length: 100 }, (_, i) =>
      mkBuild({ buildId: `b-${i}` }),
    );
    listBuilds.mockImplementation(() =>
      Promise.resolve({ builds: fullPage, totalCount: 250, nextCursor: 'c1' }),
    );

    render(Builds);
    await tick();
    await tick();

    const next = screen.getByRole('button', { name: 'next' });
    await fireEvent.click(next);
    await tick();
    await tick();

    // Two calls: page 1 (cursor undefined), page 2 (cursor c1).
    expect(listBuilds).toHaveBeenCalledTimes(2);
    expect(listBuilds.mock.calls[1][0].cursor).toBe('c1');

    // Swap the mock: filtered result is a SHORT page → no nextCursor.
    // This lets us assert Next disables post-reset (old c1 was purged).
    listBuilds.mockImplementation(() =>
      Promise.resolve({ builds: [mkBuild()], totalCount: 1 }),
    );

    const failedPill = screen.getByRole('button', { name: 'failed' });
    await fireEvent.click(failedPill);
    await tick();
    await tick();

    // Third call: cursor MUST be undefined — filter reset cleared the
    // stack. Any leftover 'c1' here would fetch stale rows from the
    // unfiltered set.
    expect(listBuilds).toHaveBeenCalledTimes(3);
    expect(listBuilds.mock.calls[2][0].cursor).toBeUndefined();
    expect(listBuilds.mock.calls[2][0].statusFilter).toBe('failed');
    // And the stack is empty past index 0: Next disables (the old c1 is
    // gone, and the short filtered page didn't seed a new one).
    expect(next).toBeDisabled();
  });

  it('resolves a deep-link id via broad fetch when not on current page', async () => {
    // First call (the paginated list effect) returns a different build;
    // second call (the deep-link fallback's limit:1000 fetch) returns
    // the target. Ordering matters — both fire on mount, but the list
    // effect registers first.
    listBuilds
      .mockResolvedValueOnce({
        builds: [mkBuild({ buildId: 'not-the-one' })],
        totalCount: 1,
      })
      .mockResolvedValueOnce({
        builds: [mkBuild({ buildId: 'deep-link-target', state: 4 })],
        totalCount: 1,
      });

    render(Builds, { props: { id: 'deep-link-target' } });
    await tick();
    await tick();
    await tick();

    // Drawer opens on the found build — no row-click involved.
    const drawer = screen.getByTestId('build-drawer');
    expect(drawer).toHaveTextContent('deep-link-target');
    // And the fallback used the scheduler's max clamp.
    expect(listBuilds).toHaveBeenCalledWith({
      statusFilter: '',
      limit: 1000,
      offset: 0,
      tenantFilter: '',
    });
  });
});
