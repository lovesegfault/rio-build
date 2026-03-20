# Plan 426: LogViewer/logStream — LINE_H rem-divergence, cap overshoot, spread arg-limit

rev-p392 correctness. Three compounding bugs in the code [P0392](plan-0392-logstream-quadratic-virtualize.md) ships. Each is a hard edge where the "normal" path works fine:

**LINE_H hardcodes 16px root font** at [`LogViewer.svelte:64`](../../rio-dashboard/src/components/LogViewer.svelte): `const LINE_H = 20; // 1.25rem @ 16px root`. The CSS at `.line { height: 1.25rem }` scales with the user's root font; the spacer math at `:135/:143` is `viewport.start * LINE_H` in **px**. A user with a 20px root (browser zoom or a11y setting) gets 25px line-heights but 20px spacers → the spacer-driven `scrollHeight` undershoots by 25% → the `$derived` viewport math at `:91` slices the wrong window → visible lines mis-align with their scroll positions. Follow-tail's `scrollTop = scrollHeight` at `:108` still works (synthetic-but-proportional), but scroll-to-line-N is wrong.

**Cap overshoot** at [`logStream.svelte.ts:67`](../../rio-dashboard/src/lib/logStream.svelte.ts): `if (lines.length > MAX_LINES) { lines.splice(0, DROP_LINES) }`. A 70K-line chunk (rare but possible — scheduler backfilling from a paused stream) pushes `lines` to 70K, splice drops 10K → 60K, still >50K. No re-check. The cap was supposed to bound memory at ~5MB; overshoot to 6MB isn't catastrophic but the invariant is broken. Worse if the next chunk is also large.

**Spread arg-limit** at [`logStream.svelte.ts:66`](../../rio-dashboard/src/lib/logStream.svelte.ts): `lines.push(...decoded)`. V8's argument-count limit is ~65K (engine-dependent, ~124K in SpiderMonkey). A 100K-line chunk throws `RangeError: Maximum call stack size exceeded` — the spread expands to 100K stack arguments. The stream catches it at `:87` and sets `err`, so the user sees "log stream failed: Maximum call stack" instead of their logs. Scheduler-side chunking typically emits <1K lines/chunk, but a recovery-restart or a `sinceLine: 0n` catch-up against a fast build can produce one giant batch.

None of these affect the 100-line-chunk happy path P0392's tests exercise. All three surface under adversarial conditions (a11y settings, backfill bursts, resume-from-zero against a completed 100K-line build).

## Entry criteria

- [P0392](plan-0392-logstream-quadratic-virtualize.md) merged (`LINE_H` virtualization + `MAX_LINES`/`DROP_LINES` cap + `lines.push(...decoded)` all exist)

## Tasks

### T1 — `fix(dashboard):` LINE_H — derive from computed style, not 16px assumption

MODIFY [`rio-dashboard/src/components/LogViewer.svelte`](../../rio-dashboard/src/components/LogViewer.svelte) at `:64`. Replace `const LINE_H = 20` with a reactive measurement:

```ts
// Measured from CSS line-height, not hardcoded. The .log-viewer has
// line-height: 1.25rem — at 16px root that's 20px, but browser zoom
// or user font settings change the root. Spacers are in px; CSS is in
// rem — divergence means wrong viewport math.
//
// Measurement happens once on mount via the container ref. jsdom
// returns 0 for getComputedStyle().lineHeight → fall back to 20
// (keeps the render-count tests deterministic).
let lineH = $state(20);
$effect(() => {
  if (container) {
    const parsed = parseFloat(getComputedStyle(container).lineHeight);
    if (parsed > 0) lineH = parsed;
  }
});
```

Replace all `LINE_H` refs with `lineH` at `:91`, `:92`, `:135`, `:143`. The `$effect` runs once when `container` binds (it reads `container`, not a stream property — no per-chunk re-measure). `parseFloat` handles `"20px"` → `20`; the `> 0` guard handles jsdom's `"normal"`/empty string → `NaN` → keep the 20 fallback.

**Why not rem-everywhere:** spacers need `style:height` which takes a px string or a CSS value. Switching to `style:height="{viewport.start * 1.25}rem"` would also work but loses the OVERSCAN-in-px simplicity and makes the `scrollTop / LINE_H` arithmetic at `:91` need a px→rem conversion too. Measuring once is cleaner.

### T2 — `fix(dashboard):` push spread — for-loop instead of `...decoded`

MODIFY [`rio-dashboard/src/lib/logStream.svelte.ts`](../../rio-dashboard/src/lib/logStream.svelte.ts) at `:66`. Replace `lines.push(...decoded)` with a loop:

