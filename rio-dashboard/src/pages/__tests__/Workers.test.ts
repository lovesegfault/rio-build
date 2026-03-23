// Workers page: ListWorkers render + heartbeat-stale classification.
// The >30s-ago → red-cell rule is the operator's dead-worker signal;
// this test pins it down with fixture timestamps on either side of the
// threshold.
import { timestampFromMs } from '@bufbuild/protobuf/wkt';
import { render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  adminMock,
  flushSvelte,
  setupStandardBeforeEach,
  teardownStandardAfterEach,
} from '../../test-support/admin-mock';

vi.mock('../../api/admin', () => ({ admin: adminMock }));

import Workers from '../Workers.svelte';

const { listWorkers } = adminMock;

describe('Workers page', () => {
  // Fixed "now" = 2026-01-01T00:01:00Z (setupStandardBeforeEach default)
  // so tsToMs/fmtTsRel are deterministic against the heartbeat fixture timestamps.
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);

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
    await flushSvelte();

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
    await flushSvelte();

    const cells = screen.getAllByTestId('heartbeat-cell');
    // First row (5s ago) — not stale.
    expect(cells[0]).not.toHaveClass('stale');
    expect(cells[0]).toHaveTextContent('5s ago');
    // Second row (45s ago) — over the 30s threshold.
    expect(cells[1]).toHaveClass('stale');
    expect(cells[1]).toHaveTextContent('45s ago');
  });
});
