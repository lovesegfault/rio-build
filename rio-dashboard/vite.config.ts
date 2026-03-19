import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';

export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist',
  },
  server: {
    // Dev proxy targets the Envoy sidecar (:8080), NOT the scheduler
    // directly. Envoy handles gRPC-Web → gRPC translation + mTLS to
    // scheduler. See r[dash.envoy.grpc-web-translate]. (USER A1)
    proxy: {
      '/rio.admin.AdminService': {
        target: 'http://localhost:8080',
        changeOrigin: true,
      },
    },
  },
});
