# Plan 0410: DerivationStatus cross-language enforcement — Rust enum ↔ TS mirror

rev-p400 test-gap at [`rio-dashboard/src/lib/graphLayout.ts:43-55,73-85`](../../rio-dashboard/src/lib/graphLayout.ts). The Rust source-of-truth is [`DerivationStatus::as_str`](../../rio-scheduler/src/state/derivation.rs) at `:138-152` (11 arms: created/queued/ready/assigned/running/completed/failed/poisoned/dependency_failed/cancelled/skipped). The wire format at [`dag.proto:126`](../../rio-proto/proto/dag.proto) is `string status = 4` (NOT an enum — matches the PG `CHECK` constraint values at [`scheduler.md:536`](../../docs/src/components/scheduler.md)). The dashboard has TWO hand-maintained mirrors: `STATUS_CLASS` (status→color) and `SORT_RANK` (status→table-position).

**The exact failure [P0400](plan-0400-graph-page-skipped-worker-race.md)-T1 fixed:** [P0252](plan-0252-ca-cutoff-propagate-skipped.md) added `Skipped` to Rust; `STATUS_CLASS` / `SORT_RANK` fell through to gray/rank-5. No lint signal, no test signal — silently wrong for any skipped-state derivation on the graph page. [`graphLayout.test.ts:112-128`](../../rio-dashboard/src/lib/__tests__/graphLayout.test.ts) pins the full current 11-set but only catches `STATUS_CLASS`↔`STATUS_CLASS` drift (same-file), not `derivation.rs`→`STATUS_CLASS` drift (cross-language).

A 12th variant added to `DerivationStatus` silently renders gray and sorts bottom until a human re-flags.

**Three fix options:**

- **(a) Generated TS file** — `build.rs` (or vite plugin) parses `derivation.rs` `as_str` arms, emits `rio-dashboard/src/gen/derivationStatuses.ts` with `export const DERIVATION_STATUSES = [...] as const`. `STATUS_CLASS`/`SORT_RANK` become `Record<DerivationStatus, ...>` and TS exhaustiveness catches misses. Drawback: `rio-dashboard` doesn't have a Rust `build.rs`; this needs either a Nix-level codegen step or a standalone script wired into `pnpm build`.
- **(b) Rust-side nextest** — iterate `DerivationStatus` variants (`strum::IntoEnumIterator` or manual `ALL: [Self; 11]`), serialize to JSON, compare against a hand-embedded mirror of `graphLayout.ts`'s 11-set. Drawback: still hand-maintained — just moves the sync point from TS to Rust (but nextest fails LOUDLY, not silently-gray).
- **(c) Shared JSON snapshot** — `derivation.rs` test writes `[…11 strings…]` to a snapshot file; vitest imports the same file and asserts `Object.keys(STATUS_CLASS)` superset. Drawback: snapshot file checked in, two consumers — the golden-file approach used elsewhere (`rio-test-support/golden/`).

**Pick (c).** Cheapest, no codegen toolchain, both sides fail on cardinality mismatch, and the snapshot file IS the single source of truth a reviewer can diff.

