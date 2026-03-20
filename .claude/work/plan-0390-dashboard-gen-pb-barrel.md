# Plan 0390: Dashboard gen/*_pb barrel — api/types.ts re-export layer

consol-mc185 finding. Seven dashboard consumers import from `../gen/*_pb` at three different path depths: `../gen/admin_pb` (api/ layer), `../gen/admin_types_pb` (pages/ + components/), `../../gen/admin_types_pb` (components/__tests__/), `../gen/types_pb` (GC.svelte for `GCProgress`). Each import is a direct reach into the protobuf-es codegen output — a path that shifts every time [P0376](plan-0376-types-proto-domain-split.md)-class splits happen (types.proto → dag.proto/build_types.proto/admin_types.proto). The next proto-split is a sed-fest across N consumers.

[`api/admin.ts`](../../rio-dashboard/src/api/admin.ts) already establishes the one-place-to-change pattern: `createClient(AdminService, transport)` exported as a singleton. But it exports only the *client*, not the *types* — consumers still reach into `../gen/*_pb` for `WorkerInfo`, `BuildInfo`, `ClusterStatusResponse`, `GCProgress`. A `src/api/types.ts` barrel mirroring `admin.ts` closes the asymmetry: one place imports from `gen/`, everything else imports from `api/`.

| Consumer | Current import | Depth |
|---|---|---|
| [`admin.ts:9`](../../rio-dashboard/src/api/admin.ts) | `../gen/admin_pb` | `src/api/` |
| [`BuildDrawer.svelte:2`](../../rio-dashboard/src/components/BuildDrawer.svelte) | `../gen/admin_types_pb` | `src/components/` |
| [`DrainHarness.svelte:8`](../../rio-dashboard/src/components/__tests__/DrainHarness.svelte) | `../../gen/admin_types_pb` | `src/components/__tests__/` |
| [`DrainButton.svelte:13`](../../rio-dashboard/src/components/DrainButton.svelte) | `../gen/admin_types_pb` | `src/components/` |
| [`Workers.svelte:10`](../../rio-dashboard/src/pages/Workers.svelte) | `../gen/admin_types_pb` | `src/pages/` |
| [`Builds.svelte:13`](../../rio-dashboard/src/pages/Builds.svelte) | `../gen/admin_types_pb` | `src/pages/` |
| [`GC.svelte:17`](../../rio-dashboard/src/pages/GC.svelte) | `../gen/types_pb` | `src/pages/` |
| [`Cluster.svelte:9`](../../rio-dashboard/src/pages/Cluster.svelte) | `../gen/admin_types_pb` | `src/pages/` |

## Entry criteria

- [P0376](plan-0376-types-proto-domain-split.md) merged (`admin_types.proto` exists as a separate file — the `admin_types_pb` import path this barrel re-exports from)

## Tasks

### T1 — `feat(dashboard):` api/types.ts barrel — re-export all consumed proto types

NEW [`rio-dashboard/src/api/types.ts`](../../rio-dashboard/src/api/types.ts):

