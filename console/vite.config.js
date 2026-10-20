import { defineConfig } from 'vite';
import { svelte } from '@sveltejs/vite-plugin-svelte';
import tailwindcss from '@tailwindcss/vite';

export default defineConfig({
  // Escaped strings retain significant whitespace without trailing literal
  // whitespace in generated JavaScript. License comment text is retained.
  esbuild: { supported: { 'template-literal': false } },
  plugins: [
    svelte({
      compilerOptions: {
        runes: true
      }
    }),
    tailwindcss(),
    {
      name: 'normalize-generated-whitespace',
      generateBundle(_options, bundle) {
        for (const output of Object.values(bundle)) {
          if (output.type === 'chunk') output.code = output.code.replace(/[ \t]+$/gm, '');
        }
      }
    }
  ]
});
