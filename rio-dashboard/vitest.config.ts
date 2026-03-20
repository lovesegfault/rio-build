import { defineConfig } from 'vitest/config';
import { svelte, vitePreprocess } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [
    // `configFile: false` + `style: false` — vitest@2 bundles vite@5
    // internally; our workspace installs vite@6. vitePreprocess's style
    // pass calls vite's preprocessCSS, which in the vite@6 plugin resolves
    // against vite@5's PartialEnvironment constructor and throws "Cannot
    // create proxy with a non-object as target or handler". Component
    // <style> blocks are plain CSS (no SCSS/PostCSS), so skipping the
    // style preprocessor under vitest loses nothing — svelte-check + vite
    // build (both vite@6) still process them at build time.
    //
    // configFile:false is required: the svelte plugin deep-merges
    // svelte.config.js's preprocess (which has the style fn) with inline
    // options, so a bare `preprocess:` override doesn't shadow it. Drop
    // this stanza once vitest bumps to vite@6.
    svelte({
      configFile: false,
      preprocess: [vitePreprocess({ style: false })],
    }),
  ],
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
