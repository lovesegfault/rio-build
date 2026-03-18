# Plan 0274: pnpm/Vite/Svelte scaffold + nix/dashboard.nix (BUILD-CHAIN PROOF)

**USER A7: Svelte 5, NOT React.** `pnpm create vite rio-dashboard --template svelte-ts`. Runes (`$state`/`$effect`) instead of hooks. `@xyflow/svelte` instead of `@xyflow/react`. ~3KB runtime vs ~45KB React+ReactDOM. R6 (500+ node freeze) is LESS of a risk — Svelte's compile-time reactivity avoids VDOM reconciliation.

Proves `fetchPnpmDeps` + `vite build` produce non-empty `dist/` in sandbox. Clones [`nix/tracey.nix:32-68`](../../nix/tracey.nix) with one deviation: we run `svelte-check` in buildPhase (tracey skipped `tsc` due to `@typescript/native-preview` postinstall trap at `:53-56` — we use stock `typescript`, no postinstall).

## Entry criteria

none — Wave 0a root. Parallel with [P0273](plan-0273-envoy-sidecar-grpc-web.md), [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md).

## Tasks

### T1 — `feat(dashboard):` Svelte scaffold

NEW `rio-dashboard/`:
- `package.json` — **audit every dep for postinstall before lockfile.** Runtime: `svelte@5`, `@connectrpc/connect`, `@connectrpc/connect-web`, `@bufbuild/protobuf`. Dev: `typescript@5` (NOT native-preview), `vite@6`, `@sveltejs/vite-plugin-svelte`, `svelte-check`, `vitest@2`, `@testing-library/svelte`, `jsdom`, `eslint@9`, `eslint-plugin-svelte`. Scripts: `build: "svelte-check --tsconfig ./tsconfig.json && vite build"`, `test: "vitest run"`, `lint: "eslint src --max-warnings 0"`, `dev: "vite"`.
- `pnpm-lock.yaml` — via `nix develop -c bash -c 'cd rio-dashboard && pnpm install --lockfile-only'`
- `tsconfig.json` — `strict: true`, `moduleResolution: "bundler"`, `target: "ES2022"`, `paths: {"@/*": ["./src/*"]}`, extends `@tsconfig/svelte`
- `svelte.config.js` — `vitePreprocess()`
- `vite.config.ts` — `plugins: [svelte()]`, `build.outDir: 'dist'`, `server.proxy: { '/rio.admin.AdminService': { target: 'http://localhost:8080', changeOrigin: true } }` (dev proxies to Envoy, not scheduler — USER A1)
- `vitest.config.ts` — `environment: 'jsdom'`, `globals: true`
- `index.html` — `<div id="app"></div>` + `<script type="module" src="/src/main.ts">`
- `src/main.ts` — `import App from './App.svelte'; mount(App, { target: document.getElementById('app')! });`
- `src/App.svelte` — `<script lang="ts"></script><h1>rio-dashboard</h1>`
- `src/App.test.ts` — `render(App); expect(screen.getByText('rio-dashboard')).toBeInTheDocument()`
- `.gitignore` — `node_modules`, `dist`

### T2 — `feat(nix):` dashboard.nix

NEW [`nix/dashboard.nix`](../../nix/dashboard.nix):

```nix
{ pkgs }:
let
  src = pkgs.lib.cleanSource ../.;  # cleanSource NOT cleanCargoSource (drops .ts/.svelte)
  dashboardRoot = "rio-dashboard";
in
pkgs.stdenvNoCC.mkDerivation rec {
  pname = "rio-dashboard";
  version = "0.1.0";
  inherit src;
  sourceRoot = "source/${dashboardRoot}";  # crane convention, tracey.nix:36

  pnpmDeps = pkgs.fetchPnpmDeps {
    inherit pname version src;
    sourceRoot = "source/${dashboardRoot}";
    pnpm = pkgs.pnpm_10;
    fetcherVersion = 3;
    hash = pkgs.lib.fakeHash;  # replace after first /nbr
  };

  nativeBuildInputs = with pkgs; [ nodejs pnpm_10 pnpmConfigHook ];

  # Unlike tracey.nix:57-61 — svelte-check + lint + test in sandbox.
  buildPhase = ''
    runHook preBuild
    pnpm run lint
    pnpm run test
    pnpm run build   # = svelte-check && vite build
    runHook postBuild
  '';

  installPhase = ''
    runHook preInstall
    cp -r dist $out
    test -f $out/index.html
    test -n "$(ls $out/assets/*.js 2>/dev/null)"
    runHook postInstall
  '';
}
```

