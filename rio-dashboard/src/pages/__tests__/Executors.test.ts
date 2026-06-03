// Executors page: ListExecutors render + kind filter + the bug_357
// pin: NO staleness class at any attempt age — the timestamp is the
// attempt-open time (never advances mid-build), so a threshold
// highlight inverts into 'every long build screams red'. Liveness is
// the OA2 wedge alert + Job census, not this page.
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
      busy: true,
      status,
      attemptOpened: timestampFromMs(now - ageSeconds * 1000),
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
  });

  it('does_not_mark_long_running_attempts_stale', async () => {
    // bug_357: the timestamp is the attempt-OPEN time (the pull). A
    // pull-mode pod sends nothing between the pull and the report, so
    // a 45-minute-old attempt-open on a long compile is HEALTHY — the
    // stream-era ">30s since last heartbeat = dead executor" highlight
    // inverted into "every build longer than 30s screams red".
    // Liveness is owned by the Job/pod phase + the OA2 wedge alert;
    // the page shows plain relative age (CLI parity).
    listExecutors.mockResolvedValue({
      executors: [
        mkExecutor('e-fresh', 'alive', 5),
        mkExecutor('e-long', 'alive', 2700),
      ],
    });

    render(Executors);
    await flushSvelte();

    const cells = screen.getAllByTestId('pulled-cell');
    expect(cells[0]).not.toHaveClass('stale');
    expect(cells[0]).toHaveTextContent('5s ago');
    // A 45-minute-old attempt is a long build, not a dead pod — NO
    // stale class at any age.
    expect(cells[1]).not.toHaveClass('stale');
    expect(cells[1]).toHaveTextContent('45m ago');
  });

  // r[verify dash.executors.kind-filter]
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
