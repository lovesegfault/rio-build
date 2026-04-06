import { render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

const { clearPoison } = vi.hoisted(() => ({ clearPoison: vi.fn() }));
vi.mock('../../api/admin', () => ({ admin: { clearPoison } }));

import ClearPoisonButton from '../ClearPoisonButton.svelte';

describe('ClearPoisonButton', () => {
  beforeEach(() => {
    vi.useFakeTimers();
    vi.stubGlobal('confirm', vi.fn(() => true));
  });
  afterEach(() => {
    vi.unstubAllGlobals();
    clearPoison.mockReset();
    vi.useRealTimers();
  });

  it('renders only when poisoned', () => {
    const { rerender } = render(ClearPoisonButton, {
      props: { derivationHash: 'abc', poisoned: false },
    });
    expect(screen.queryByTestId('clear-poison-btn')).not.toBeInTheDocument();
    rerender({ derivationHash: 'abc', poisoned: true });
    expect(screen.getByTestId('clear-poison-btn')).toBeInTheDocument();
  });

  it('fires onCleared callback on success', async () => {
    clearPoison.mockResolvedValue({ cleared: true });
    const onCleared = vi.fn();
    render(ClearPoisonButton, {
      props: { derivationHash: 'abc', poisoned: true, onCleared },
    });

    screen.getByTestId('clear-poison-btn').click();
    await tick();
    await vi.advanceTimersByTimeAsync(0);

    expect(clearPoison).toHaveBeenCalledWith({ derivationHash: 'abc' });
    expect(onCleared).toHaveBeenCalledTimes(1);
  });

  it('does not fire onCleared when server says not-poisoned', async () => {
    // cleared=false → the derivation wasn't actually poisoned (race with
    // a retry that cleared it, or stale UI). No refetch needed.
    clearPoison.mockResolvedValue({ cleared: false });
    const onCleared = vi.fn();
    render(ClearPoisonButton, {
      props: { derivationHash: 'abc', poisoned: true, onCleared },
    });

    screen.getByTestId('clear-poison-btn').click();
    await tick();
    await vi.advanceTimersByTimeAsync(0);

    expect(onCleared).not.toHaveBeenCalled();
  });

  it('does not fire onCleared on error', async () => {
    clearPoison.mockRejectedValue(new Error('unavailable'));
    const onCleared = vi.fn();
    render(ClearPoisonButton, {
      props: { derivationHash: 'abc', poisoned: true, onCleared },
    });

    screen.getByTestId('clear-poison-btn').click();
    await tick();
    await vi.advanceTimersByTimeAsync(0);

    expect(onCleared).not.toHaveBeenCalled();
  });
});