```ts
// Avoid spread: lines.push(...decoded) expands to one stack arg per
// line — V8's ~65K arg limit means a 100K-line backfill chunk throws
// RangeError. Loop-push is O(chunk) either way and has no arg ceiling.
// Svelte's $state proxy tracks .push() per call; batching the render
// via $effect means one DOM update regardless.
for (const line of decoded) lines.push(line);
```

The runes-proxy reactivity concern P0392's comment at `:60-64` addressed (push triggers reactivity) holds for one-at-a-time push too — `$effect(() => void stream.lines.length; ...)` at [`LogViewer.svelte:106`](../../rio-dashboard/src/components/LogViewer.svelte) reads `.length` which bumps once per chunk (the IIFE's `for await` yields between chunks, microtask-batching the per-line pushes into one re-render).

### T3 — `fix(dashboard):` cap — single-pass Math.max truncate, not splice-then-maybe-still-over

MODIFY same file at `:67-72`. Replace the `if > MAX_LINES { splice(0, DROP_LINES) }` with a bounded-truncate:

```ts
// Cap at MAX_LINES. DROP_LINES gives hysteresis (don't splice every
// chunk once near the cap). Single check post-push: if over, trim to
// MAX_LINES - DROP_LINES. A giant chunk that alone exceeds MAX_LINES
// gets its HEAD dropped — we keep the TAIL (most recent). Prior
// `if (> MAX_LINES) splice(0, DROP_LINES)` left a 70K chunk at 60K.
if (lines.length > MAX_LINES) {
  const excess = lines.length - (MAX_LINES - DROP_LINES);
  lines.splice(0, excess);
  truncated = true;
}
```

`excess` is always `> DROP_LINES` (since `lines.length > MAX_LINES` and target is `MAX_LINES - DROP_LINES`), so small-chunk behavior is identical to before (drop 10K at 50K → 40K). Large-chunk behavior now guarantees `lines.length === MAX_LINES - DROP_LINES` post-splice.

### T4 — `test(dashboard):` regression tests for all three bugs

MODIFY [`rio-dashboard/src/lib/__tests__/logStream.perf.test.ts`](../../rio-dashboard/src/lib/__tests__/logStream.perf.test.ts) — add after P0392-T3's truncation test:

```ts
// rev-p392 correctness: single giant chunk is bounded correctly.
// Pre-fix a 70K-line chunk left lines at 60K (splice removed 10K,
// didn't re-check). Post-fix it's capped at MAX_LINES - DROP_LINES.
it('caps a single oversized chunk to the target, not just -DROP_LINES', async () => {
  // Drive one 70_000-line chunk through the mock.
  // ... mock-stream emits one chunk with 70K Uint8Array entries ...
  const stream = createLogStream('b-giant');
  // ... await drain ...
  expect(stream.truncated).toBe(true);
  expect(stream.lines.length).toBe(40_000); // MAX_LINES - DROP_LINES
});

// rev-p392 correctness: 100K-line chunk doesn't RangeError on spread.
// Pre-fix lines.push(...decoded) hit V8's ~65K arg limit. Post-fix
// loop-push has no ceiling. This test proves no exception; the cap
// test above proves the result is bounded.
it('handles a 100K-line chunk without RangeError', async () => {
  const stream = createLogStream('b-100k');
  // ... drive 100K-line chunk ...
  expect(stream.err).toBeNull();       // no RangeError surfaced as err
  expect(stream.lines.length).toBe(40_000);
});
```

MODIFY [`rio-dashboard/src/components/__tests__/LogViewer.test.ts`](../../rio-dashboard/src/components/__tests__/LogViewer.test.ts) — add a `lineH`-derivation test. jsdom's `getComputedStyle().lineHeight` returns `""` (empty) so the test asserts the fallback holds AND the `$effect` wiring is correct by stubbing `getComputedStyle`:

```ts
// rev-p392 correctness: LINE_H is measured, not hardcoded. jsdom's
// layout returns "" for lineHeight → 20px fallback. Stub it to return
// a non-16px-root value and assert the spacer math follows.
it('derives lineH from computed style (a11y: non-16px root)', () => {
  const orig = window.getComputedStyle;
  window.getComputedStyle = (...args) =>
    ({ ...orig(...args), lineHeight: '25px' } as CSSStyleDeclaration);
  try {
    createLogStream.mockReturnValue({
      lines: new Array(100).fill('x'),
      done: true,
      err: null,
      truncated: false,
      destroy: vi.fn(),
    });
    const { container } = render(LogViewer, {
      props: { buildId: 'b-a11y', viewportOverride: { start: 10, end: 20 } },
    });
    const spacer = container.querySelector('.spacer') as HTMLElement;
    // start=10 × lineH=25 → 250px top spacer (not 200px).
    expect(spacer.style.height).toBe('250px');
  } finally {
    window.getComputedStyle = orig;
  }
});
```

