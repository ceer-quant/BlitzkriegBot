import { defineConfig } from 'vite'
import vue from '@vitejs/plugin-vue'
import tailwindcss from '@tailwindcss/vite'
import { fileURLToPath, URL } from 'node:url'

// Dev proxy targets the Blitzkrieg gateway (ui_kit_web) serving /api/* on the
// same host; no cross-origin in dev, none needed in prod (same origin).
export default defineConfig({
  base: '/panel/',
  plugins: [vue(), tailwindcss()],
  resolve: {
    alias: {
      '@': fileURLToPath(new URL('./src', import.meta.url)),
    },
  },
  server: {
    port: 51889,
    proxy: {
      '/api': { target: 'http://127.0.0.1:51888', changeOrigin: false },
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    rollupOptions: {
      output: {
        /**
         * Keep the heavy vendor surfaces out of the entry chunk (#781). Pages
         * arrive through `defineAsyncComponent`, so a page chunk only drags in
         * the vendor chunk it actually imports: echarts + zrender for the
         * chart pages, Vue Flow for the blueprint canvas, the icon set for the
         * shell and every page. Vue/Pinia/VueUse form the shared runtime chunk
         * that the shell needs anyway. Anything unmatched stays with its
         * importing chunk (Rollup hoists deps shared by several chunks), so an
         * unlisted library can never silently reassemble into the entry.
         */
        manualChunks(id: string): string | undefined {
          if (!id.includes('node_modules')) return undefined
          // Last `node_modules/` segment: handles pnpm layouts and nested deps.
          const pkg = id.split('node_modules/').pop() ?? id
          if (pkg.startsWith('echarts/') || pkg.startsWith('zrender/')) return 'echarts'
          if (pkg.startsWith('@vue-flow/')) return 'vue-flow'
          if (pkg.startsWith('lucide-vue-next/')) return 'icons'
          if (
            pkg.startsWith('vue/') ||
            pkg.startsWith('@vue/') ||
            pkg.startsWith('pinia/') ||
            pkg.startsWith('@vueuse/') ||
            pkg.startsWith('vue-demi/')
          ) {
            return 'vue-vendor'
          }
          return undefined
        },
      },
    },
  },
})
