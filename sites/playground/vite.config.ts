import { createHash } from 'node:crypto';
import { readFileSync } from 'node:fs';
import { fileURLToPath } from 'node:url';
import { defineConfig, type Plugin } from 'vite';

/** Writes sw.js with the list of built files, so the service worker caches them all at install. */
function serviceWorker(): Plugin {
  return {
    name: 'moul-playground-sw',
    apply: 'build',
    generateBundle(_, bundle) {
      const assets = Object.keys(bundle)
        .filter((f) => !f.endsWith('.map'))
        .map((f) => `/${f}`)
        .sort();
      const version = createHash('sha256').update(assets.join('\n')).digest('hex').slice(0, 12);
      const source = readFileSync(new URL('./sw-template.js', import.meta.url), 'utf8')
        .replace('__ASSETS__', JSON.stringify(assets.filter((f) => f !== '/index.html')))
        .replace('__VERSION__', version);
      this.emitFile({ type: 'asset', fileName: 'sw.js', source });
    },
  };
}

export default defineConfig({
  plugins: [serviceWorker()],
  resolve: {
    alias: {
      // Studio from source, so the Playground and Studio change together.
      '@moul-dev/studio': fileURLToPath(new URL('../../packages/studio/src/element.ts', import.meta.url)),
    },
  },
  build: {
    outDir: 'dist',
    emptyOutDir: true,
    sourcemap: true,
    target: ['chrome123', 'edge123', 'firefox120', 'safari17.5', 'ios17.5'],
    cssTarget: ['chrome123', 'edge123', 'firefox120', 'safari17.5', 'ios17.5'],
  },
  worker: {
    format: 'es',
  },
  optimizeDeps: {
    // The SDK starts its worker with new URL('./worker.js', import.meta.url), which pre-bundling breaks.
    exclude: ['@moul-dev/client', '@moul-dev/studio'],
  },
  server: {
    port: 5175,
  },
});
