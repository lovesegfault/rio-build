# Plan 988499302: Dashboard eslint no-restricted-imports — ban gen/*_pb outside src/api/

rev-p390 finding at [`rio-dashboard/eslint.config.js:29`](../../rio-dashboard/eslint.config.js). [P0390](plan-0390-dashboard-gen-pb-barrel.md) establishes the `api/types.ts` barrel convention: consumers import from `../api/types`, only `src/api/*` reaches into `../gen/*_pb`. The convention is currently grep-enforced only (P0390's exit criterion: `grep -r "from.*gen/.*_pb" rio-dashboard/src | grep -v 'src/api/'` → 0). A future 8th consumer can reach straight into `gen/` with no lint signal — the grep criterion only catches it at P0390's implementer, never again.

`no-restricted-imports` with a pattern makes the convention lint-enforced. ESLint 9 flat config supports pattern-based restrictions with file-level overrides. One config stanza: restrict `**/gen/*_pb` everywhere, override-allow for `src/api/**`.

## Entry criteria

- [P0390](plan-0390-dashboard-gen-pb-barrel.md) merged (`api/types.ts` barrel exists; all 7 consumers migrated to `../api/types`)

## Tasks

### T1 — `feat(dashboard):` eslint.config.js — no-restricted-imports gen/*_pb pattern

MODIFY [`rio-dashboard/eslint.config.js`](../../rio-dashboard/eslint.config.js) after the existing rules block at `:28-34`. Add a pattern-restrict + file-override pair:

```javascript
export default ts.config(
  // ... existing config ...
  {
    rules: {
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
    // barrel's job to re-export. Test-support (admin-mock.ts) also
    // needs gen/admin_pb for vi.mock() stubbing.
    files: ['src/api/**', 'src/test-support/**'],
    rules: {
      'no-restricted-imports': 'off',
    },
  },
);
```

**CARE — test-support exemption:** [`src/test-support/admin-mock.test.ts:11`](../../rio-dashboard/src/test-support/admin-mock.test.ts) imports `AdminService` from `../gen/admin_pb` for the `vi.mock` verification. That import is LEGITIMATE — it's testing the barrel mechanism, not consuming it. Include `src/test-support/**` in the exemption OR migrate that one site to `../api/admin` (which re-exports `AdminService` via the client singleton — check at dispatch whether `api/admin.ts` re-exports the service descriptor or only the client; if only the client, the test-support exemption stays).

**CARE — pattern matching:** `**/gen/*_pb` matches `'../gen/admin_types_pb'` and `'../../gen/admin_types_pb'` (depth-independent). The `.js` variant catches consumers that accidentally import the emitted JS instead of the TS (unlikely but protobuf-es emits both in some configs). If ESLint's `patterns.group` glob doesn't match relative imports the way `paths` does, use `paths: [{ name: '../gen/admin_types_pb', ... }, ...]` with explicit enumeration instead — check ESLint docs at dispatch.

### T2 — `test(dashboard):` lint regression — add stray gen/ import, confirm lint fails

Manual smoke-test during impl (no committed test-file — eslint itself IS the test):

```bash
# Add a stray gen/ import to a consumer
echo "import type { WorkerInfo } from '../gen/admin_types_pb';" >> rio-dashboard/src/pages/Cluster.svelte
pnpm --filter rio-dashboard lint  # → error: no-restricted-imports
git checkout -- rio-dashboard/src/pages/Cluster.svelte
```

Document the expected error message in the commit body. The `/nbr .#ci` pre-commit hook runs eslint on staged files — this makes the restriction enforced at commit time, not just at an arbitrary `pnpm lint` invocation.

## Exit criteria

- `grep 'no-restricted-imports' rio-dashboard/eslint.config.js` → ≥2 hits (restrict rule + off-override)
- `pnpm --filter rio-dashboard lint` → 0 errors (existing code is compliant post-P0390)
- Manual T2 smoke: append stray `from '../gen/admin_types_pb'` to any consumer outside `src/api/` → `pnpm lint` fails with "Import from src/api/types (barrel) instead"
- `grep -r "from.*gen/.*_pb" rio-dashboard/src --include='*.svelte' --include='*.ts' | grep -v 'src/api/\|src/test-support/'` → 0 hits (same criterion as P0390, now lint-enforced)
- `/nbr .#ci` green

## Tracey

No markers. Tooling/lint-only change; no spec-observable behavior.

## Files

```json files
[
  {"path": "rio-dashboard/eslint.config.js", "action": "MODIFY", "note": "T1: no-restricted-imports pattern gen/*_pb + files-override src/api/** + src/test-support/** exemption. Lint-enforces P0390 barrel convention."}
]
```

```
rio-dashboard/
└── eslint.config.js     # T1: +no-restricted-imports pattern + exemption override
```

## Dependencies

```json deps
{"deps": [390], "soft_deps": [988499301, 389], "note": "rev-p390 trivial → promoted to P-new (lint-enforcement closes a drift-back hole that grep-criterion can't). HARD-dep P0390: the barrel must EXIST and consumers must be migrated before the restriction can land (otherwise lint fails on the 7 existing consumers). Soft-dep P988499301 (buildInfo.ts extraction — its T1 imports BuildInfo from api/types or gen/admin_types_pb depending on P0390 timing; this eslint rule would catch the wrong choice, sequence-independent). Soft-dep P0389 (admin-mock.ts — the test-support exemption covers it). eslint.config.js count=0 (low-traffic — only P0390-adjacent work touches it). Single-file, additive config stanza — zero conflict surface."}
```

**Depends on:** [P0390](plan-0390-dashboard-gen-pb-barrel.md) — barrel + 7-consumer migration must land first.
**Conflicts with:** none. `eslint.config.js` is a low-traffic single-purpose file; this is the only plan touching it.
