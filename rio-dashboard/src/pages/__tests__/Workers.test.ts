// Workers page: ListWorkers render + heartbeat-stale classification.
// The >30s-ago → red-cell rule is the operator's dead-worker signal;
// this test pins it down with fixture timestamps on either side of the
// threshold.
import { timestampFromMs } from '@bufbuild/protobuf/wkt';
import { render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { listWorkers } = vi.hoisted(() => ({ listWorkers: vi.fn() }));
// DrainButton pulls admin.drainWorker — include a stub so the full-page
// render doesn't crash on the missing method.
vi.mock('../../api/admin', () => ({
  admin: { listWorkers, drainWorker: vi.fn() },
}));

import Workers from '../Workers.svelte';

describe('Workers page', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    // Fix "now" so ageMs is deterministic. 2026-01-01T00:01:00Z.
    vi.setSystemTime(new Date('2026-01-01T00:01:00Z'));
    vi.stubGlobal('confirm', vi.fn(() => true));
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    listWorkers.mockReset();
    vi.useRealTimers();
  });

  function mkWorker(id: string, status: string, ageSeconds: number) {
    const now = Date.now();
    return {
      workerId: id,
      systems: [],
      supportedFeatures: [],
      maxBuilds: 4,
      runningBuilds: 2,
      status,
      lastHeartbeat: timestampFromMs(now - ageSeconds * 1000),
      sizeClass: 'medium',
    };
  }

  it('renders rows with status pills and load bars', async () => {
    listWorkers.mockResolvedValue({
      workers: [mkWorker('w-fresh', 'alive', 5), mkWorker('w-old', 'alive', 45)],
    });

    render(Workers);
    await tick();
    await vi.advanceTimersByTimeAsync(0);

    const table = screen.getByTestId('workers-table');
    expect(table).toHaveTextContent('w-fresh');
    expect(table).toHaveTextContent('w-old');
    expect(table).toHaveTextContent('2/4');
    // Two rows → two DrainButtons.
    expect(screen.getAllByTestId('drain-btn')).toHaveLength(2);
  });

  it('flags >30s-stale heartbeat', async () => {
    listWorkers.mockResolvedValue({
      workers: [mkWorker('w-fresh', 'alive', 5), mkWorker('w-stale', 'alive', 45)],
    });

    render(Workers);
    await tick();
    await vi.advanceTimersByTimeAsync(0);

    const cells = screen.getAllByTestId('heartbeat-cell');
    // First row (5s ago) — not stale.
    expect(cells[0]).not.toHaveClass('stale');
    expect(cells[0]).toHaveTextContent('5s ago');
    // Second row (45s ago) — over the 30s threshold.
    expect(cells[1]).toHaveClass('stale');
    expect(cells[1]).toHaveTextContent('45s ago');
  });
});
