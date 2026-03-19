# Plan 0295: Doc-rot batch sweep — 9 errata from retrospective

**Retro §Doc-rot batch.** 11 doc-rot items found; P0128 (landmine) and P0160 (5-site) were carved out as standalone [P0292](plan-0292-scheduler-probe-comment-landmine.md) and [P0293](plan-0293-link-parent-5-site-doc-fix.md). These are the remaining 9. All are comment/doc updates, no behavior change. Skip-CI candidate except clippy (comment-only).

Full detail at [`done-plan-retrospective.md §Doc rot`](../notes/done-plan-retrospective.md).

## Entry criteria

- [P0204](plan-0204-phase4b-doc-sync.md) merged (phase4b fan-out root)

## Tasks

### T1 — `docs:` README.md dead attributes (P0068)

MODIFY [`README.md`](../../README.md) at `:98-102` — `nix build .#ci-local-fast` / `.#ci-fast` neither exists (retired [`89ba88fd`](https://github.com/search?q=89ba88fd&type=commits)). New contributor copying from README gets `attribute not found`. Also "30s fuzz" → "2min fuzz."

Collapse to single `nix build .#ci` block per retrospective.

### T2 — `docs:` flake.nix branch-coverage DO-NOT warning (P0143)

MODIFY [`flake.nix`](../../flake.nix) at `:250` and/or [`nix/coverage.nix`](../../nix/coverage.nix) header — `RUSTFLAGS = "-C instrument-coverage"` has NO warning about branch coverage. `-Z coverage-options=branch` was tried ([`8126dcf`](https://github.com/search?q=8126dcf&type=commits)), segfaulted `llvm-cov export` at ~15GB RSS with 20+ object files, reverted ([`4c8365d`](https://github.com/search?q=4c8365d&type=commits)). Diagnostic info lives ONLY in commit [`395c0493`](https://github.com/search?q=395c0493&type=commits) — which was itself reverted.

Add: `# Do NOT add -Z coverage-options=branch — llvm-cov export segfaults at ~15GB RSS with 20+ object files (tried 8126dcf, reverted 4c8365d, diagnostic in 395c049).`

### T3 — `docs:` remediation-index stale OPEN counts (P0200)

MODIFY [`docs/src/remediations/phase4a.md`](../../docs/src/remediations/phase4a.md) at `:1457-1588` — index table says 129 OPEN, 1 FIXED — but ≥6 spot-checked entries are fixed (wkr-cancel-flag-stale, common-redact-password-at-sign, common-otel-empty-endpoint, cfg-zero-interval-tokio-panic, wkr-fod-flag-trust, gw-temp-roots-unbounded-insert-only). Index was INPUT to 21-plan sweep; nobody closed the loop.

Sweep table against 21 per-remediation docs + `plan-0200 §Remainder`: flip OPEN→FIXED(rem-NN). ~5-10 genuinely-open items stay OPEN with `TODO(phase4b)`.

### T4 — `docs:` synth_db.rs "deferred" → "by design" (P0051)

MODIFY [`rio-worker/src/synth_db.rs`](../../rio-worker/src/synth_db.rs) at `:13`, `:93`, `:158` — "Realisations: currently empty (CA support deferred)" implies temporary + globally-deferred. Both wrong. Table empty PRE-build is **permanent by design** (nix-daemon INSERTs post-CA-build; rio never populates — scheduler resolves CA inputs before dispatch). CA landed at store/gateway in 2c.

Rewrite to: "empty pre-build by design — nix-daemon INSERTs here post-CA-build; rio never populates (scheduler resolves CA inputs before dispatch per phase5.md)."

**Also:** add note at appropriate spot (check at impl time) that `drv_content` must never be empty for CA-resolved drvs (per retrospective — `executor/mod.rs:312` + `derivation.rs:352` fallbacks fetch unresolved .drv).

### T5 — `docs:` auth.rs TODO retag + security.md bearer-token strikethrough (P0154)

MODIFY [`rio-store/src/cache_server/auth.rs`](../../rio-store/src/cache_server/auth.rs):
- `:36` retag `TODO(phase4b)` → `TODO(phase5)` (phase4b.md deliberately omits; `security.md:82` + `phase5.md:19` assign to Phase 5)
- `:7` "in 4b" → "in Phase 5"

MODIFY [`docs/src/security.md`](../../docs/src/security.md) at `:69` — lists "Bearer token authentication" as not-yet-implemented. It shipped (`r[impl store.cache.auth-bearer]`). Strike from the list.

### T6 — `docs:` plan-0113 fabricated claims → actual (P0113)

MODIFY [`.claude/work/plan-0113-closure-size-estimator-fallback.md`](plan-0113-closure-size-estimator-fallback.md) at `:9`, `:19` — claims `MEMORY_MULTIPLIER` constant + `HistoryEntry.input_closure_bytes` field. **Neither ever existed in code** (`git log -S` returns only the backfill doc commit itself). Fabricated by `rio-backfill-clusterer`. Actual commit [`f343eb9`](https://github.com/search?q=f343eb9&type=commits) is DURATION proxy only.

Rewrite to describe DURATION proxy (`CLOSURE_BYTES_PER_SEC_PROXY` 10MB/s, `CLOSURE_PROXY_MIN_SECS` 5s floor). Delete fabricated claims.

### T7 — `docs:` plan-0005 "intentional" → "WRONG — bug #11" (P0005)

MODIFY [`.claude/work/plan-0005-live-daemon-golden-conformance.md`](plan-0005-live-daemon-golden-conformance.md) at `:33`, `:51`, `:63` — "Intentional divergence: wopNarFromPath sends STDERR_WRITE — both work, more robust." **All false.** Bug #11, fixed [`132de90b`](https://github.com/search?q=132de90b&type=commits) (140 commits after phase-1b). Causes `error: no sink`. `gateway.md:76` already has Historical note.

Append after `:51`: `[WRONG — bug #11, fixed 132de90b in phase 2a: Nix client's processStderr() passes no sink → error: no sink. See gateway.md:76.]`. Strike "intentional" on `:33`, `:63`.

### T8 — `docs:` plan-0076 test-rename forward-pointer (P0076)

MODIFY [`.claude/work/plan-0076-figment-config.md`](plan-0076-figment-config.md) at `:25` — describes `config_defaults_match_phase2a()` test. [`2ab2d22e`](https://github.com/search?q=2ab2d22e&type=commits) (P0097) renamed to `config_defaults_are_stable` and FIXED three dangerous defaults.

Append: `(2ab2d22e/P0097 later renamed test → config_defaults_are_stable, fixed 3 dangerous defaults. fuse_passthrough guard remains.)`

### T9 — `docs:` plan-0025 HashingReader cfg(test)-gated note (P0025)

MODIFY [`.claude/work/plan-0025-nar-streaming-refactor.md`](plan-0025-nar-streaming-refactor.md) at `:58` — "HashingReader — phase 2 uses verbatim." `HashingReader` was `#[cfg(test)]`-gated at [`68571efa`](https://github.com/search?q=68571efa&type=commits) (wrapping already-accumulated Vec doubled peak memory, ~8GiB for 4GiB NAR). `FramedStreamReader` survived.

Append: `(Later: HashingReader cfg(test)-gated at 68571efa — gRPC chunk accumulation already buffers into Vec; NarDigest::from_bytes on slice is production path. FramedStreamReader survived.)`

## Exit criteria

- `/nbr .#ci` green (clippy-only gate; no behavior change)
- `grep 'ci-fast\|ci-local-fast' README.md` → 0 hits
- `grep 'coverage-options=branch' flake.nix nix/coverage.nix` → ≥1 hit (the DO-NOT comment)
- `grep -c 'OPEN' docs/src/remediations/phase4a.md` — significantly reduced from 129
- `grep 'CA support deferred' rio-worker/src/synth_db.rs` → 0 hits
- `grep 'TODO(phase4b)' rio-store/src/cache_server/auth.rs` → 0 hits
- `grep 'Bearer token authentication' docs/src/security.md | grep -v 'r\['` → 0 hits in not-yet-implemented context
- `grep 'MEMORY_MULTIPLIER\|input_closure_bytes' .claude/work/plan-0113*.md` → 0 hits
- `grep 'Intentional divergence' .claude/work/plan-0005*.md` → 0 hits (or fenced in `[WRONG]` erratum)

## Tracey

No marker changes. All 9 items are errata in plan docs, README, code comments. `r[sched.trace.assignment-traceparent]` is NOT touched here (that was P0160's item, carved out to P0293).

## Files

```json files
[
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: DO-NOT branch-coverage comment at :250 (P0143)"},
  {"path": "nix/coverage.nix", "action": "MODIFY", "note": "T2: DO-NOT branch-coverage comment in header (P0143)"},
  {"path": "docs/src/remediations/phase4a.md", "action": "MODIFY", "note": "T3: sweep index OPEN→FIXED(rem-NN) at :1457-1588 (P0200)"},
  {"path": "rio-worker/src/synth_db.rs", "action": "MODIFY", "note": "T4: 'deferred'→'by design' at :13,:93,:158 + drv_content-nonempty note (P0051)"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "T5: TODO(phase4b)→TODO(phase5) at :36, :7 (P0154)"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "T5: strike Bearer token from not-yet-implemented at :69 (P0154)"},
  {"path": ".claude/work/plan-0113-closure-size-estimator-fallback.md", "action": "MODIFY", "note": "T6: delete fabricated MEMORY_MULTIPLIER/input_closure_bytes, describe DURATION proxy (P0113)"},
  {"path": ".claude/work/plan-0005-live-daemon-golden-conformance.md", "action": "MODIFY", "note": "T7: [WRONG — bug #11] erratum at :51, strike 'intentional' :33,:63 (P0005)"},
  {"path": ".claude/work/plan-0076-figment-config.md", "action": "MODIFY", "note": "T8: forward-pointer to 2ab2d22e rename (P0076)"},
  {"path": ".claude/work/plan-0025-nar-streaming-refactor.md", "action": "MODIFY", "note": "T9: HashingReader cfg(test)-gated note (P0025)"}
]
```

**Root-level file (outside Files-fence validator pattern):** `README.md` MODIFY at repo root — T1 dead `.#ci-fast` attrs → single `.#ci` block. Zero collision risk.

```
README.md                          # T1 (root-level)
flake.nix                          # T2
nix/coverage.nix                   # T2
docs/src/remediations/phase4a.md   # T3
rio-worker/src/synth_db.rs         # T4
rio-store/src/cache_server/auth.rs # T5
docs/src/security.md               # T5
.claude/work/plan-00{05,25,76,113}*.md  # T6-T9
```

## Dependencies

```json deps
{"deps": [204], "soft_deps": [], "note": "retro §Doc-rot — 9 errata remaining after P0128(→P0292)/P0160(→P0293) carved out. No behavior change. P0068 README dead attrs, P0143 branch-cov DO-NOT, P0200 index stale OPEN, P0051 synth_db permanent-by-design, P0154 retag+strikethrough, P0113 fabricated claims, P0005 bug#11 erratum, P0076/P0025 forward-pointers."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root.
**Conflicts with:** [`security.md`](../../docs/src/security.md) also touched by [P0286](plan-0286-privileged-hardening-device-plugin.md) (spec additions — different section). Everything else is plan-doc errata and low-traffic code comments. Low collision risk.
