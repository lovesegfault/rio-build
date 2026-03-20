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
  //
  // 'svelte' is listed explicitly: packages like svelte-routing export only
  // {main, types, svelte} — no default/import/browser condition. With this
  // config providing an explicit `conditions` array, vite's default
  // svelte-condition auto-append doesn't kick in, so we include it here.
  resolve: {
    conditions: ['browser', 'svelte'],
  },
  test: {
    environment: 'jsdom',
    globals: true,
    setupFiles: ['./vitest.setup.ts'],
  },
});
