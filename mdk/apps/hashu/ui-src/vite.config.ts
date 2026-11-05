// Builds index.tsx to ../ui/index.js (+ ../ui/style.css) for the MDK dashboard.
// Run from the app root with `npm run build:ui`.
//
// React and the MDK packages stay external: the dashboard lends the app its own
// instances through an import map, so the app ships none of them (a second
// React would break hooks).
import { MDK_APP_SHARED_MODULES } from '@tetherto/mdk-ui-foundation/app-shared-modules'
import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

export default defineConfig({
  plugins: [react()],
  define: { 'process.env.NODE_ENV': JSON.stringify('production') },
  build: {
    // The app package ships ui/, not this source: build one level up.
    outDir: '../ui',
    emptyOutDir: true,
    lib: {
      entry: 'index.tsx',
      formats: ['es'],
      fileName: () => 'index.js',
      cssFileName: 'style',
    },
    rollupOptions: { external: [...MDK_APP_SHARED_MODULES] },
  },
})
