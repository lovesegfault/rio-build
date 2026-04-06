// ESLint 9 flat config. Keep permissive for the scaffold — downstream
// plans that add real UI will tighten rules as needed.
import js from '@eslint/js';
import svelte from 'eslint-plugin-svelte';
import globals from 'globals';
import ts from 'typescript-eslint';

export default ts.config(
  // src/gen/ ships `/* eslint-disable */` at top of file so lint would pass
  // anyway — but ignoring means eslint doesn't parse 100KB of types_pb.ts on
  // every run. Pre-commit never sees these (gitignored); this is for local
  // `pnpm run lint` and the sandbox buildPhase.
  { ignores: ['dist/', 'node_modules/', 'src/gen/'] },
  js.configs.recommended,
  ...ts.configs.recommended,
  ...svelte.configs['flat/recommended'],
  {
    languageOptions: {
      globals: { ...globals.browser, ...globals.node },
    },
  },
  {
    files: ['**/*.svelte'],
    languageOptions: {
      parserOptions: { parser: ts.parser },
    },
  },
  {
    rules: {
      // mount(App, { target: document.getElementById('app')! })
      // — scaffold intentionally uses the non-null assertion.
      '@typescript-eslint/no-non-null-assertion': 'off',
    },
  },
);
