import { render, screen } from '@testing-library/svelte';
import { describe, expect, it, vi } from 'vitest';

// svelte-routing reads window.location at Router-mount time. jsdom's
// location is 'about:blank' by default, which the router parses into a
// non-existent route — prime it to '/' before App imports Cluster (which
// would otherwise fire a real fetch against jsdom's no-op network).
// The admin-client mock short-circuits the network call so the Cluster
// page resolves to its loading/error/data state without a transport.
vi.mock('./api/admin', () => ({
  admin: { clusterStatus: vi.fn().mockResolvedValue({}) },
}));

import App from './App.svelte';

describe('App', () => {
  it('renders the shell nav', () => {
    render(App);
    // Nav heading persists across all routes; the scaffold test previously
    // asserted the same text on the placeholder root — same assertion
    // still holds, now proving the router mounted without throwing.
    expect(screen.getByText('rio-dashboard')).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Cluster' })).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Builds' })).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'Executors' })).toBeInTheDocument();
    expect(screen.getByRole('link', { name: 'GC' })).toBeInTheDocument();
  });
});
