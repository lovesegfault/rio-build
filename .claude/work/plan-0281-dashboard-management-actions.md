# Plan 0281: Management actions — Drain/ClearPoison/TriggerGC (Svelte)

**The Grafana-can't value.** Three write RPCs, all exist server-side. `TriggerGC` is a second server-stream — validates [P0279](plan-0279-dashboard-streaming-log-viewer.md)'s pattern.

**USER A7: Svelte.** Optimistic updates via `$state` mutation + revert-on-error.

## Entry criteria

- [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) merged (transport)

## Tasks

### T1 — `feat(dashboard):` Workers page

NEW `rio-dashboard/src/pages/Workers.svelte` — `admin.listWorkers({})`. Columns: `worker_id`, status pill, `running_builds/max_builds` bar, `size_class`, `last_heartbeat` relative (**>30s ago → red** — operator's first dead-worker signal). Per-row `DrainButton`.

### T2 — `feat(dashboard):` DrainButton — optimistic update

NEW `rio-dashboard/src/components/DrainButton.svelte`:
```svelte
<script lang="ts">
  import { admin } from "../api/admin";
  let { workerId, workers = $bindable() } = $props();

  async function drain() {
    if (!confirm(`Drain ${workerId}?`)) return;
    // Optimistic: mark draining immediately
    const idx = workers.findIndex(w => w.workerId === workerId);
    const prev = workers[idx].status;
    workers[idx].status = "draining";
    try {
      await admin.drainWorker({ workerId, force: false });
    } catch (e) {
      workers[idx].status = prev;  // revert
      toast.error(String(e));
    }
  }
</script>
<button on:click={drain}>Drain</button>
```

### T3 — `feat(dashboard):` ClearPoisonButton

NEW `rio-dashboard/src/components/ClearPoisonButton.svelte` — embedded in `DrvNode.svelte` context menu when `status==='poisoned'`. `admin.clearPoison({derivationHash})` → refetch graph → node re-renders queued.

### T4 — `feat(dashboard):` GC page — second server-stream

NEW `rio-dashboard/src/pages/GC.svelte`:
```svelte
<script lang="ts">
  import { admin } from "../api/admin";
  let dryRun = $state(true);
  let gracePeriodHours = $state(24);
  let progress = $state<GcProgress | null>(null);

  async function trigger() {
    for await (const p of admin.triggerGC({ dryRun, gracePeriodHours })) {
      progress = p;  // pathsScanned / pathsCollected / bytesFreed
      if (p.isComplete) break;
    }
  }
</script>
<!-- form + progress bar -->
```

Reuses P0279's AbortController pattern.

### T5 — `feat(dashboard):` Toast

NEW `rio-dashboard/src/components/Toast.svelte` — portal + auto-dismiss. Svelte store + `{#each toasts as t}`. ~40 lines. No library.

### T6 — `feat(dashboard):` integrate

GC button on Cluster page (P0277). ClearPoison in DrvNode context menu (P0280).

### T7 — `test(dashboard):` optimistic state transitions

Mock each RPC. Assert optimistic state set → success keeps it / error reverts. GC: mock stream → 3 progress chunks → bar updates.

## Exit criteria

- `/nbr .#ci` green

## Tracey

none — write actions, no normative spec marker.

## Files

```json files
[
  {"path": "rio-dashboard/src/pages/Workers.svelte", "action": "NEW", "note": "T1: worker list (>30s heartbeat = red)"},
  {"path": "rio-dashboard/src/components/DrainButton.svelte", "action": "NEW", "note": "T2: optimistic update"},
  {"path": "rio-dashboard/src/components/ClearPoisonButton.svelte", "action": "NEW", "note": "T3"},
  {"path": "rio-dashboard/src/pages/GC.svelte", "action": "NEW", "note": "T4: second server-stream"},
  {"path": "rio-dashboard/src/components/Toast.svelte", "action": "NEW", "note": "T5: hand-rolled ~40 lines"},
  {"path": "rio-dashboard/src/pages/Cluster.svelte", "action": "MODIFY", "note": "T6: GC button"},
  {"path": "rio-dashboard/src/components/DrvNode.svelte", "action": "MODIFY", "note": "T6: ClearPoison context menu (soft-conflict P0280, merge-trivial)"}
]
```

```
rio-dashboard/src/
├── pages/
│   ├── Workers.svelte            # T1
│   └── GC.svelte                 # T4: server-stream #2
└── components/
    ├── DrainButton.svelte        # T2
    ├── ClearPoisonButton.svelte  # T3
    └── Toast.svelte              # T5
```

## Dependencies

```json deps
{"deps": [277], "soft_deps": [278, 279, 280], "note": "USER A7: Svelte. Soft-deps integrate into P0278/P0279/P0280 components — merge-trivial. last_heartbeat >30s → red is the dead-worker signal."}
```

**Depends on:** [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) hard. Soft: P0278/P0279/P0280 (integrates into their components).
**Conflicts with:** `DrvNode.svelte` / `Cluster.svelte` — merge-trivial additions.
