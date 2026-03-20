import { render, screen } from '@testing-library/svelte';
import { describe, expect, it } from 'vitest';
import BuildStatePill, { STATE_META } from '../BuildStatePill.svelte';

describe('BuildStatePill', () => {
  it('maps every known state to a label', () => {
    // Exhaustive: if the proto grows a state and STATE_META isn't
    // updated, this test won't fail — but the fallback path test below
    // covers the "unknown" rendering. This asserts the current table.
    for (const s of [0, 1, 2, 3, 4, 5]) {
      const { unmount } = render(BuildStatePill, { props: { state: s } });
      expect(screen.getByText(STATE_META[s].label)).toBeInTheDocument();
      unmount();
    }
  });

  it('falls back to unknown for an out-of-range state', () => {
    // Future BUILD_STATE_POISONED = 6 — should render neutral grey,
    // not crash with "Cannot read properties of undefined".
    render(BuildStatePill, { props: { state: 99 } });
    expect(screen.getByText('unknown')).toBeInTheDocument();
  });
});
