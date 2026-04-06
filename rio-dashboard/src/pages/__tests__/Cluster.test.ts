// r[verify dash.journey.build-to-logs]
// Proves the Cluster page renders RPC results and surfaces transport
// errors. The full click→DAG→node→stream journey is covered end-to-end
// by later plans; this is the "page loads and talks to the scheduler"
// prerequisite check.
import { render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

// Hoisted mock: vitest lifts vi.mock calls to the top of the module, so
// the vi.fn() reference must come from vi.hoisted or the mock factory
// closes over an undefined binding.
const { clusterStatus } = vi.hoisted(() => ({
  clusterStatus: vi.fn(),
}));
vi.mock('../../api/admin', () => ({ admin: { clusterStatus } }));

// Imported after vi.mock so the Svelte module sees the mocked admin.
import Cluster from '../Cluster.svelte';

describe('Cluster', () => {
  beforeEach(() => {
    // Cluster.svelte's $effect registers a 5s setInterval. Fake timers
    // keep the test deterministic and let us assert the teardown path.
    vi.useFakeTimers();
  });

  afterEach(() => {
    clusterStatus.mockReset();
    vi.useRealTimers();
  });

  it('renders cluster counts from the RPC', async () => {
    clusterStatus.mockResolvedValue({
      totalWorkers: 5,
      activeWorkers: 3,
      drainingWorkers: 1,
      pendingBuilds: 12,
      activeBuilds: 4,
      queuedDerivations: 200,
      runningDerivations: 8,
      storeSizeBytes: 0n,
    });

    render(Cluster);
    // $effect fires after mount on the microtask queue; tick() flushes
    // svelte's scheduler. advanceTimersByTimeAsync(0) then resolves the
    // refresh() promise and its `.then` callback without tripping the
    // 5s interval (which runOnlyPendingTimersAsync would).
    await tick();
    await vi.advanceTimersByTimeAsync(0);

    const dl = screen.getByTestId('cluster-status');
    expect(dl).toHaveTextContent('3 active / 5 total');
    expect(dl).toHaveTextContent('12 pending / 4 active');
    expect(dl).toHaveTextContent('200 queued / 8 running');
  });

  it('surfaces transport errors via role=alert', async () => {
    clusterStatus.mockRejectedValue(new Error('upstream connect error'));

    render(Cluster);
    await tick();
    await vi.advanceTimersByTimeAsync(0);

    const alert = screen.getByRole('alert');
    expect(alert).toHaveTextContent('scheduler unreachable');
    expect(alert).toHaveTextContent('upstream connect error');
  });

  it('clears the poll interval on unmount', async () => {
    clusterStatus.mockResolvedValue({
      totalWorkers: 0,
      activeWorkers: 0,
      drainingWorkers: 0,
      pendingBuilds: 0,
      activeBuilds: 0,
      queuedDerivations: 0,
      runningDerivations: 0,
      storeSizeBytes: 0n,
    });

    const { unmount } = render(Cluster);
    await tick();
    await vi.advanceTimersByTimeAsync(0);
    expect(clusterStatus).toHaveBeenCalledTimes(1);

    // Advance past one poll window — interval fires once.
    await vi.advanceTimersByTimeAsync(5000);
    expect(clusterStatus).toHaveBeenCalledTimes(2);

    unmount();
    // After unmount the $effect teardown must have run clearInterval;
    // advancing further produces no new calls. If the teardown were
    // missing this would climb to 4.
    await vi.advanceTimersByTimeAsync(10000);
    expect(clusterStatus).toHaveBeenCalledTimes(2);
  });
});
