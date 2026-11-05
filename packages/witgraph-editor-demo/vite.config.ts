import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

export default defineConfig({
  root: '.',
  // Relative, so the built page works wherever it is served from: GitHub Pages
  // puts it under /<repository>/.
  base: './',
  plugins: [tailwindcss()],
  // The graph runs in a module worker.
  worker: { format: 'es' },

  server: {
    fs: {
      allow: ['../..'],
    },
    // Debugging source lines needs shared memory, which only an isolated page has.
    headers: {
      'Cross-Origin-Opener-Policy': 'same-origin',
      'Cross-Origin-Embedder-Policy': 'require-corp',
    },
  },

  optimizeDeps: {
    exclude: ['witgraph-loader', 'witgraph-editor'],
    // Found only as they start otherwise, which then reloads the page under what is running.
    entries: ['**/*.html', 'src/session-worker.ts', 'test/lines/worker.ts'],
  },
});
