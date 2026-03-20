# Plan 0392: LogStream O(n²) spread-reassign + viewer virtualization

[P0279](plan-0279-dashboard-streaming-log-viewer.md) ships `createLogStream` at [`logStream.svelte.ts`](../../rio-dashboard/src/lib/logStream.svelte.ts). Review found two compounding perf issues:

**O(n²) accumulation at `:55`:** `lines = [...lines, ...decoded]` copies the entire existing array per chunk. For a 10K-line build emitting 100-line chunks, that's ~100 chunks × (100+200+…+10000) ref-copies ≈ 50M copies. The comment at `:50-53` says spread-reassign is preferred for "unambiguous $effect dep" — but Svelte 5 runes proxy arrays deeply, so `lines.push(...decoded)` triggers reactivity identically and is O(chunk) per update.

**No virtualization at [`LogViewer.svelte:66`](../../rio-dashboard/src/components/LogViewer.svelte):** `{#each stream.lines as line, i (i)}` renders one `<pre>` per line. 10K+ DOM nodes for a long build → every scroll recomputes layout for all of them. The follow-tail `$effect` at `:49-54` fires on every chunk arrival and reads `scrollHeight` (forced layout). Combined with O(n²) array growth, the UI becomes janky well before the build completes.

Neither affects correctness — logs display correctly and follow-tail works. But the [`r[dash.stream.log-tail]`](../../docs/src/components/dashboard.md) spec at `:40` implies the viewer handles arbitrary build durations; a 2-hour kernel build can emit 100K+ lines.

## Entry criteria

- [P0279](plan-0279-dashboard-streaming-log-viewer.md) merged (`logStream.svelte.ts` + `LogViewer.svelte` exist)

## Tasks

### T1 — `perf(dashboard):` O(chunk) push instead of O(n) spread-reassign

MODIFY [`rio-dashboard/src/lib/logStream.svelte.ts`](../../rio-dashboard/src/lib/logStream.svelte.ts) at `:48-55`. Replace spread-reassign with `push`:

```ts
for await (const chunk of stream) {
  const decoded = chunk.lines.map((b: Uint8Array) => decoder.decode(b));
  // Svelte 5 runes proxy arrays: .push() is tracked. No reassign needed.
  // Prior spread-reassign was O(n) per chunk → O(n²) total for n lines.
  lines.push(...decoded);
  if (chunk.isComplete) {
    done = true;
    return;
  }
}
```

Delete the comment at `:49-53` explaining spread-reassign — it documented a false constraint. If `$state` array `.push()` doesn't trigger the `LogViewer` `$effect` (it should — [svelte.dev/docs/runes#$state](https://svelte.dev/docs/svelte/$state) says "mutations to the array are tracked"), the fallback is explicit length-read: `$effect(() => { void stream.lines.length; ... })` at the viewer side. Verify with the follow-tail test first; only reach for the fallback if it fails.

**Memory-cap side-effect:** while here, add a hard cap. Unbounded growth means an infinite-log build (stuck builder spinning output) eventually OOMs the tab. Cap at 50K lines — drop the oldest 10K when exceeded, set a `truncated: boolean` flag the viewer renders as a "— earlier output truncated —" banner.

### T2 — `perf(dashboard):` virtualize the log viewer

MODIFY [`rio-dashboard/src/components/LogViewer.svelte`](../../rio-dashboard/src/components/LogViewer.svelte) at `:66-68`. Replace `{#each stream.lines}` with a windowed slice. Options at dispatch:

**Option A — manual windowing (no new dep):** compute visible range from `scrollTop` / line-height, render only `lines.slice(startIdx, endIdx)` plus fixed-height spacers for off-screen ranges. The `.line` CSS has `line-height: 1.25rem` (fixed) so index→offset is trivial arithmetic. ~40L of new logic in the component. Integrates with existing `onScroll` at `:37-42`.

