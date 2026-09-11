import { defineConfig } from 'vite'
import { svelte } from '@sveltejs/vite-plugin-svelte'

// Fixed output names and no code splitting: the built files are listed by name
// in a const table in `src/ui.rs`, and content hashes in the filename would mean
// regenerating that table on every build. Freshness is handled by an ETag over
// the bytes instead, which is the same guarantee without the coupling.
// `VITE_REPLAY=1 npm run build -- --outDir dist-demo` builds the recorded
// snapshot the documentation site hosts instead of the one the binary embeds.
// A `define` and not an env read in the source, so the ordinary build folds it
// to `false` and rollup drops `lib/replay.js` and the fixture file with it —
// `import.meta.env` would survive as a property read and carry both into the
// bundle that ends up inside `mira`.
export default defineConfig({
  plugins: [svelte()],
  define: { __REPLAY__: JSON.stringify(!!process.env.VITE_REPLAY) },
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
