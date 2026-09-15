import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'

// Dev proxy targets the Blitzkrieg gateway (ui_kit_web) serving /api/* on the
// same host; no cross-origin in dev, none needed in prod (same origin).
export default defineConfig({
  plugins: [vue()],
  server: {
    port: 51889,
    proxy: {
      '/api': { target: 'http://127.0.0.1:51888', changeOrigin: false },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
  },
})
