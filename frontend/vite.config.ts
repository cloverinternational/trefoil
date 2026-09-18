import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';
import tailwindcss from '@tailwindcss/vite';

// Dev proxy targets the Rust backend so the frontend can use relative
// /v0 and /v1 paths identically to the Plexus reference.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  test: { exclude: ['e2e/**', 'tests/e2e/**', 'node_modules/**'] },
  server: {
    proxy: {
      '/v0': 'http://127.0.0.1:8080',
      '/v1': 'http://127.0.0.1:8080',
    },
  },
});
