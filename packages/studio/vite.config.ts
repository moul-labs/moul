/// <reference types="vitest/config" />
import { defineConfig } from 'vite';

// `vite` serves index.html for development. `vite build` makes the library: Studio's own code as
// one module that defines <moul-studio>, with its dependencies left to the app's bundler, so the
// app and Studio share one copy of Lit, Moul UI and the SDK (and so one engine).
export default defineConfig({
  build: {
    lib: {
      entry: 'src/element.ts',
      formats: ['es'],
      fileName: () => 'moul-studio.js',
    },
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: true,
    // @moul-dev/web-component themes with light-dark(), which the theme switch drives through
    // `color-scheme`. Older targets would rewrite light-dark() into variables that follow the OS.
    cssTarget: ['chrome123', 'edge123', 'firefox120', 'safari17.5', 'ios17.5'],
    rollupOptions: {
      // Moul UI's design tokens are inlined (they are CSS, which an app's bundler may not take
      // from a dependency); its elements, Lit, TanStack and the SDK stay the app's.
      external: (id) => !id.endsWith('.css?inline') && /^(@moul-dev\/|lit(\/|$)|@lit\/|@tanstack\/)/.test(id),
    },
  },
  optimizeDeps: {
    // The SDK starts its worker with new URL('./worker.js', import.meta.url), which pre-bundling breaks.
    exclude: ['@moul-dev/client'],
  },
  server: {
    port: 5174,
  },
  test: {
    // Moul UI's token sheet is read as text by a test; Vitest leaves CSS empty unless told.
    css: { include: [/web-component\/dist\/assets\/style\.css/] },
  },
});
