import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'
import tailwindcss from '@tailwindcss/vite'
import path from 'path'

// Daemon admin API port is not yet defined — no proxy config until
// janus-daemon exists. Mirrors shadow-ui's shape (see shadow-ui/vite.config.ts)
// once there's a real backend to point at.
export default defineConfig({
  plugins: [react(), tailwindcss()],
  resolve: {
    alias: {
      '@': path.resolve(import.meta.dirname, './src'),
    },
  },
})
