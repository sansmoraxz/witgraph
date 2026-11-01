import { defineConfig } from 'vite';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  root: '.',
  plugins: [tailwindcss()],

  server: {
    fs: {
      allow: ['../..'],
    },
  },

  optimizeDeps: {
    exclude: ['witgraph-loader', 'witgraph-editor'],
  },
});