```ts
// Barrel re-export of all protobuf-es generated types the dashboard
// consumes. Mirrors api/admin.ts (which exports the AdminService
// CLIENT; this exports the TYPES). Consumers import from '$api/types'
// instead of reaching into '../gen/*_pb' at file-depth-specific paths.
//
// RATIONALE: P0376 split types.proto into 4 domain protos; each split
// is a sed across N `../gen/*_pb` consumers. Centralizing here means
// the next split touches ONE file. Gen paths are buf.gen.yaml output —
// brittle to codegen config changes, stable when fronted by a barrel.

// admin_types.proto — dashboard-specific response shapes
export type {
  WorkerInfo,
  BuildInfo,
  ClusterStatusResponse,
  PoisonEntry,
  TenantInfo,
  SizeClassStatus,
  CutoffStats,
  GcStats,
} from '../gen/admin_types_pb';

// types.proto (legacy monolith residue — GCProgress lives here until
// a further split moves it to admin_types or a gc_types domain)
export type { GCProgress } from '../gen/types_pb';

// admin.proto service descriptor — test-support/admin-mock.test.ts
// introspects AdminService.method for surface-parity assertions.
export { AdminService } from '../gen/admin_pb';
```

**CARE — `export type` vs `export`**: proto messages are both types AND schema values (connect-es v2 `GenMessage<T>` carries runtime schema for create()/toBinary()). If consumers only use them as TypeScript types (`import type { WorkerInfo }`), `export type` is correct and tree-shake-friendly. If any consumer does `create(WorkerInfoSchema, {...})`, they need the value export too. Check at dispatch: `grep 'create(.*Schema' rio-dashboard/src/` — if zero hits, `export type` throughout; if nonzero, `export { X, XSchema }` for those.

**CARE — enumeration vs wildcard**: `export * from '../gen/admin_types_pb'` is simpler but re-exports everything including internal `file_admin_types` descriptors. Explicit enumeration documents what's ACTUALLY CONSUMED (the 7 consumers above use 5 types). Prefer enumeration for discoverability; extend the list when a new consumer appears. The parity test in [P0389](plan-0389-dashboard-test-admin-mock-setup.md)-T3 already catches "new AdminService RPC → missing stub"; a similar test here could catch "new consumer → missing barrel export" but that's overkill for a type barrel.

### T2 — `refactor(dashboard):` migrate 7 consumers from ../gen/*_pb → ../api/types

MODIFY all seven consumers. Each import becomes `from '../api/types'` (or `'../../api/types'` for `__tests__/`):

```ts
// BEFORE (DrainButton.svelte:13)
import type { WorkerInfo } from '../gen/admin_types_pb';
// AFTER
import type { WorkerInfo } from '../api/types';
```

Same pattern for all seven. The path-depth problem REDUCES to one dimension: how deep is the consumer relative to `src/api/` (not: which `gen/*_pb` file has the type AND how deep is the consumer).

**Option — tsconfig path alias**: add `"$api/*": ["src/api/*"]` to `tsconfig.json` `compilerOptions.paths` → consumers import `from '$api/types'` with zero path-depth reasoning. Vitest + Svelte tooling both honor tsconfig paths. Prefer this IF the project already uses path aliases (check `tsconfig.json` at dispatch); otherwise keep relative paths (less tooling-config surface).

### T3 — `refactor(dashboard):` api/index.ts barrel-of-barrels (optional)

OPTIONAL. NEW [`rio-dashboard/src/api/index.ts`](../../rio-dashboard/src/api/index.ts):

```ts
export { admin } from './admin';
export * from './types';
```

Then consumers `import { admin, WorkerInfo } from '../api'` — one import line covers client + types. Symmetric with the `rio-proto/src/lib.rs` pattern (`pub use crate::{client, types}` flat namespace). Skip T3 if the two-import style (`from '../api/admin'` + `from '../api/types'`) is preferred for explicitness.

## Exit criteria

- `/nbr .#ci` green (or `pnpm tsc --noEmit && pnpm vitest run` — whichever gates dashboard)
- `grep -r "from.*gen/.*_pb" rio-dashboard/src --include='*.svelte' --include='*.ts' | grep -v 'src/api/'` → 0 hits (only api/types.ts + api/admin.ts reach into gen/)
- `grep -c "from.*api/types" rio-dashboard/src/` → ≥7 (all consumers migrated)
- `test -f rio-dashboard/src/api/types.ts` (barrel exists)
- `pnpm vitest run` → all existing tests pass unchanged (types.ts is transparent re-export, no runtime semantics change)
- Mutation check: rename `admin_types_pb.ts` → `admin_types_pb_renamed.ts` → ONLY `api/types.ts` and `api/admin.ts` break (not 7 files). Proves the barrel is the ONE change-site. Revert.

## Tracey

No new markers. Proto-type import paths are build-system/tooling concern, not spec'd behavior. No `r[dash.*]` marker covers module organization.

## Files

```json files
[
  {"path": "rio-dashboard/src/api/types.ts", "action": "NEW", "note": "T1: barrel re-export of WorkerInfo/BuildInfo/ClusterStatusResponse/GCProgress/AdminService + admin-domain types"},
  {"path": "rio-dashboard/src/api/index.ts", "action": "NEW", "note": "T3 (OPTIONAL): barrel-of-barrels — re-export admin + types"},
  {"path": "rio-dashboard/src/api/admin.ts", "action": "MODIFY", "note": "T1: may source AdminService via ./types instead of ../gen/admin_pb (optional — keeps admin.ts self-contained if skipped)"},
  {"path": "rio-dashboard/src/components/BuildDrawer.svelte", "action": "MODIFY", "note": "T2: :2 import ../gen/admin_types_pb → ../api/types"},
  {"path": "rio-dashboard/src/components/DrainButton.svelte", "action": "MODIFY", "note": "T2: :13 import ../gen/admin_types_pb → ../api/types"},
  {"path": "rio-dashboard/src/components/__tests__/DrainHarness.svelte", "action": "MODIFY", "note": "T2: :8 import ../../gen/admin_types_pb → ../../api/types"},
  {"path": "rio-dashboard/src/pages/Workers.svelte", "action": "MODIFY", "note": "T2: :10 import ../gen/admin_types_pb → ../api/types"},
  {"path": "rio-dashboard/src/pages/Builds.svelte", "action": "MODIFY", "note": "T2: :13 import ../gen/admin_types_pb → ../api/types"},
  {"path": "rio-dashboard/src/pages/GC.svelte", "action": "MODIFY", "note": "T2: :17 import ../gen/types_pb (GCProgress) → ../api/types"},
  {"path": "rio-dashboard/src/pages/Cluster.svelte", "action": "MODIFY", "note": "T2: :9 import ../gen/admin_types_pb → ../api/types"}
]
```

```
rio-dashboard/src/
├── api/
│   ├── admin.ts   # (existing — AdminService client singleton)
│   ├── types.ts   # T1: NEW — type barrel
│   └── index.ts   # T3: NEW (optional) — barrel-of-barrels
├── components/
│   ├── BuildDrawer.svelte  # T2: import retarget
│   ├── DrainButton.svelte  # T2
│   └── __tests__/DrainHarness.svelte  # T2
└── pages/
    ├── Workers.svelte  # T2
    ├── Builds.svelte   # T2
    ├── GC.svelte       # T2
    └── Cluster.svelte  # T2
```

## Dependencies

```json deps
{"deps": [376], "soft_deps": [389, 304, 279, 280], "note": "P0376 split types.proto into admin_types.proto/dag.proto/build_types.proto — the admin_types_pb path this barrel re-exports from exists post-P0376. discovered_from=consol-mc185. Soft-dep P0389 (same planning batch — dashboard admin-mock): its T3 imports AdminService from gen/admin_pb; if this plan lands first, that import becomes `from '../api/types'` (cleaner, both work). Soft-dep P0304-T138 (adds rio-proto/src/lib.rs admin_types re-export — Rust-side parity to this TS-side barrel; same structural shape, different language; sequence-independent). Soft-dep P0279/P0280 (dashboard placeholder-fills — may add new gen/*_pb consumers before this lands; if so, T2 migrates them too). All 7 target files low-collision (<5 each)."}
```

**Depends on:** [P0376](plan-0376-types-proto-domain-split.md) (DONE — `admin_types.proto` split landed).

**Conflicts with:** [P0389](plan-0389-dashboard-test-admin-mock-setup.md) touches test files (not component/page .svelte files) — no overlap. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T124 through T127 add attrs to DrainButton/BuildDrawer/Builds.svelte — same files as T2 here, but T2 edits the import line at top-of-`<script>`, T124-T127 edit template markup. Non-overlapping hunks, rebase-clean either order.
