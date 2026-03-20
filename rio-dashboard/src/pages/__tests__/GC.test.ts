// Second server-stream consumer. The mock yields 3 chunks from an async
// generator — same shape connect-web exposes for streaming RPCs — and
// the test asserts the page's progress <dl> updates on each chunk and
// surfaces completion. The abort-on-cancel path is also covered: the
// page stashes an AbortController and calls .abort() from the Cancel
// button; we assert the signal passed into the RPC opts flips.
import { fireEvent, render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

const { triggerGC } = vi.hoisted(() => ({ triggerGC: vi.fn() }));
vi.mock('../../api/admin', () => ({ admin: { triggerGC } }));

import GC from '../GC.svelte';

function chunk(
  scanned: bigint,
  collected: bigint,
  freed: bigint,
  done: boolean,
) {
  return {
    pathsScanned: scanned,
    pathsCollected: collected,
    bytesFreed: freed,
    isComplete: done,
    currentPath: '',
  };
}

describe('GC page', () => {
  afterEach(() => {
    triggerGC.mockReset();
  });

  it('consumes 3 progress chunks and surfaces completion', async () => {
    triggerGC.mockImplementation(async function* () {
      yield chunk(100n, 10n, 1024n, false);
      yield chunk(200n, 25n, 4096n, false);
      yield chunk(300n, 50n, 1_048_576n, true);
    });

    render(GC);
    await fireEvent.submit(screen.getByRole('button', { name: /preview gc/i }).closest('form')!);
    // Drain the async generator: each yield schedules a microtask; tick()
    // flushes Svelte's scheduler, one Promise.resolve() per await inside
    // the for-await loop. Three chunks → three settle rounds.
    for (let i = 0; i < 4; i++) {
      await tick();
      await Promise.resolve();
    }

    const stats = screen.getByTestId('gc-stats');
    expect(stats).toHaveTextContent(/scanned\s*300/);
    expect(stats).toHaveTextContent(/collected\s*50/);
    expect(stats).toHaveTextContent('1.0 MiB');
    expect(screen.getByTestId('gc-complete')).toBeInTheDocument();
    // Bar fills on completion.
    expect(screen.getByTestId('gc-progress')).toHaveAttribute(
      'aria-valuenow',
      '100',
    );
  });

  it('passes dryRun/gracePeriodHours through to the RPC', async () => {
    triggerGC.mockImplementation(async function* () {
      yield chunk(0n, 0n, 0n, true);
    });
    render(GC);
    await fireEvent.submit(
      screen.getByRole('button', { name: /preview gc/i }).closest('form')!,
    );
    await tick();

    expect(triggerGC).toHaveBeenCalledTimes(1);
    const [req, opts] = triggerGC.mock.calls[0];
    expect(req.dryRun).toBe(true);
    expect(req.gracePeriodHours).toBe(24);
    expect(req.extraRoots).toEqual([]);
    expect(req.force).toBe(false);
    // AbortController pattern wired through the second arg.
    expect(opts.signal).toBeInstanceOf(AbortSignal);
  });

  it('aborts the stream when Cancel is clicked', async () => {
    let seenSignal: AbortSignal | undefined;
    triggerGC.mockImplementation(async function* (
      _req: unknown,
      opts: { signal?: AbortSignal },
    ) {
      seenSignal = opts.signal;
      yield chunk(10n, 0n, 0n, false);
      // Hang forever; the Cancel click must cut it off.
      await new Promise(() => {});
    });

    render(GC);
    await fireEvent.submit(
      screen.getByRole('button', { name: /preview gc/i }).closest('form')!,
    );
    await tick();
    await Promise.resolve();
    await tick();

    // Running state: Cancel button is visible.
    const cancel = screen.getByTestId('gc-cancel');
    expect(seenSignal?.aborted).toBe(false);

    cancel.click();
    await tick();
    expect(seenSignal?.aborted).toBe(true);
  });
});