> **DISPATCH NOTE (consol-mc225):** golden shape must be `[{status, terminal}]` not `[string]`.
>
> [P0400](plan-0400-graph-page-skipped-worker-race.md) (merged this window) added `export const TERMINAL` at [`graphLayout.ts:67-73`](../../rio-dashboard/src/lib/graphLayout.ts) mirroring [`DerivationStatus::is_terminal()`](../../rio-scheduler/src/state/derivation.rs) at `:49-58` (5 members: completed/skipped/poisoned/dependency_failed/cancelled). The `[string]` golden covers `STATUS_CLASS` + `SORT_RANK` but NOT `TERMINAL` — `is_terminal` is a PREDICATE over the status set, not derivable from the string list alone. [`graphLayout.test.ts:168-185`](../../rio-dashboard/src/lib/__tests__/graphLayout.test.ts) currently pins `TERMINAL` against a HARDCODED TS-side set — same-file self-consistency only, no Rust linkage. If Rust reclassifies (e.g., `Failed` becomes terminal, or a new `Quarantined` is terminal), `TERMINAL` silently drifts while the `STATUS_CLASS` golden still passes.
>
> **Extend T2's golden shape** to:
>
> ```json
> [
>   {"status": "created",           "terminal": false},
>   {"status": "queued",            "terminal": false},
>   {"status": "ready",             "terminal": false},
>   {"status": "assigned",          "terminal": false},
>   {"status": "running",           "terminal": false},
>   {"status": "completed",         "terminal": true},
>   {"status": "failed",            "terminal": false},
>   {"status": "poisoned",          "terminal": true},
>   {"status": "dependency_failed", "terminal": true},
>   {"status": "cancelled",         "terminal": true},
>   {"status": "skipped",           "terminal": true}
> ]
> ```
>
> **Extend T1's Rust emit** to serialize `{status: s.as_str(), terminal: s.is_terminal()}` instead of bare `s.as_str()`. The `ALL` const + `_witness` exhaustive-match stay unchanged (they cover the enum-variant set; `is_terminal()` is a derived predicate).
>
> **Add to T3** a second vitest assert:
>
> ```typescript
> it('TERMINAL matches Rust is_terminal() via golden', () => {
>   for (const { status, terminal } of goldenStatuses) {
>     expect(TERMINAL.has(status),
>       `TERMINAL drift: Rust says "${status}".is_terminal()=${terminal}, ` +
>       `TS TERMINAL.has=${TERMINAL.has(status)}. ` +
>       `Update graphLayout.ts:67-73.`
>     ).toBe(terminal);
>   }
> });
> ```
>
> This closes the third mirror in one 130L TS file (`STATUS_CLASS`:43, `TERMINAL`:67, `SORT_RANK`:107) — 2 of 3 were covered by original-P0410, the 3rd landed SAME WINDOW as P0410 landed in dag. Zero-cost at authoring time (one more serde field, one more vitest assert).

## Entry criteria

- [P0400](plan-0400-graph-page-skipped-worker-race.md) merged (`skipped` arm present in both `STATUS_CLASS` and `SORT_RANK`; test at `:116-124` pins 11-set including skipped)

## Tasks

### T1 — `test(scheduler):` derivation.rs — emit status-strings snapshot

MODIFY [`rio-scheduler/src/state/derivation.rs`](../../rio-scheduler/src/state/derivation.rs) `cfg(test)` mod (after `:637`). Add a const `ALL` array on `DerivationStatus` (the enum has no `strum::EnumIter`; a manual `[Self; 11]` is simpler than adding a dep):

```rust
impl DerivationStatus {
    /// All variants. Used by the snapshot test (T1 below) and by
    /// the dashboard's cross-language cardinality check.
    #[cfg(test)]
    pub const ALL: [Self; 11] = [
        Self::Created, Self::Queued, Self::Ready, Self::Assigned,
        Self::Running, Self::Completed, Self::Failed, Self::Poisoned,
        Self::DependencyFailed, Self::Cancelled, Self::Skipped,
    ];
}

#[cfg(test)]
mod status_snapshot {
    use super::*;

    /// Writes the canonical as_str set to a JSON file that vitest
    /// (graphLayout.test.ts) reads. Fails if the snapshot drifts —
    /// adding a 12th variant here AND forgetting STATUS_CLASS in the
    /// dashboard breaks BOTH this snapshot test AND the vitest
    /// superset check.
    ///
    /// r[verify sched.state.transitions]
    #[test]
    fn derivation_status_snapshot_is_current() {
        let strings: Vec<&str> = DerivationStatus::ALL.iter()
            .map(|s| s.as_str()).collect();
        let json = serde_json::to_string_pretty(&strings).unwrap();
        let golden = include_str!(
            "../../../rio-test-support/golden/derivation_statuses.json"
        );
        assert_eq!(
            json.trim(), golden.trim(),
            "DerivationStatus::as_str set drifted from golden snapshot. \
             If you added a variant, also update: \
             (1) rio-test-support/golden/derivation_statuses.json, \
             (2) rio-dashboard/src/lib/graphLayout.ts STATUS_CLASS+SORT_RANK, \
             (3) rio-dashboard/src/lib/__tests__/graphLayout.test.ts :116-124, \
             (4) docs/src/components/scheduler.md :536 PG CHECK constraint."
        );
    }

    /// Positive control: `ALL` cardinality matches the enum. A 12th
    /// variant without updating `ALL` compiles (arrays don't enforce
    /// exhaustiveness) — this catches that via match-exhaustive.
    #[test]
    fn all_const_is_exhaustive() {
        // Exhaustive match forces compile-error on new variant.
        fn _witness(s: DerivationStatus) -> usize {
            match s {
                DerivationStatus::Created => 0,
                DerivationStatus::Queued => 1,
                DerivationStatus::Ready => 2,
                DerivationStatus::Assigned => 3,
                DerivationStatus::Running => 4,
                DerivationStatus::Completed => 5,
                DerivationStatus::Failed => 6,
                DerivationStatus::Poisoned => 7,
                DerivationStatus::DependencyFailed => 8,
                DerivationStatus::Cancelled => 9,
                DerivationStatus::Skipped => 10,
            }
        }
        assert_eq!(DerivationStatus::ALL.len(), 11);
    }
}
```

