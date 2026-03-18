# Plan 0278: Build list + detail drawer (Svelte)

**USER A7: Svelte.** Build table with status pills, progress bars, click → slide-over drawer. Drawer has "Logs" + "Graph" tab placeholders that [P0279](plan-0279-dashboard-streaming-log-viewer.md) and [P0280](plan-0280-dashboard-dag-viz-xyflow.md) fill.

## Entry criteria

- [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) merged (transport + router)

## Tasks

### T1 — `feat(dashboard):` Builds page

NEW `rio-dashboard/src/pages/Builds.svelte`:
```svelte
<script lang="ts">
  import { admin } from "../api/admin";
  import BuildDrawer from "../components/BuildDrawer.svelte";
  import BuildStatePill from "../components/BuildStatePill.svelte";

  let builds = $state<BuildInfo[]>([]);
  let selectedBuild = $state<BuildInfo | null>(null);
  let statusFilter = $state<string>("");
  let page = $state(0);

  $effect(() => {
    admin.listBuilds({ statusFilter, limit: 100, offset: page * 100 })
      .then(r => builds = r.builds);
  });
</script>

<!-- Status filter pills -->
<!-- Table: build_id (mono, click-copy), state pill, tenant, progress bar, submitted_at relative, duration -->
<!-- Row click → selectedBuild = build -->

{#if selectedBuild}
  <BuildDrawer build={selectedBuild} on:close={() => selectedBuild = null} />
{/if}
```

Scheduler-side cap check: `rio-scheduler/src/admin/builds.rs:33` clamps limit to 1000 — 100/page is safe.

### T2 — `feat(dashboard):` BuildDrawer component

NEW `rio-dashboard/src/components/BuildDrawer.svelte` — slide-over showing all `BuildInfo` fields. Two tab placeholders ("Logs" / "Graph"). Deep-link fallback: `/builds/:id` with no list context → `listBuilds({limit:1000})` client-side `.find()`.

### T3 — `feat(dashboard):` BuildStatePill component

NEW `rio-dashboard/src/components/BuildStatePill.svelte` — `BuildState` enum → color + label. Single source of truth for "what color is Failed."

### T4 — `test(dashboard):` list + drawer

Mock `listBuilds` → 3 builds mixed states → assert 3 rows. Click → drawer opens. Filter pill → assert filter in call args.

## Exit criteria

- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[dash.journey.build-to-logs]` — T1 is step 1 ("click build"). Partial — full journey closed by P0280+P0279.

## Files

```json files
[
  {"path": "rio-dashboard/src/pages/Builds.svelte", "action": "NEW", "note": "T1: build list (Svelte)"},
  {"path": "rio-dashboard/src/components/BuildDrawer.svelte", "action": "NEW", "note": "T2: drawer with Logs/Graph tab placeholders"},
  {"path": "rio-dashboard/src/components/BuildStatePill.svelte", "action": "NEW", "note": "T3: state→color"},
  {"path": "rio-dashboard/package.json", "action": "MODIFY", "note": "hash bump if table lib added"},
  {"path": "nix/dashboard.nix", "action": "MODIFY", "note": "A8: pnpmDeps hash bump"}
]
```

```
rio-dashboard/src/
├── pages/Builds.svelte           # T1
└── components/
    ├── BuildDrawer.svelte        # T2 (P0279/P0280 fill tabs)
    └── BuildStatePill.svelte     # T3
```

## Dependencies

```json deps
{"deps": [277], "soft_deps": [], "note": "USER A7: Svelte. BuildDrawer created here; P0279/P0280 add tab content. A8: pnpmDeps hash bump."}
```

**Depends on:** [P0277](plan-0277-dashboard-app-shell-clusterstatus.md).
**Conflicts with:** `package.json`/`nix/dashboard.nix` serial via dep chain.
