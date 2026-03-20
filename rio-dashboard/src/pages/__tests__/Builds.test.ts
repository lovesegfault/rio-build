// r[verify dash.journey.build-to-logs]
// Proves "click build → drawer opens" — step 1 of the killer journey.
// Later plans (P0279 LogViewer, P0280 Graph) extend the same marker
// with the node-click → log-stream legs.
import { fireEvent, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

// Same hoisted-mock pattern as Cluster.test.ts: vi.mock is lifted above
// all imports, so the vi.fn() ref has to come from vi.hoisted() or the
// factory closes over undefined.
const { listBuilds } = vi.hoisted(() => ({
  listBuilds: vi.fn(),
}));
vi.mock('../../api/admin', () => ({ admin: { listBuilds } }));

import Builds from '../Builds.svelte';

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
  afterEach(() => {
    listBuilds.mockReset();
  });

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
    // Tab placeholders exist — P0279/P0280 will swap the bodies.
    expect(drawer).toHaveTextContent('Logs');
    expect(drawer).toHaveTextContent('Graph');

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