**CARE — `include_str!` path:** relative to the file (`derivation.rs`), so `../../../rio-test-support/golden/derivation_statuses.json` from `rio-scheduler/src/state/derivation.rs`. If `rio-test-support/golden/` doesn't exist yet, create it. The crane source filter for tests may not include `rio-test-support/golden/` — check `craneLib.buildDepsOnly` fileset; if excluded, the `include_str!` fails to compile. Alternative: embed the golden in `rio-scheduler/tests/golden/` (guaranteed in fileset) and symlink/copy for the dashboard.

### T2 — `feat(test-support):` golden/derivation_statuses.json — seed snapshot

NEW [`rio-test-support/golden/derivation_statuses.json`](../../rio-test-support/golden/derivation_statuses.json):

```json
[
  "created",
  "queued",
  "ready",
  "assigned",
  "running",
  "completed",
  "failed",
  "poisoned",
  "dependency_failed",
  "cancelled",
  "skipped"
]
```

One-line-per-status (pretty-printed) so `git diff` on a new variant shows a single-line add.

### T3 — `test(dashboard):` graphLayout.test.ts — cross-language superset check

MODIFY [`rio-dashboard/src/lib/__tests__/graphLayout.test.ts`](../../rio-dashboard/src/lib/__tests__/graphLayout.test.ts) after the existing status-class test (`:112-128`). Import the golden JSON and assert both TS maps cover it:

```typescript
import goldenStatuses from '../../../../rio-test-support/golden/derivation_statuses.json';

describe('cross-language status enforcement', () => {
  it('STATUS_CLASS covers every Rust DerivationStatus::as_str', () => {
    // goldenStatuses is the canonical set from derivation.rs.
    // statusClass(s) returns 'gray' for unknown — so a fall-through
    // is silent-gray not throw. This test catches fall-throughs.
    const classesUsed = new Set(goldenStatuses.map(statusClass));
    // If any golden status is NOT in STATUS_CLASS, it maps to 'gray'.
    // We can't distinguish "intentionally gray" from "fell through",
    // so instead check cardinality: all 11 golden statuses produce
    // EXACTLY the 4 colors (green/yellow/red/gray), AND the explicit
    // gray-set + green/yellow/red-set union == golden set.
    const green = goldenStatuses.filter(s => statusClass(s) === 'green');
    const yellow = goldenStatuses.filter(s => statusClass(s) === 'yellow');
    const red = goldenStatuses.filter(s => statusClass(s) === 'red');
    const gray = goldenStatuses.filter(s => statusClass(s) === 'gray');
    // The test at :116-124 already pins these buckets; this
    // cross-check proves the golden file AND those buckets align.
    expect(green.length + yellow.length + red.length + gray.length)
      .toBe(goldenStatuses.length);
    // Fell-through detection: the :116-124 arrays are the INTENDED
    // classification. Any golden status NOT in those arrays = drift.
    const intended = new Set([
      'completed', 'skipped',                          // green
      'running', 'assigned',                           // yellow
      'failed', 'poisoned', 'dependency_failed',       // red
      'created', 'queued', 'ready', 'cancelled',       // gray
    ]);
    for (const s of goldenStatuses) {
      expect(intended.has(s),
        `Rust emitted "${s}" but STATUS_CLASS has no explicit arm — ` +
        `it will fall through to gray. Add to graphLayout.ts:43-55.`
      ).toBe(true);
    }
  });

  it('SORT_RANK covers every golden status (no fall-through to rank-5)', () => {
    for (const s of goldenStatuses) {
      // sortForTable uses `SORT_RANK[s] ?? 5` — rank-5 = unknown/bottom.
      // We re-export SORT_RANK or check via the sort behavior:
      const nodes = [
        { drvPath: 'a', pname: 'a', status: s },
        { drvPath: 'b', pname: 'b', status: 'completed' },  // rank 4
      ] as any;
      const sorted = sortForTable(nodes);
      // If s has no SORT_RANK arm, it's rank-5 → sorts AFTER completed.
      // But we want EXPLICIT rank for every status. Either export
      // SORT_RANK and check `s in SORT_RANK`, or accept this
      // heuristic: only known-rank-5 would sort after completed, and
      // there IS no intentional rank-5 in the current design.
      // Simpler: export SORT_RANK from graphLayout.ts (currently
      // file-private at :73).
    }
  });
});
```

