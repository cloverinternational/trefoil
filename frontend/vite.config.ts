import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Dev proxy targets the Rust backend so the frontend can use relative
// /v0 and /v1 paths identically to the Plexus reference.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  // css.include: let src/surfaces.guard.test.ts read globals.css?raw (vitest stubs CSS otherwise).
  test: { exclude: ['e2e/**', 'tests/e2e/**', 'node_modules/**'], css: { include: [/globals\.css/] } },
  server: {
    proxy: {
      '/v0': 'http://127.0.0.1:8080',
      '/v1': 'http://127.0.0.1:8080',
    },
  },
});
