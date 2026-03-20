// Optimistic state transition: the button flips status→draining before
// the RPC resolves, keeps it on success, and reverts on error. These
// assertions pin down that the Svelte 5 $bindable() roundtrip actually
// mutates the parent-owned array (the rune proxy is deep, so an in-place
// `workers[idx].status = ...` is reactive without reassigning the array).
import { render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { drainWorker } = vi.hoisted(() => ({ drainWorker: vi.fn() }));
vi.mock('../../api/admin', () => ({ admin: { drainWorker } }));

import DrainHarness, { setSeed } from './DrainHarness.svelte';

describe('DrainButton', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    // jsdom's confirm() is a real prompt; stub it to auto-accept so the
    // click path proceeds past the guard.
    vi.stubGlobal('confirm', vi.fn(() => true));
  });

  afterEach(() => {
    vi.unstubAllGlobals();
    drainWorker.mockReset();
    vi.useRealTimers();
  });

  function workers() {
    return [
      { workerId: 'w-1', status: 'alive' },
      { workerId: 'w-2', status: 'alive' },
    ];
  }

  it('optimistically flips to draining before the RPC resolves', async () => {
    let resolve!: (v: unknown) => void;
    drainWorker.mockReturnValue(new Promise((r) => (resolve = r)));

    setSeed(workers());
    render(DrainHarness, { props: { workerId: 'w-1' } });
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('alive');

    screen.getByTestId('drain-btn').click();
    await tick();

    // Optimistic: status flipped BEFORE the promise resolved.
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('draining');
    expect(drainWorker).toHaveBeenCalledWith({
      workerId: 'w-1',
      force: false,
    });

    // Resolve → status stays.
    resolve({ accepted: true, runningBuilds: 0 });
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('draining');
    // Other rows untouched.
    expect(screen.getByTestId('status-w-2')).toHaveTextContent('alive');
  });

  it('reverts on error', async () => {
    let reject!: (e: unknown) => void;
    drainWorker.mockReturnValue(new Promise((_, r) => (reject = r)));

    setSeed(workers());
    render(DrainHarness, { props: { workerId: 'w-1' } });
    screen.getByTestId('drain-btn').click();
    await tick();
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('draining');

    reject(new Error('scheduler 503'));
    await vi.advanceTimersByTimeAsync(0);
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('alive');
  });

  it('does nothing when confirm() declines', async () => {
    vi.stubGlobal('confirm', vi.fn(() => false));
    drainWorker.mockResolvedValue({ accepted: true, runningBuilds: 0 });

    setSeed(workers());
    render(DrainHarness, { props: { workerId: 'w-1' } });
    screen.getByTestId('drain-btn').click();
    await tick();

    expect(drainWorker).not.toHaveBeenCalled();
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('alive');
  });

  it('disables for non-alive workers', () => {
    setSeed([{ workerId: 'w-1', status: 'draining' }]);
    render(DrainHarness, { props: { workerId: 'w-1' } });
    expect(screen.getByTestId('drain-btn')).toBeDisabled();
  });
});
