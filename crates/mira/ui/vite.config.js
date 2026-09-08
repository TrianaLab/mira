import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

// Fixed output names and no code splitting: the built files are listed by name
// in a const table in `src/ui.rs`, and content hashes in the filename would mean
// regenerating that table on every build. Freshness is handled by an ETag over
// the bytes instead, which is the same guarantee without the coupling.
export default defineConfig({
  plugins: [svelte()],
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    assetsDir: '',
    // Every byte here ends up inside the binary via include_bytes!, so the
    // sourcemap and the legacy polyfill chunk are not free.
    sourcemap: false,
    modulePreload: { polyfill: false },
    rollupOptions: {
      output: {
        inlineDynamicImports: true,
        entryFileNames: 'app.js',
        assetFileNames: 'app.[ext]',
      },
    },
  },
})
