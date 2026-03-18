# Plan 0279: Streaming log viewer — R3 consumer (Svelte)

**Highest TS-side risk.** [P0273](plan-0273-envoy-sidecar-grpc-web.md)'s curl proved Envoy STREAMS (0x80 trailer byte). This plan proves the browser CONSUMES it via `for await ... of` on a Connect server-stream.

**R8:** `BuildLogChunk.lines` is `repeated bytes` → `Uint8Array[]`. Build output CAN be non-UTF-8 (compiler locale garbage, binary accidentally cat'd). `new TextDecoder('utf-8', {fatal: false})` → `\ufffd`, no throw.

**USER A7: Svelte.** Custom store + `$state` wrapper instead of `useSyncExternalStore`.

## Entry criteria

- [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) merged (transport works)

## Tasks

### T1 — `feat(dashboard):` log stream store

NEW `rio-dashboard/src/lib/logStream.svelte.ts` (Svelte 5 `.svelte.ts` for runes-in-modules):

```typescript
import { admin } from "../api/admin";

// R8: lossy decode. {fatal:false} → U+FFFD, no throw.
const decoder = new TextDecoder("utf-8", { fatal: false });

export function createLogStream(buildId: string, drvPath?: string) {
  let lines = $state<string[]>([]);
  let done = $state(false);
  let err = $state<Error | null>(null);
  const ctrl = new AbortController();

  (async () => {
    try {
      const stream = admin.getBuildLogs(
        { buildId, derivationPath: drvPath ?? "", sinceLine: 0n },
        { signal: ctrl.signal }
      );
      for await (const chunk of stream) {
        // r[impl dash.stream.log-tail]
        const decoded = chunk.lines.map((b: Uint8Array) => decoder.decode(b));
        lines = [...lines, ...decoded];  // $state reassignment triggers reactivity
        if (chunk.isComplete) { done = true; return; }
      }
    } catch (e) {
      if (!ctrl.signal.aborted) err = e as Error;
    }
  })();

  return {
    get lines() { return lines; },
    get done() { return done; },
    get err() { return err; },
    destroy: () => ctrl.abort(),
  };
}
```

**Perf note:** `[...lines, ...decoded]` is O(n) per chunk. `types.proto:178` says 64 lines/100ms batching — chunks are small. If 10-minute builds become a bottleneck: switch to a ring-buffer store. Don't prematurely optimize.

### T2 — `feat(dashboard):` LogViewer component

NEW `rio-dashboard/src/components/LogViewer.svelte`:
```svelte
<script lang="ts">
  import { createLogStream } from "../lib/logStream.svelte";
  let { buildId, drvPath } = $props();
  const stream = createLogStream(buildId, drvPath);
  $effect(() => () => stream.destroy());  // cleanup on unmount

  let container: HTMLDivElement;
  let userScrolledUp = $state(false);
  $effect(() => {
    stream.lines;  // track dependency
    if (!userScrolledUp && container) {
      container.scrollTop = container.scrollHeight;  // follow-tail
    }
  });
</script>

<div bind:this={container} on:scroll={/* detect userScrolledUp */}>
  {#each stream.lines as line}
    <pre>{line}</pre>
  {/each}
</div>
```

Virtual scrolling (if needed for >10k lines): `@tanstack/svelte-virtual` or hand-rolled `{#each stream.lines.slice(visible)}`.

### T3 — `feat(dashboard):` embed in BuildDrawer Logs tab

MODIFY `rio-dashboard/src/components/BuildDrawer.svelte` — fill the "Logs" tab placeholder from [P0278](plan-0278-dashboard-build-list-drawer.md).

### T4 — `test(dashboard):` stream consumption + R8

NEW `rio-dashboard/src/lib/__tests__/logStream.test.ts`:
```typescript
// Mock admin.getBuildLogs as async generator yielding 2 chunks → assert lines accumulate.
// r[verify dash.stream.log-tail] — R8 test:
//   yield {lines: [Uint8Array.of(0x48,0x69), Uint8Array.of(0xff,0xfe,0x21)]}
//   → no throw, lines[1].includes('\ufffd')
// Mock isComplete=true → done flips.
// destroy() → generator's signal.aborted=true.
```

## Exit criteria

- `/nbr .#ci` green
- R8 vitest passes (`\ufffd` for bad UTF-8, no throw)

## Tracey

References existing markers:
- `r[dash.stream.log-tail]` — T1 implements, T4 verifies (seeded by P0284)
- `r[dash.journey.build-to-logs]` — T3 is the final step ("log stream renders"). Full journey impl closed here (with P0278+P0280).

## Files

```json files
[
  {"path": "rio-dashboard/src/lib/logStream.svelte.ts", "action": "NEW", "note": "T1: Svelte 5 runes-in-module, AbortController cleanup"},
  {"path": "rio-dashboard/src/components/LogViewer.svelte", "action": "NEW", "note": "T2: follow-tail scroller"},
  {"path": "rio-dashboard/src/components/BuildDrawer.svelte", "action": "MODIFY", "note": "T3: fill Logs tab (merge-trivial with P0280's Graph tab)"},
  {"path": "rio-dashboard/src/lib/__tests__/logStream.test.ts", "action": "NEW", "note": "T4: R8 test (\\ufffd for bad UTF-8)"},
  {"path": "rio-dashboard/package.json", "action": "MODIFY", "note": "virtual scroll dep if needed"},
  {"path": "nix/dashboard.nix", "action": "MODIFY", "note": "A8: pnpmDeps hash bump"}
]
```

```
rio-dashboard/src/
├── lib/
│   ├── logStream.svelte.ts       # T1: runes-in-module
│   └── __tests__/logStream.test.ts # T4: R8 test
└── components/
    ├── LogViewer.svelte          # T2
    └── BuildDrawer.svelte        # T3: Logs tab
```

## Dependencies

```json deps
{"deps": [277], "soft_deps": [278], "note": "USER A7: Svelte runes-in-module (.svelte.ts). Highest TS-side risk (R3 consumer). R8: TextDecoder fatal:false. BuildDrawer.svelte soft-conflict with P0280 (distinct tab slots — merge-trivial)."}
```

**Depends on:** [P0277](plan-0277-dashboard-app-shell-clusterstatus.md) — transport works. Soft: [P0278](plan-0278-dashboard-build-list-drawer.md) — embeds in drawer.
**Conflicts with:** `BuildDrawer.svelte` merge-trivial with P0280 (distinct tab slots). `package.json` serial.
