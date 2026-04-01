// Executors page: ListExecutors render + heartbeat-stale classification
// + kind filter. The >30s-ago → red-cell rule is the operator's
// dead-executor signal; this test pins it down with fixture timestamps
// on either side of the threshold.
import { timestampFromMs } from '@bufbuild/protobuf/wkt';
import { fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  adminMock,
  flushSvelte,
  setupStandardBeforeEach,
  teardownStandardAfterEach,
} from '../../test-support/admin-mock';

vi.mock('../../api/admin', () => ({ admin: adminMock }));

import Executors from '../Executors.svelte';

const { listExecutors } = adminMock;

describe('Executors page', () => {
  // Fixed "now" = 2026-01-01T00:01:00Z (setupStandardBeforeEach default)
  // so tsToMs/fmtTsRel are deterministic against the heartbeat fixture timestamps.
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);

  function mkExecutor(
    id: string,
    status: string,
    ageSeconds: number,
    kind = 0,
  ) {
    const now = Date.now();
    return {
      executorId: id,
      systems: [],
      supportedFeatures: [],
      runningBuilds: 1,
      status,
      lastHeartbeat: timestampFromMs(now - ageSeconds * 1000),
      sizeClass: 'medium',
      kind,
    };
  }

  it('renders rows with status and load pills', async () => {
    listExecutors.mockResolvedValue({
      executors: [mkExecutor('e-fresh', 'alive', 5), mkExecutor('e-old', 'alive', 45)],
    });

    render(Executors);
    await flushSvelte();

    const table = screen.getByTestId('executors-table');
    expect(table).toHaveTextContent('e-fresh');
    expect(table).toHaveTextContent('e-old');
    expect(screen.getAllByTestId('load-pill')[0]).toHaveTextContent('busy');
    // Two rows → two DrainButtons.
    expect(screen.getAllByTestId('drain-btn')).toHaveLength(2);
  });

  it('flags >30s-stale heartbeat', async () => {
    listExecutors.mockResolvedValue({
      executors: [mkExecutor('e-fresh', 'alive', 5), mkExecutor('e-stale', 'alive', 45)],
    });

    render(Executors);
    await flushSvelte();

    const cells = screen.getAllByTestId('heartbeat-cell');
    // First row (5s ago) — not stale.
    expect(cells[0]).not.toHaveClass('stale');
    expect(cells[0]).toHaveTextContent('5s ago');
    // Second row (45s ago) — over the 30s threshold.
    expect(cells[1]).toHaveClass('stale');
    expect(cells[1]).toHaveTextContent('45s ago');
  });

  // r[verify builder.executor.kind-gate]
  it('filters by executor kind', async () => {
    listExecutors.mockResolvedValue({
      executors: [
        mkExecutor('b-1', 'alive', 5, 0), // builder
        mkExecutor('f-1', 'alive', 5, 1), // fetcher
        mkExecutor('b-2', 'alive', 5, 0), // builder
      ],
    });

    render(Executors);
    await flushSvelte();

    // Default 'all' — all three rows, kind column populated.
    let table = screen.getByTestId('executors-table');
    expect(table).toHaveTextContent('b-1');
    expect(table).toHaveTextContent('f-1');
    expect(table).toHaveTextContent('b-2');
    const kindCells = screen.getAllByTestId('kind-cell');
    expect(kindCells[0]).toHaveTextContent('builder');
    expect(kindCells[1]).toHaveTextContent('fetcher');

    // Filter to fetchers — only f-1.
    const select = screen.getByTestId('kind-filter');
    await fireEvent.change(select, { target: { value: '1' } });
    await flushSvelte();
    table = screen.getByTestId('executors-table');
    expect(table).toHaveTextContent('f-1');
    expect(table).not.toHaveTextContent('b-1');
    expect(table).not.toHaveTextContent('b-2');

    // Filter to builders — b-1 + b-2.
    await fireEvent.change(select, { target: { value: '0' } });
    await flushSvelte();
    table = screen.getByTestId('executors-table');
    expect(table).toHaveTextContent('b-1');
    expect(table).toHaveTextContent('b-2');
    expect(table).not.toHaveTextContent('f-1');
  });
});
