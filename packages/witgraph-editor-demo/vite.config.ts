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
  },

  optimizeDeps: {
    exclude: ['witgraph-loader', 'witgraph-editor'],
  },
});