### T3 — `feat(flake):` wire dashboard check

MODIFY [`flake.nix`](../../flake.nix) — 4 additive edits:
1. Near tracey import: `rioDashboard = import ./nix/dashboard.nix { inherit pkgs; };`
2. `checks` attrset: `dashboard = rioDashboard;`
3. `ciParts` list: add dashboard check
4. Dev shell `nativeBuildInputs`: add `nodejs`, `pnpm_10`

### T4 — `feat(git):` ignore node_modules/dist

MODIFY `.gitignore` — add `rio-dashboard/node_modules/`, `rio-dashboard/dist/`.

### T5 — `feat(nix):` fakeHash cycle

First `/nbr .#checks.x86_64-linux.dashboard` fails with hash mismatch → paste real hash into `nix/dashboard.nix`. Per memory `tracey-adoption.md`: "bump = ONE fakeHash (pnpmDeps)."

## Exit criteria

- `/nbr .#checks.x86_64-linux.dashboard` → `result/index.html` contains `<div id="app">`
- `ls result/assets/*.js` non-empty
- `/nbr .#ci` green (dashboard now a member)

## Tracey

none — build infrastructure. No markers.

## Files

```json files
[
  {"path": "rio-dashboard/package.json", "action": "NEW", "note": "T1: Svelte 5 deps (USER A7)"},
  {"path": "rio-dashboard/pnpm-lock.yaml", "action": "NEW", "note": "T1: generated lockfile"},
  {"path": "rio-dashboard/tsconfig.json", "action": "NEW", "note": "T1"},
  {"path": "rio-dashboard/svelte.config.js", "action": "NEW", "note": "T1"},
  {"path": "rio-dashboard/vite.config.ts", "action": "NEW", "note": "T1: dev proxy → Envoy :8080 (USER A1)"},
  {"path": "rio-dashboard/vitest.config.ts", "action": "NEW", "note": "T1"},
  {"path": "rio-dashboard/index.html", "action": "NEW", "note": "T1"},
  {"path": "rio-dashboard/src/main.ts", "action": "NEW", "note": "T1: mount(App)"},
  {"path": "rio-dashboard/src/App.svelte", "action": "NEW", "note": "T1: Svelte root"},
  {"path": "rio-dashboard/src/App.test.ts", "action": "NEW", "note": "T1"},
  {"path": "rio-dashboard/.gitignore", "action": "NEW", "note": "T1"},
  {"path": "nix/dashboard.nix", "action": "NEW", "note": "T2: fetchPnpmDeps derivation"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T3: 4 additive edits (checks + devshell) — count=24 HOT but disjoint"},
  {"path": "rio-dashboard/.gitignore", "action": "NEW", "note": "T4: node_modules/dist (root .gitignore also gets entries but this is the primary)"}
]
```

```
rio-dashboard/                    # T1: NEW directory (Svelte 5)
├── package.json
├── pnpm-lock.yaml
├── svelte.config.js
├── vite.config.ts
├── src/
│   ├── main.ts
│   └── App.svelte
└── ...
nix/dashboard.nix                 # T2: NEW
flake.nix                         # T3: 4 additive edits
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "USER A7: Svelte 5 NOT React. Wave 0a root. Parallel with P0273/P0276. flake.nix count=24 HOT but additive disjoint edits. A8: pnpmDeps hash bumps on every dep-adding downstream plan."}
```

**Depends on:** none — Wave 0a root.
**Conflicts with:** `flake.nix` count=24 — additive edits at distinct anchors. Disjoint from P0273 (no flake touch). P0282 also touches flake.nix (packages section) — different section, low conflict.
