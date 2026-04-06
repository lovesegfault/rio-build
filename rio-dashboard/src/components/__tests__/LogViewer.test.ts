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

describe('LogViewer', () => {
  afterEach(() => {
    createLogStream.mockReset();
  });

  it('renders error alert when stream.err is set', () => {
    createLogStream.mockReturnValue({
      lines: [],
      done: true,
      err: new Error('boom'),
      destroy: vi.fn(),
    });
    render(LogViewer, { props: { buildId: 'b-err' } });

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
      lines: [],
      done: true,
      err: null,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: { buildId: 'b-empty' } });

    // {:else if stream.lines.length === 0 && !stream.err} branch.
    expect(screen.getByText('no log output')).toBeInTheDocument();
    // Spinner is in the {#if !stream.done} branch — absent here.
    expect(screen.queryByTestId('log-tail')).toBeNull();
  });

  it('shows spinner while stream.done is false', () => {
    // {#if !stream.done} branch: spinner + "streaming…" text present
    // even with some lines already rendered.
    createLogStream.mockReturnValue({
      lines: ['line1', 'line2'],
      done: false,
      err: null,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: { buildId: 'b-tail' } });

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
      lines: ['final line'],
      done: true,
      err: null,
      destroy: vi.fn(),
    });
    render(LogViewer, { props: { buildId: 'b-done' } });

    expect(screen.queryByTestId('log-tail')).toBeNull();
    // Non-empty lines → empty-state branch also absent.
    expect(screen.queryByText('no log output')).toBeNull();
    expect(screen.getByText('final line')).toBeInTheDocument();
  });
});