## Exit criteria

- `/nbr .#ci` green
- `pnpm --filter rio-dashboard test -- logStream.perf` → 100K-line and 70K-line tests both pass; no RangeError
- `pnpm --filter rio-dashboard test -- LogViewer` → all pass including lineH-from-computed-style
- `grep 'const LINE_H = 20' rio-dashboard/src/components/LogViewer.svelte` → 0 hits (hardcode removed)
- `grep 'lineH = \$state\|parseFloat.*lineHeight' rio-dashboard/src/components/LogViewer.svelte` → ≥2 hits (reactive measurement + parse)
- `grep 'lines.push(\.\.\.decoded)' rio-dashboard/src/lib/logStream.svelte.ts` → 0 hits (spread removed)
- `grep 'for.*decoded.*lines.push\|for (const line of decoded)' rio-dashboard/src/lib/logStream.svelte.ts` → ≥1 hit (loop-push)
- `grep 'lines.length - (MAX_LINES - DROP_LINES)\|excess' rio-dashboard/src/lib/logStream.svelte.ts` → ≥1 hit (bounded truncate)

## Tracey

References existing markers:
- `r[dash.stream.log-tail]` — T2+T3 refine the existing `r[impl]` at [`logStream.svelte.ts:15`](../../rio-dashboard/src/lib/logStream.svelte.ts); T4 adds another `r[verify]` site (correctness-structural, partners with P0392-T3's perf-structural verify)

No new markers. LINE_H-derivation and arg-limit hardening are implementation details of log-tail; the spec ("viewer follows new output, handles arbitrary build durations") already covers the intent.

## Files

```json files
[
  {"path": "rio-dashboard/src/components/LogViewer.svelte", "action": "MODIFY", "note": "T1: :64 LINE_H const→lineH $state + $effect measure; :91/:92/:135/:143 ref-swap"},
  {"path": "rio-dashboard/src/lib/logStream.svelte.ts", "action": "MODIFY", "note": "T2: :66 push(...decoded)→for-loop; T3: :67-72 if-splice→excess-truncate"},
  {"path": "rio-dashboard/src/lib/__tests__/logStream.perf.test.ts", "action": "MODIFY", "note": "T4: +70K-chunk cap test + 100K-chunk no-RangeError test; r[verify dash.stream.log-tail]"},
  {"path": "rio-dashboard/src/components/__tests__/LogViewer.test.ts", "action": "MODIFY", "note": "T4: +lineH-from-computed-style test (stub getComputedStyle → 25px → spacer=250px)"}
]
```

```
rio-dashboard/src/
├── components/
│   ├── LogViewer.svelte                   # T1: lineH measurement
│   └── __tests__/LogViewer.test.ts        # T4: a11y regression
└── lib/
    ├── logStream.svelte.ts                # T2+T3: loop-push + bounded-truncate
    └── __tests__/logStream.perf.test.ts   # T4: giant-chunk regressions
```

## Dependencies

```json deps
{"deps": [392], "soft_deps": [311, 304], "note": "discovered_from=392 (rev-p392 correctness rows 1-3). Hard-dep P0392 (ships all four files + LINE_H/MAX_LINES/push-spread sites). Soft-dep P0311-T60 (same __tests__/LogViewer.test.ts — additive test-fns, non-overlapping). Soft-dep P0304-T213/T214/T215 (trivial LogViewer polish batched there — :138 key, viewportOverride typing, title hover — same file, non-overlapping hunks). logStream.svelte.ts + LogViewer.svelte were both NEW in P0279 → only P0392 and this plan touch them; low collision."}
```

**Depends on:** [P0392](plan-0392-logstream-quadratic-virtualize.md) — ships the `LINE_H` constant, `push(...decoded)`, and cap logic this plan fixes.

**Conflicts with:** [P0311-T60](plan-0311-test-gap-batch-cli-recovery-dash.md) (same `__tests__/LogViewer.test.ts`, additive). [P0304-T213-T215](plan-0304-trivial-batch-p0222-harness.md) (same `LogViewer.svelte` — T213 adds `title={line}` at `:139`, T214 types `viewportOverride`, T215 keys `{#each}` on content — all adjacent to T1's `:64`/`:91`/`:135`/`:143` hunks but non-overlapping).
