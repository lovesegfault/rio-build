import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  // Svelte 5's package.json uses conditional exports: the default resolves
  // to the SSR build under vitest (node env), which has no `mount`. Force
  // the browser condition so tests get the client build.
  // @testing-library/svelte's svelteTesting() plugin tries to do this but
  // only when 'node' is already in the user-facing resolve.conditions —
  // Vite 6 sets that internally, not in config, so the plugin no-ops.
  resolve: {
    conditions: ['browser'],
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./vitest.setup.ts'],
  },
});
