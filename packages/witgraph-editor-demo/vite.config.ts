import tailwindcss from '@tailwindcss/vite';
import { defineConfig } from 'vite';

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
