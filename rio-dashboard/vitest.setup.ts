// Extends vitest's expect() with jest-dom matchers (toBeInTheDocument etc).
// Separate entrypoint so pnpm's --ignore-scripts has nothing to break here.
import '@testing-library/jest-dom/vitest';
import { vi } from 'vitest';

// jsdom doesn't implement scrollTo; svelte-routing calls it on every route
// activation. The default jsdom behavior logs "Not implemented" to stderr
// (noisy, not a failure). Stub it globally so CI logs stay readable.
window.scrollTo = vi.fn();
