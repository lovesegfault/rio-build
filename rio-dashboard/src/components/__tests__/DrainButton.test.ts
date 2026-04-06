// Optimistic state transition: the button flips status→draining before
// the RPC resolves, keeps it on success, and reverts on error. These
// assertions pin down that the Svelte 5 $bindable() roundtrip actually
// mutates the parent-owned array (the rune proxy is deep, so an in-place
// `executors[idx].status = ...` is reactive without reassigning the array).
import { render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import {
  adminMock,
  flushSvelte,
  setupStandardBeforeEach,
  teardownStandardAfterEach,
  toastMock,
} from '../../test-support/admin-mock';

vi.mock('../../api/admin', () => ({ admin: adminMock }));
vi.mock('../../lib/toast', () => ({ toast: toastMock }));

import DrainHarness, {
  getExecutors,
  reassign,
  setSeed,
} from './DrainHarness.svelte';

const { drainExecutor } = adminMock;

describe('DrainButton', () => {
  beforeEach(() => setupStandardBeforeEach());
  afterEach(teardownStandardAfterEach);

  function executors() {
    return [
      { executorId: 'w-1', status: 'alive' },
      { executorId: 'w-2', status: 'alive' },
    ];
  }

  it('optimistically flips to draining before the RPC resolves', async () => {
    let resolve!: (v: unknown) => void;
    drainExecutor.mockReturnValue(new Promise((r) => (resolve = r)));

    setSeed(executors());
    render(DrainHarness, { props: { executorId: 'w-1' } });
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('alive');

    screen.getByTestId('drain-btn').click();
    await tick();

    // Optimistic: status flipped BEFORE the promise resolved.
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('draining');
    expect(drainExecutor).toHaveBeenCalledWith({
      executorId: 'w-1',
      force: false,
    });

    // Resolve → status stays.
    resolve({ accepted: true, runningBuilds: 0 });
    await flushSvelte();
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('draining');
    // Other rows untouched.
    expect(screen.getByTestId('status-w-2')).toHaveTextContent('alive');
  });

  it('reverts on error', async () => {
    let reject!: (e: unknown) => void;
    drainExecutor.mockReturnValue(new Promise((_, r) => (reject = r)));

    setSeed(executors());
    render(DrainHarness, { props: { executorId: 'w-1' } });
    screen.getByTestId('drain-btn').click();
    await tick();
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('draining');

    reject(new Error('scheduler 503'));
    await flushSvelte();
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('alive');
  });

  it('does nothing when confirm() declines', async () => {
    vi.stubGlobal('confirm', vi.fn(() => false));
    drainExecutor.mockResolvedValue({ accepted: true, runningBuilds: 0 });

    setSeed(executors());
    render(DrainHarness, { props: { executorId: 'w-1' } });
    screen.getByTestId('drain-btn').click();
    await tick();

    expect(drainExecutor).not.toHaveBeenCalled();
    expect(screen.getByTestId('status-w-1')).toHaveTextContent('alive');
  });

  it('disables for non-alive executors', () => {
    setSeed([{ executorId: 'w-1', status: 'draining' }]);
    render(DrainHarness, { props: { executorId: 'w-1' } });
    expect(screen.getByTestId('drain-btn')).toBeDisabled();
  });

  it('revert-on-error keys on executorId, not pre-await index', async () => {
    // 3 executors, target is at index 1. A 5s refresh tick lands mid-RPC
    // and removes w-a → target shifts to index 0. The pre-P0377 code
    // would revert stale idx=1 (w-c), not the target.
    let reject!: (e: unknown) => void;
    drainExecutor.mockReturnValue(new Promise((_, r) => (reject = r)));

    setSeed([
      { executorId: 'w-a', status: 'alive' },
      { executorId: 'w-target', status: 'alive' },
      { executorId: 'w-c', status: 'alive' },
    ]);
    render(DrainHarness, { props: { executorId: 'w-target' } });
    screen.getByTestId('drain-btn').click();
    await tick();
    // Optimistic set landed on the correct row (sync path, no race yet).
    expect(screen.getByTestId('status-w-target')).toHaveTextContent(
      'draining',
    );

    // Simulate the parent's refresh() reassigning executors: w-a dropped,
    // positions shift. w-target now at idx=0; stale idx=1 would hit w-c.
    reassign([
      { executorId: 'w-target', status: 'draining' },
      { executorId: 'w-c', status: 'alive' },
    ]);

    reject(new Error('scheduler 503'));
    await flushSvelte();

    // Revert lands on w-target (idx=0 NOW), not w-c (stale idx=1).
    const ws = getExecutors();
    expect(ws[0].executorId).toBe('w-target');
    expect(ws[0].status).toBe('alive'); // reverted
    expect(ws[1].executorId).toBe('w-c');
    expect(ws[1].status).toBe('alive'); // untouched
    // DOM view agrees with the proxy state.
    expect(screen.getByTestId('status-w-target')).toHaveTextContent('alive');
    expect(screen.getByTestId('status-w-c')).toHaveTextContent('alive');
  });

  it('revert is a no-op when executor removed mid-await', async () => {
    let reject!: (e: unknown) => void;
    drainExecutor.mockReturnValue(new Promise((_, r) => (reject = r)));

    setSeed([{ executorId: 'w-gone', status: 'alive' }]);
    render(DrainHarness, { props: { executorId: 'w-gone' } });
    screen.getByTestId('drain-btn').click();
    await tick();

    // Scheduler swept w-gone between click and error. Pre-P0377 code
    // would hit `executors[0].status = ...` on undefined → TypeError.
    reassign([]);
    reject(new Error('already dead'));
    await flushSvelte();

    // No throw (findIndex→-1→no-op); toast.error still fires.
    expect(getExecutors()).toHaveLength(0);
    expect(toastMock.error).toHaveBeenCalledWith(
      expect.stringContaining('w-gone'),
    );
  });
});