**CARE — SORT_RANK export:** `SORT_RANK` is file-private at `:73`. The cleanest check is `Object.keys(SORT_RANK)` — export it as `export const SORT_RANK` (the `statusClass` fn is already exported so `STATUS_CLASS` is testable indirectly; `sortForTable` similarly uses `SORT_RANK` internally). Exporting the const is a 1-token change at `:73` (`const` → `export const`).

**CARE — JSON import in vitest:** `import foo from '*.json'` needs `"resolveJsonModule": true` in `tsconfig.json`. Check at dispatch — if missing, add it (1 line in `compilerOptions`). Alternative: `fs.readFileSync` + `JSON.parse` in the test (more explicit, no tsconfig change).

**CARE — relative path from test file:** `rio-dashboard/src/lib/__tests__/graphLayout.test.ts` → `../../../../rio-test-support/golden/derivation_statuses.json` (4 levels up to workspace root, then down). If vitest's module resolution complains, use `path.resolve(__dirname, '../../../../rio-test-support/golden/...')` with `fs.readFileSync`.

### T4 — `docs(dashboard):` graphLayout.ts — cross-ref comment at STATUS_CLASS

MODIFY [`rio-dashboard/src/lib/graphLayout.ts`](../../rio-dashboard/src/lib/graphLayout.ts) at `:38-42` (the comment above `STATUS_CLASS`). Currently says "Mirrors rio-scheduler/src/state/derivation.rs:138-152 (as_str)" — extend with the enforcement pointer:

```typescript
// DerivationStatus string → CSS class. Mirrors
// rio-scheduler/src/state/derivation.rs:138-152 (as_str). The golden
// snapshot at rio-test-support/golden/derivation_statuses.json is the
// cross-language source of truth — both the Rust snapshot test
// (derivation.rs cfg(test) mod status_snapshot) and the vitest
// cross-check (graphLayout.test.ts) fail if a new variant isn't
// plumbed here. Completed is green; in-flight ...
```

Same comment extension at `:70-72` above `SORT_RANK`.

## Exit criteria

- `test -f rio-test-support/golden/derivation_statuses.json` → exists with 11 entries
- `cargo nextest run -p rio-scheduler derivation_status_snapshot_is_current` → 1 passed
- `cargo nextest run -p rio-scheduler all_const_is_exhaustive` → 1 passed
- `pnpm --filter rio-dashboard test -- graphLayout -t 'cross-language'` → ≥2 passed
- **Mutation check:** add a 12th variant `Quarantined` to `DerivationStatus` + `as_str` arm + `ALL` const but NOT to `STATUS_CLASS` → (a) `all_const_is_exhaustive` compile-error on the match; (b) after fixing match + snapshot: vitest `STATUS_CLASS covers every...` FAILS with "Rust emitted 'quarantined' but STATUS_CLASS has no explicit arm". Revert after confirming.
- `grep 'export const SORT_RANK' rio-dashboard/src/lib/graphLayout.ts` → 1 hit (T3 exported it for testability)
- `grep 'terminal' rio-test-support/golden/derivation_statuses.json | wc -l` → 11 (DISPATCH-NOTE: golden is `{status,terminal}` shape)
- `pnpm --filter rio-dashboard test -- graphLayout -t 'TERMINAL matches Rust'` → 1 passed (DISPATCH-NOTE vitest assert)
- **TERMINAL mutation:** change Rust `is_terminal()` to include `Self::Failed` → snapshot test FAILS (golden drifts) → after updating golden: vitest `TERMINAL matches Rust` FAILS ("Rust says 'failed'.is_terminal()=true, TS TERMINAL.has=false"). Revert.
- `/nbr .#ci` green

