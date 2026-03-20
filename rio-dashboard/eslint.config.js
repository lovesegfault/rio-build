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
      // P0390 barrel convention: only src/api/* may import from gen/*_pb.
      // Consumers use ../api/types (re-exported set of actually-consumed
      // proto types). Direct gen/ imports are brittle to protobuf-es
      // codegen config changes; the barrel is the one change-site.
      'no-restricted-imports': ['error', {
        patterns: [{
          group: ['**/gen/*_pb', '**/gen/*_pb.js'],
          message: 'Import from src/api/types (barrel) instead of gen/*_pb directly. See P0390.',
        }],
      }],
    },
  },
  {
    // src/api/* is the ONE allowed importer of gen/*_pb — it's the
    // barrel's job to re-export. Everything else consumes via
    // ../api/types.
    files: ['src/api/**'],
    rules: {
      'no-restricted-imports': 'off',
    },
  },
);
