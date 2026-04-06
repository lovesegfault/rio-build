import { render, screen } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';

import Toast from '../Toast.svelte';
import { toast } from '../../lib/toast';

describe('Toast', () => {
  beforeEach(() => {
    vi.useFakeTimers();
  });
  afterEach(() => {
    // Flush any pending auto-dismiss timers so the store is empty for
    // the next test (the writable<ToastMsg[]> is module-level state).
    vi.runAllTimers();
    vi.useRealTimers();
  });

  it('renders pushed toasts and auto-dismisses after ttl', async () => {
    render(Toast);
    expect(screen.queryAllByTestId('toast')).toHaveLength(0);

    toast.info('hello');
    await tick();
    expect(screen.getByTestId('toast')).toHaveTextContent('hello');

    // Auto-dismiss default is 4s.
    vi.advanceTimersByTime(4000);
    await tick();
    expect(screen.queryAllByTestId('toast')).toHaveLength(0);
  });

  it('dismisses on close-button click', async () => {
    render(Toast);
    toast.error('boom');
    await tick();
    expect(screen.getByTestId('toast')).toHaveTextContent('boom');

    screen.getByRole('button', { name: 'dismiss' }).click();
    await tick();
    expect(screen.queryAllByTestId('toast')).toHaveLength(0);
  });
});
