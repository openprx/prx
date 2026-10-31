import { fileURLToPath } from 'node:url';
import { defineConfig } from 'vitest/config';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import { svelteTesting } from '@testing-library/svelte/vite';

const svelteClient = fileURLToPath(new URL('./node_modules/svelte/src/index-client.js', import.meta.url));

export default defineConfig({
  plugins: [
    svelte({
      compilerOptions: {
        runes: true
      }
    }),
    svelteTesting()
  ],
  resolve: {
    alias: [{ find: /^svelte$/, replacement: svelteClient }],
    conditions: ['browser']
  },
  test: {
    environment: 'jsdom',
    include: ['src/**/*.test.js'],
    setupFiles: ['src/test/setup.js']
  }
});
