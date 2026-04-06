// Extends vitest's expect() with jest-dom matchers (toBeInTheDocument etc).
// Separate entrypoint so pnpm's --ignore-scripts has nothing to break here.
import '@testing-library/jest-dom/vitest';
