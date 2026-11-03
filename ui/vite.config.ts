import { defineConfig } from 'vitest/config'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'

// The daemon's UI listener serves `dist/` at its root (with an SPA fallback
// to index.html) and the admin API at `/v1` on the same origin, so the
// default base is `/`. JANUS_UI_BASE builds for a different mount point
// (for example `/janus/` behind a reverse proxy); the API base follows it
// unless VITE_JANUS_API_BASE overrides it (see src/config.ts).
const base = process.env.JANUS_UI_BASE ?? '/'

// `npm run dev` against a running daemon: JANUS_ADMIN_URL=http://127.0.0.1:<ui port>
// proxies `/v1` there. The daemon answers no CORS preflight, so the dev
// server has to be same-origin with the API, exactly like production.
const adminUrl = process.env.JANUS_ADMIN_URL

export default defineConfig({
  base,
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      '@': path.resolve(import.meta.dirname, './src'),
    },
  },
  server: adminUrl
    ? { proxy: { [`${base.replace(/\/$/, '')}/v1`]: { target: adminUrl, changeOrigin: false } } }
    : {},
  test: {
    environment: 'jsdom',
    setupFiles: ['./src/test/setup.ts'],
    restoreMocks: true,
    unstubGlobals: true,
  },
})
