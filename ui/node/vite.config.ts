import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// FLUXVM_DEV_API points the dev proxy at a running daemon (default: the loopback listener).
const api = process.env.FLUXVM_DEV_API || 'http://127.0.0.1:7788';
const upstream = { target: api, secure: false, changeOrigin: true };

// The build is embedded in the daemon and served under /console/ (or any prefix a reverse proxy adds),
// so: relative base, and fixed file names that the Rust side can embed and ETag.
export default defineConfig({
  base: './',
  plugins: [react()],
  server: { port: 5174, proxy: { '/v1': { ...upstream, ws: true }, '/healthz': upstream } },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: false,
    chunkSizeWarningLimit: 600,
    rolldownOptions: {
      output: {
        entryFileNames: 'assets/app.js',
        chunkFileNames: 'assets/[name].js',
        assetFileNames: 'assets/[name][extname]',
      },
    },
  },
});
