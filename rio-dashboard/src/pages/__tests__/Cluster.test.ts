// r[verify dash.journey.build-to-logs]
// Proves the Cluster page renders RPC results and surfaces transport
// errors. The full click→DAG→node→stream journey is covered end-to-end
// by later plans; this is the "page loads and talks to the scheduler"
// prerequisite check.
import { render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  adminMock,
  flushSvelte,
  setupStandardBeforeEach,
  teardownStandardAfterEach,
} from '../../test-support/admin-mock';

vi.mock('../../api/admin', () => ({ admin: adminMock }));

import Cluster from '../Cluster.svelte';

const { clusterStatus } = adminMock;

describe('Cluster', () => {
  // Cluster.svelte's $effect registers a 5s setInterval; fake timers
  // (from setupStandardBeforeEach) keep the test deterministic and let
  // us assert the teardown path.
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);

  it('renders cluster counts from the RPC', async () => {
    clusterStatus.mockResolvedValue({
      totalExecutors: 5,
      activeExecutors: 3,
      drainingExecutors: 1,
      pendingBuilds: 12,
      activeBuilds: 4,
      queuedDerivations: 200,
      runningDerivations: 8,
      storeSizeBytes: 0n,
    });

    render(Cluster);
    await flushSvelte();

    const dl = screen.getByTestId('cluster-status');
    expect(dl).toHaveTextContent('3 active / 5 total');
    expect(dl).toHaveTextContent('12 pending / 4 active');
    expect(dl).toHaveTextContent('200 queued / 8 running');
  });

  it('surfaces transport errors via role=alert', async () => {
    clusterStatus.mockRejectedValue(new Error('upstream connect error'));

    render(Cluster);
    await flushSvelte();

    const alert = screen.getByRole('alert');
    expect(alert).toHaveTextContent('scheduler unreachable');
    expect(alert).toHaveTextContent('upstream connect error');
  });

  it('clears the poll interval on unmount', async () => {
    clusterStatus.mockResolvedValue({
      totalExecutors: 0,
      activeExecutors: 0,
      drainingExecutors: 0,
      pendingBuilds: 0,
      activeBuilds: 0,
      queuedDerivations: 0,
      runningDerivations: 0,
      storeSizeBytes: 0n,
    });

    const { unmount } = render(Cluster);
    await flushSvelte();
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
