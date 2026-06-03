// LogViewer jsdom-testable paths: error-alert, empty-state, spinner.
//
// Follow-tail scroll behavior is NOT testable here — jsdom layout is
// all-zeros (scrollHeight === scrollTop === 0) so the `follow`
// heuristic is a no-op under vitest. That path is covered manually.
//
// These three tests cover the conditional rendering branches at
// LogViewer.svelte:63-75: {#if stream.err}, {#each stream.lines},
// {#if !stream.done}/{:else if empty}. A refactor that broke any
// branch would be caught here before a runtime surprise.

import { render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';

// Mock the log stream factory so we can control `lines`/`done`/`err`
// independently of any RPC wiring. Each test stubs `createLogStream`
// to return a specific shape; LogViewer renders off those reactive
// getters directly.
const { createLogStream } = vi.hoisted(() => ({ createLogStream: vi.fn() }));
vi.mock('../../lib/logStream.svelte', () => ({ createLogStream }));

// Importing after the mock so LogViewer picks up the stubbed factory.
import LogViewer from '../LogViewer.svelte';

// Monomorphic row constructors mirroring logStream's LogRow shape.
function lineRows(texts: string[]) {
  return texts.map((text) => ({ kind: 'line', text, from: 0n, until: 0n }));
}
function gapRow(from: bigint, until: bigint) {
  return { kind: 'gap', text: '', from, until };
}

describe('LogViewer', () => {
  afterEach(() => {
    createLogStream.mockReset();
  });

  it('renders error alert when stream.err is set', () => {
    createLogStream.mockReturnValue({
      rows: [],
      done: true,
      err: new Error('boom'),
      destroy: vi.fn(),
    });
    render(LogViewer, { props: {} });

    // {#if stream.err} branch: <div role="alert">log stream failed:
    // {stream.err.message}</div>. Using the ARIA role — a future
    // className refactor shouldn't break the test.
    const alert = screen.getByRole('alert');
    expect(alert.textContent).toContain('log stream failed: boom');

    // With err set and done=true, the empty-state branch is guarded
    // by `&& !stream.err` — assert it does NOT render.
    expect(screen.queryByText('no log output')).toBeNull();
  });

  it('renders empty-state when done && lines empty && no err', () => {
    createLogStream.mockReturnValue({
      rows: [],
      done: true,
      err: null,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: {} });

    // {:else if stream.lines.length === 0 && !stream.err} branch.
    expect(screen.getByText('no log output')).toBeInTheDocument();
    // Spinner is in the {#if !stream.done} branch — absent here.
    expect(screen.queryByTestId('log-tail')).toBeNull();
  });

  it('shows spinner while stream.done is false', () => {
    // {#if !stream.done} branch: spinner + "streaming…" text present
    // even with some lines already rendered.
    createLogStream.mockReturnValue({
      rows: lineRows(['line1', 'line2']),
      done: false,
      err: null,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: {} });

    expect(screen.getByTestId('log-tail')).toBeInTheDocument();
    expect(screen.getByText(/streaming/)).toBeInTheDocument();
    // Lines render alongside the spinner — they're not gated on done.
    expect(screen.getByText('line1')).toBeInTheDocument();
    expect(screen.getByText('line2')).toBeInTheDocument();
  });

  it('hides spinner once stream.done is true with lines present', () => {
    // Complement to the above: done=true with non-empty lines →
    // neither spinner nor empty-state renders.
    createLogStream.mockReturnValue({
      rows: lineRows(['final line']),
      done: true,
      err: null,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: {} });

    expect(screen.queryByTestId('log-tail')).toBeNull();
    // Non-empty lines → empty-state branch also absent.
    expect(screen.queryByText('no log output')).toBeNull();
    expect(screen.getByText('final line')).toBeInTheDocument();
    // The mock omits `incomplete` (undefined → falsy): a complete log
    // must not render the incomplete banner.
    expect(screen.queryByTestId('log-incomplete')).toBeNull();
  });

  it('renders windowed subset, not all lines (virtualized)', () => {
    // jsdom's layout is all-zeros (scrollTop, clientHeight, scrollHeight
    // are all 0), so we can't drive the scroll arithmetic. But with
    // both at 0 the viewport math resolves to {start:0, end:min(n,
    // 2*OVERSCAN)} = {start:0, end:20} — so a 5000-line stream renders
    // exactly 20 <pre> nodes. That's the structural assertion: the
    // {#each} is bounded by viewport math, not by lines.length.
    //
    // A regression back to {#each stream.lines as line} would render
    // 5000 nodes here. We assert <100 (way under 5000) and >0 (not
    // accidentally empty) to tolerate OVERSCAN tuning without tying
    // the test to the exact constant.
    const lines = Array.from({ length: 5000 }, (_, i) => `L${i}`);
    createLogStream.mockReturnValue({
      rows: lineRows(lines),
      done: true,
      err: null,
      truncated: false,
      destroy: vi.fn(),
    });
    const { container } = render(LogViewer, { props: {} });

    const pres = container.querySelectorAll('pre.line');
    expect(pres.length).toBeLessThan(100);
    expect(pres.length).toBeGreaterThan(0);
    // With scrollTop=0 the visible slice starts at line 0.
    expect(pres[0].textContent).toBe('L0');
  });

  it('honors viewportOverride prop for slice-bounds testing', () => {
    // The test-only viewportOverride prop lets us assert the slice
    // boundaries directly — jsdom can't drive scrollTop so the prod
    // path's arithmetic is opaque here, but the override exercises the
    // same $derived + {#each lines.slice(start, end)} rendering.
    const lines = Array.from({ length: 1000 }, (_, i) => `L${i}`);
    createLogStream.mockReturnValue({
      rows: lineRows(lines),
      done: true,
      err: null,
      truncated: false,
      destroy: vi.fn(),
    });
    const { container } = render(LogViewer, {
      props: {
                _viewportOverride: { start: 400, end: 430 },
      },
    });

    const pres = container.querySelectorAll('pre.line');
    expect(pres.length).toBe(30);
    expect(pres[0].textContent).toBe('L400');
    expect(pres[29].textContent).toBe('L429');
    // Spacers are present for the off-screen ranges. Their inline
    // height is computed from (start × LINE_H) and
    // ((n - end) × LINE_H). We don't assert exact px (LINE_H is an
    // implementation constant) but we do assert both spacers exist
    // with nonzero height when the window is mid-stream.
    const spacers = container.querySelectorAll('.spacer');
    expect(spacers.length).toBe(2);
    const [top, bottom] = spacers;
    expect((top as HTMLElement).style.height).not.toBe('0px');
    expect((bottom as HTMLElement).style.height).not.toBe('0px');
  });

  it('renders truncation banner when stream.truncated is set', () => {
    // logStream flips `truncated` after dropping the oldest 10K lines
    // at the 50K cap. The banner tells the user the head of the log is
    // gone — not an error, just a memory cap. Asserting testid rather
    // than the exact em-dash string so the copy can change freely.
    createLogStream.mockReturnValue({
      rows: lineRows(['tail line']),
      done: true,
      err: null,
      truncated: true,
      droppedLines: 12340,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: {} });

    expect(screen.getByTestId('log-truncated')).toBeInTheDocument();
    expect(screen.getByText(/12,340 earlier lines truncated/)).toBeInTheDocument();
  });

  // r[verify obs.log.incomplete-surfaced+2]
  it('renders incomplete banner when stream.incomplete is set', () => {
    // logStream flips `incomplete` when the server stream ends without an
    // is_complete=true terminal chunk — a `.partial` blob or a
    // still-running build. The banner warns that the tail (usually the
    // build error) may be missing. Asserting the testid rather than the
    // exact copy, same as the truncation test.
    createLogStream.mockReturnValue({
      rows: lineRows(['some line']),
      done: true,
      err: null,
      truncated: false,
      incomplete: true,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: {} });

    expect(screen.getByTestId('log-incomplete')).toBeInTheDocument();
  });

  it('derives lineH from computed style (a11y: non-16px root)', () => {
    // rev-p392 correctness: lineH is measured, not hardcoded. jsdom's
    // layout returns "" for lineHeight → 20px fallback. Stub
    // getComputedStyle to return a non-16px-root value and assert the
    // spacer math follows. The prior const LINE_H = 20 broke when a
    // user's browser zoom or a11y setting changed the root font: CSS
    // lines scaled to 25px, spacers stayed at 20px × N — viewport math
    // sliced the wrong window.
    const orig = window.getComputedStyle;
    // Narrow stub: forward to the real implementation but override
    // lineHeight. Spread-then-cast keeps the return shape close enough
    // for parseFloat(lineHeight) — the component doesn't touch other
    // properties from this call.
    window.getComputedStyle = ((
      ...args: Parameters<typeof orig>
    ): CSSStyleDeclaration =>
      ({
        ...orig(...args),
        lineHeight: '25px',
      }) as CSSStyleDeclaration) as typeof window.getComputedStyle;
    try {
      createLogStream.mockReturnValue({
        rows: lineRows(Array.from({ length: 100 }, (_, i) => `L${i}`)),
        done: true,
        err: null,
        truncated: false,
        destroy: vi.fn(),
      });
      const { container } = render(LogViewer, {
        props: { _viewportOverride: { start: 10, end: 20 } },
      });
      const spacers = container.querySelectorAll('.spacer');
      const top = spacers[0] as HTMLElement;
      const bottom = spacers[1] as HTMLElement;
      // start=10 × lineH=25 → 250px top spacer (not 200px).
      // (100 - end=20) × lineH=25 → 2000px bottom spacer (not 1600px).
      // Both prove the measured value feeds the spacer arithmetic.
      expect(top.style.height).toBe('250px');
      expect(bottom.style.height).toBe('2000px');
    } finally {
      window.getComputedStyle = orig;
    }
  });

  it('renders gap rows inline with the missing range named', () => {
    // merged_bug_306 dashboard half: a forward jump in the served
    // stream renders as an explicit marked row between the lines —
    // never a seamless splice. The range is inclusive in the copy
    // (lines 2–9 == [2, 10)).
    createLogStream.mockReturnValue({
      rows: [...lineRows(['l0', 'l1']), gapRow(2n, 10n), ...lineRows(['l10'])],
      done: true,
      err: null,
      truncated: false,
      incomplete: false,
      gapCount: 1,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: {} });

    const gap = screen.getByTestId('log-gap');
    expect(gap.textContent).toContain('lines 2–9 missing');
    // The interior-gap banner names the count; the missing-tail banner
    // stays absent (the log is complete apart from the interior hole).
    expect(screen.getByTestId('log-gap-banner')).toBeInTheDocument();
    expect(screen.queryByTestId('log-incomplete')).toBeNull();
  });

  it('splits the banner copy: interior gaps vs missing tail', () => {
    // Both failure stories at once: interior holes AND an incomplete
    // tail. Each banner names its own story — the user can tell "lines
    // are missing in the middle" from "the end never made it".
    createLogStream.mockReturnValue({
      rows: [...lineRows(['l0']), gapRow(1n, 4n), ...lineRows(['l4'])],
      done: true,
      err: null,
      truncated: false,
      incomplete: true,
      gapCount: 1,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: {} });

    expect(screen.getByTestId('log-gap-banner')).toBeInTheDocument();
    const tail = screen.getByTestId('log-incomplete');
    expect(tail.textContent).toContain('missing tail');
  });
});