## Tracey

References existing markers:
- `r[sched.state.transitions]` — T1's snapshot test adds a verify site (the snapshot IS the state-machine's string representation; keeping it current is part of transition-validity). The annotation at `:315` in [`scheduler.md`](../../docs/src/components/scheduler.md) describes the valid transition matrix; the `as_str` mapping serializes states for PG/wire.
- `r[dash.graph.degrade-threshold]` — T3/T4 touch the same `graphLayout.ts` file but NOT the degrade-threshold behavior at `:31`. Mentioned for context — no annotation change.

## Files

```json files
[
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "T1: cfg(test) ALL: [Self; 11] const + status_snapshot mod (2 tests: snapshot-match + exhaustive-witness). r[verify sched.state.transitions]."},
  {"path": "rio-test-support/golden/derivation_statuses.json", "action": "NEW", "note": "T2: 11-string pretty-printed JSON array, one status per line. Cross-language source of truth."},
  {"path": "rio-dashboard/src/lib/__tests__/graphLayout.test.ts", "action": "MODIFY", "note": "T3: import golden JSON; cross-language superset checks for STATUS_CLASS + SORT_RANK. Needs resolveJsonModule or fs.readFileSync."},
  {"path": "rio-dashboard/src/lib/graphLayout.ts", "action": "MODIFY", "note": "T3: export SORT_RANK (1-token: const→export const at :73). T4: comment extension at :38-42 + :70-72 pointing at golden snapshot enforcement."},
  {"path": "rio-dashboard/tsconfig.json", "action": "MODIFY", "note": "T3: +resolveJsonModule:true IF needed (check at dispatch — may already be set)"}
]
```

```
rio-scheduler/src/state/
└── derivation.rs              # T1: ALL const + snapshot tests
rio-test-support/golden/
└── derivation_statuses.json   # NEW — T2
rio-dashboard/src/lib/
├── graphLayout.ts             # T3: export SORT_RANK; T4: comments
└── __tests__/
    └── graphLayout.test.ts    # T3: cross-language superset tests
```

## Dependencies

```json deps
{"deps": [400], "soft_deps": [252, 280, 394], "note": "rev-p400 test-gap (discovered_from=400). HARD-dep P0400: T1 added the 'skipped' arm to STATUS_CLASS+SORT_RANK and :116-124 test — without it, T3's intended-set check fails immediately on 'skipped'. Soft-dep P0252 (DONE — added Skipped to Rust; the motivating miss). Soft-dep P0280 (DONE — shipped graphLayout.ts). Soft-dep P0394 (spec-metrics include_str!-derive pattern — T1's include_str! on golden JSON is the same pattern, adjacent in CARE notes). The reviewer noted 'Fix option: build.rs that parses derivation.rs … or a nextest that compares … to strum::IntoEnumIterator' — this plan picks option-(c) shared-snapshot, which is the lighter-weight middle ground (no strum dep, no codegen toolchain, both sides fail loud). derivation.rs count=moderate (P0252/P0307/P0399 all touch it) — T1's edits are cfg(test)-only after :637, additive. graphLayout.ts count=low (P0280/P0400/P0304-T170 touch different regions: T170 at :118-122, P0400 at :43-85, T4 here at :38-42+:70-72 comment-only). graphLayout.test.ts count=low (P0400-T1 touched :112-128; T3 here appends after :128, additive)."}
```

**Depends on:** [P0400](plan-0400-graph-page-skipped-worker-race.md) — `skipped` arm present in both maps.
**Conflicts with:** [`derivation.rs`](../../rio-scheduler/src/state/derivation.rs) — [P0307](plan-0307-wire-poisonconfig-retrypolicy-scheduler-toml.md) may add `Deserialize` derive to `PoisonConfig` at `:596`; T1 here is at `:47` (impl block) + `:637+` (cfg(test) mod). Non-overlapping. [`graphLayout.ts`](../../rio-dashboard/src/lib/graphLayout.ts) — [P0304-T170](plan-0304-trivial-batch-p0222-harness.md) edits `:118-122` stale comment; T3+T4 here at `:73` + `:38-42/:70-72`. Non-overlapping.
