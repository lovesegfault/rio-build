# Plan 0275: Proto TS codegen — buf CLI first (TWO-EXIT SPIKE)

**USER A3: buf CLI first, buildNpmPackage fallback.** `pkgs.buf` IS in nixpkgs. Try `buf generate` in-sandbox. If buf's bundled plugins cover connect-es without network → done. If buf needs remote plugins (network) → package `@connectrpc/protoc-gen-connect-es` via `buildNpmPackage` with vendored binary. Committed-gen is LAST resort, not expected.

De-risks R2: `@bufbuild/protoc-gen-es` ships native binary via postinstall; `fetchPnpmDeps` uses `--ignore-scripts` ([`nix/tracey.nix:54-56`](../../nix/tracey.nix) proves the trap).

## Entry criteria

- [P0274](plan-0274-dashboard-svelte-scaffold.md) merged (scaffold + `nix/dashboard.nix` exist)

## Tasks

### T0 — `feat(nix):` try pkgs.buf in-sandbox (USER A3 primary path)

MODIFY [`nix/dashboard.nix`](../../nix/dashboard.nix):

```nix
nativeBuildInputs = with pkgs; [ nodejs pnpm_10 pnpmConfigHook buf protobuf ];
preBuild = ''
  # USER A3: buf CLI first. nixpkgs buf may bundle plugins.
  buf generate --template ${./buf.gen.yaml} ../rio-proto/proto -o src/gen
  test -f src/gen/admin_connect.ts || (echo "buf generate no-op"; exit 1)
'';
```

If `pkgs.buf` works with `local:` plugin refs → done. If it needs `remote:` plugins (network) → T1.

### T1 — `feat(nix):` buildNpmPackage fallback for plugins

IF T0 fails: package `@connectrpc/protoc-gen-connect-es` + `@bufbuild/protoc-gen-es` as nixpkgs overlays using `buildNpmPackage` with vendored binaries. Add to `nativeBuildInputs`.

### T2 — `feat(dashboard):` buf.gen.yaml

NEW `rio-dashboard/buf.gen.yaml`:

```yaml
version: v2
inputs:
  - directory: ../rio-proto/proto
plugins:
  - local: protoc-gen-es        # from pkgs.buf or buildNpmPackage
    out: src/gen
    opt: [target=ts]
  - local: protoc-gen-connect-es
    out: src/gen
    opt: [target=ts]
```

### T3 — `feat(dashboard):` lint/tsconfig exclude gen/

`tsconfig.json include: ["src"]` (gen/ under src/). `eslint.config.js` ignore `src/gen/**`. `flake.nix` `pre-commit.settings.excludes`: `"^rio-dashboard/src/gen/"`.

### T4 — DECISION POINT

`/nbr .#checks.x86_64-linux.dashboard`:
- **Passes with T0** → buf-in-sandbox, done
- **Passes with T1** → buildNpmPackage fallback, done
- **Both fail** → LAST resort: committed-gen with drift check (NEW `scripts/regen-proto-ts.sh`, commit `src/gen/*.ts`, buildPhase diffs against fresh gen). Per A3 this is NOT expected.

### T5 — `test(dashboard):` import compiles

Add `import { AdminService } from './gen/admin_connect'` in `src/App.svelte` `<script>` — verifies types resolve + tree-shaking doesn't break.

## Exit criteria

- `/nbr .#checks.x86_64-linux.dashboard` green (one of the three exit paths)
- `src/gen/admin_connect.ts` exports `AdminService`
- Import in App.svelte compiles

## Tracey

none — build tooling.

## Files

```json files
[
  {"path": "rio-dashboard/buf.gen.yaml", "action": "NEW", "note": "T2"},
  {"path": "nix/dashboard.nix", "action": "MODIFY", "note": "T0: buf in nativeBuildInputs + preBuild gen"},
  {"path": "rio-dashboard/package.json", "action": "MODIFY", "note": "pnpmDeps hash bump if plugins added to deps"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T3: pre-commit exclude (one line)"},
  {"path": "scripts/regen-proto-ts.sh", "action": "NEW", "note": "T4 LAST resort only — not expected per USER A3"}
]
```

```
rio-dashboard/
├── buf.gen.yaml                  # T2: NEW
└── src/gen/                      # generated (or committed if last-resort)
nix/dashboard.nix                 # T0/T1: buf integration
flake.nix                         # T3: exclude (one line)
```

## Dependencies

```json deps
{"deps": [274], "soft_deps": [], "note": "USER A3: buf CLI first (nixpkgs), buildNpmPackage fallback, committed-gen LAST resort. Serialized on P0274 (same files). Parallel with P0273/P0276."}
```

**Depends on:** [P0274](plan-0274-dashboard-svelte-scaffold.md) hard — same files (`nix/dashboard.nix`, `package.json`).
**Conflicts with:** `nix/dashboard.nix` + `package.json` serial via dep. `flake.nix` one-line additive.