**Option B — [`svelte-virtual-list-ce`](https://www.npmjs.com/package/svelte-virtual-list-ce) or [`@tanstack/svelte-virtual`](https://tanstack.com/virtual):** drop-in `<VirtualList items={stream.lines} let:item>` wrapper. Adds ~8KB to the bundle. Pick whichever is Svelte-5-runes-compatible at dispatch (check the repo READMEs; `@tanstack/svelte-virtual` has a v5 adapter).

Prefer **Option A** — fixed line-height makes manual virtualization straightforward, and the bundle-size win matters for the dashboard image. Option B only if the follow-tail interaction with virtualization is tricky (virtual lists sometimes fight with programmatic `scrollTop` writes).

Both options MUST preserve follow-tail: the auto-scroll `$effect` at `:49-54` sets `container.scrollTop = container.scrollHeight`. With virtualization, `scrollHeight` is synthetic (spacer height) — verify the math is correct or switch to `scrollTo({ top: totalHeight })`.

### T3 — `test(dashboard):` large-log perf regression guard

NEW [`rio-dashboard/src/lib/__tests__/logStream.perf.test.ts`](../../rio-dashboard/src/lib/__tests__/logStream.perf.test.ts). Not a microbenchmark — a structural assertion that the accumulation is linear-ish:

```ts
// r[verify dash.stream.log-tail]
// Structural: accumulating 200 chunks of 100 lines shouldn't copy
// 2M+ refs. We can't measure ref-copies directly; instead, assert
// the mock-stream drain completes in <50ms on 20K lines. O(n²)
// spread-reassign takes ~1500ms on the same input in CI.
//
// This is a weak bound (CI machines vary ~3×) but the gap is 30×
// — a regression back to spread-reassign trips it reliably.
it('drains 20K lines in linear-ish time', async () => {
  const start = performance.now();
  // ... drive mock chunks through createLogStream ...
  const elapsed = performance.now() - start;
  expect(elapsed).toBeLessThan(200); // generous upper bound
  expect(stream.lines.length).toBe(20_000);
});
```

Also add a truncation test: drive >50K lines, assert `stream.truncated === true` and `stream.lines.length === 40_000` (50K cap - 10K drop).

### T4 — `test(dashboard):` LogViewer windowed-render smoke test

MODIFY (or NEW if absent) [`rio-dashboard/src/components/__tests__/LogViewer.test.ts`](../../rio-dashboard/src/components/__tests__/LogViewer.test.ts). Add a render-count assertion alongside [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T56's coverage (error-alert/empty-state/spinner):

```ts
// jsdom's layout is all-zeros per the :47 comment, so we can't test
// the actual scroll windowing. We CAN assert that rendering 5K lines
// doesn't produce 5K <pre> nodes — the virtual window should cap at
// ~viewport-height/line-height + overscan (~40 nodes for a 24rem
// container at 1.25rem lines + 10 overscan).
it('renders windowed subset, not all lines', () => {
  // ... mount with 5000-line mock stream ...
  const pres = container.querySelectorAll('pre.line');
  expect(pres.length).toBeLessThan(100);  // way under 5000
  expect(pres.length).toBeGreaterThan(0); // not empty
});
```

If jsdom can't measure anything useful for Option A (it probably can't — `scrollTop` is always 0), stub the start/end indices via a test-only prop override and assert the slice bounds directly.

## Exit criteria

- `pnpm --filter rio-dashboard test -- logStream.perf` → pass (<200ms on 20K lines)
- `pnpm --filter rio-dashboard test -- LogViewer` → all pass including windowed-subset
- `grep '\\[\\.\\.\\.lines, \\.\\.\\.decoded\\]\|lines = \\[\\.\\.\\.lines' rio-dashboard/src/lib/logStream.svelte.ts` → 0 hits (spread-reassign removed)
- `grep 'lines.push(' rio-dashboard/src/lib/logStream.svelte.ts` → ≥1 hit
- `grep 'truncated\|MAX_LINES\|50.?000' rio-dashboard/src/lib/logStream.svelte.ts` → ≥1 hit (cap present)
- Manual smoke: open the dashboard against a long-running build (or `yes | head -50000 > /dev/stderr` in a test derivation), verify follow-tail stays smooth
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[dash.stream.log-tail]` — T1 refines the implementation at the existing `r[impl]` annotation ([`logStream.svelte.ts:15`](../../rio-dashboard/src/lib/logStream.svelte.ts)); T3 adds a second `r[verify]` site (perf-structural, partners with P0279's functional verify at `__tests__/logStream.test.ts`)

No new markers. Virtualization is an implementation detail of log-tail; the spec doesn't commit to a render strategy.

## Files

```json files
[
  {"path": "rio-dashboard/src/lib/logStream.svelte.ts", "action": "MODIFY", "note": "T1: :48-55 spread-reassign → push; +50K-line cap + truncated flag"},
  {"path": "rio-dashboard/src/components/LogViewer.svelte", "action": "MODIFY", "note": "T2: :66-68 {#each all} → windowed slice (Option A manual or Option B virtual-list dep)"},
  {"path": "rio-dashboard/src/lib/__tests__/logStream.perf.test.ts", "action": "NEW", "note": "T3: 20K-line drain timing + truncation flag test; r[verify dash.stream.log-tail]"},
  {"path": "rio-dashboard/src/components/__tests__/LogViewer.test.ts", "action": "MODIFY", "note": "T4: windowed-subset render-count assert (alongside P0311-T60's error/empty/spinner tests)"},
  {"path": "rio-dashboard/package.json", "action": "MODIFY", "note": "T2 conditional: +@tanstack/svelte-virtual IF Option B chosen"}
]
```

```
rio-dashboard/src/
├── lib/
│   ├── logStream.svelte.ts          # T1: push not spread
│   └── __tests__/
│       └── logStream.perf.test.ts   # T3: NEW
└── components/
    ├── LogViewer.svelte             # T2: virtualize
    └── __tests__/
        └── LogViewer.test.ts        # T4: render-count
```

## Dependencies

```json deps
{"deps": [279], "soft_deps": [311, 280, 304], "note": "discovered_from=279 (rev-p279 perf). P0279 (UNIMPL — in frontier) ships logStream.svelte.ts + LogViewer.svelte. Soft-dep P0311-T60 (LogViewer component tests for error-alert/empty-state/spinner — same __tests__/LogViewer.test.ts file as T4; both additive, non-overlapping test-fns). Soft-dep P0280 (node-click → drvPath param usage — P0280 makes the drvPath prop live; T1 here doesn't touch drvPath, orthogonal). Soft-dep P0304-T154 (tags the :42 reconnect-on-transient-error comment with a TODO(P0NNN) — T1 rewrites :48-55 adjacent; non-overlapping lines, rebase-clean). package.json is low-conflict (additive dep line IF Option B). logStream.svelte.ts was NEW in P0279 → this plan is the only subsequent toucher; LogViewer.svelte same."}
```

**Depends on:** [P0279](plan-0279-dashboard-streaming-log-viewer.md) — ships both files this plan modifies.

**Conflicts with:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T56 also creates/modifies `__tests__/LogViewer.test.ts` — both additive test-fns, zero name collision. [P0304](plan-0304-trivial-batch-p0222-harness.md)-T149 edits `logStream.svelte.ts:42` (TODO tag); T1 edits `:48-55`; adjacent but non-overlapping hunks.
