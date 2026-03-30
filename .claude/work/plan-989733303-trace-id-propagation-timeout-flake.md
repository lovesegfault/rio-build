# Plan 989733303: trace-id-propagation 600s timeout flake — keynes-load-aware

`vm-test-run-rio-observability` subtest `trace-id-propagation` hit the 600s
`globalTimeout` at
[`nix/tests/scenarios/observability.nix:54`](../../nix/tests/scenarios/observability.nix)
during [P0499](plan-0499-sizeclass-admin-cli-table.md)'s CI retry 2 of 3.
sprint-1's drv was cached green — P0499 differed only via `-source` rehash
(no observability-code change). No assertion failure: the test ran out of
clock before the OTLP collector file had the expected spans. Timing under
remote-builder load on keynes, not a correctness bug.

The comment at [`:52-53`](../../nix/tests/scenarios/observability.nix)
budgets "3 sequential builds (~5s each under VM) + OTLP batch flush
interval (~5s) + VM boot overhead" → 600s "generous". That budget is right
for an idle keynes. Under concurrent CI runs (the 2026-03-30 load profile
that also produced the P0501 `Killed` OOM), VM I/O slows and the OTLP
batch-flush wait at the end of the test extends. 600s is not generous;
it's the P50.

Two routes. **Route A (prefer):** raise `globalTimeout` to 900s to match
sibling scenarios
([`dashboard.nix:42`](../../nix/tests/scenarios/dashboard.nix),
[`dashboard-gateway.nix:45`](../../nix/tests/scenarios/dashboard-gateway.nix),
[`fetcher-split.nix:77`](../../nix/tests/scenarios/fetcher-split.nix) —
all 900s). **Route B (if 900s proves insufficient):** add an explicit
OTLP-flush poll loop with its own sub-timeout instead of a single sleep,
so the test succeeds as soon as spans appear rather than waiting a fixed
interval. Route A is a one-line change and aligns with siblings; Route B
is structural and only worth it if this re-flakes at 900s.

## Tasks

### T1 — `test(observability):` globalTimeout 600→900 — match sibling scenarios

MODIFY [`nix/tests/scenarios/observability.nix:54`](../../nix/tests/scenarios/observability.nix):

```nix
  # 3 sequential builds (~5s each under VM) + OTLP batch flush interval (~5s)
  # + VM boot overhead. 900s matches sibling scenarios (dashboard, fetcher-
  # split). Was 600s — P0499 hit timeout under keynes concurrent-CI load
  # (no assertion failure, just ran out of clock on OTLP flush wait). P989733303.
  globalTimeout = 900 + common.covTimeoutHeadroom;
```

The `+ common.covTimeoutHeadroom` stays (coverage-mode adds 300s on top,
per [`common.nix:77-79`](../../nix/tests/common.nix)).

### T2 — `fix(tooling):` add known-flake entry — retry-once gate

The implementer runs, from the worktree:

```bash
.claude/bin/onibus flake add \
  --test 'vm-test-run-rio-observability/trace-id-propagation' \
  --symptom '600s globalTimeout no assertion failure' \
  --root-cause 'timing under remote builder load' \
  --fix-owner P989733303 \
  --fix-description 'raise globalTimeout 600→900s to match sibling scenarios' \
  --retry Once
```

This writes to `.claude/known-flakes.jsonl` in the worktree. Commit it
alongside T1's `observability.nix` change — entry and fix merge atomically.
The `--fix-owner P989733303` gets string-replaced to the real plan number
at merge time.

**Removal:** if this plan's fix sticks (no re-flake at 900s for ≥20
merges), a future batch sweep removes the known-flake entry. If it
re-flakes at 900s, the entry stays and Route B (OTLP-flush poll loop)
becomes a follow-up plan. Don't remove preemptively.

## Exit criteria

- `grep 'globalTimeout = 900' nix/tests/scenarios/observability.nix` → 1 hit
- `grep 'P989733303\|keynes concurrent' nix/tests/scenarios/observability.nix` → ≥1 hit (comment names the cause)
- `jq -r 'select(.test == "vm-test-run-rio-observability/trace-id-propagation") | .fix_owner' .claude/known-flakes.jsonl` → `P989733303` (or the merged real number)
- `/nixbuild .#checks.x86_64-linux.vm-observability-standalone` green — proves 900s is enough for the idle-keynes case; loaded-keynes validation happens organically via CI retries on subsequent merges
- `/nixbuild .#ci` green

## Tracey

References existing markers:
- `r[obs.trace.scheduler-id-in-metadata]` — the `trace-id-propagation`
  subtest verifies this at
  [`nix/tests/default.nix:500`](../../nix/tests/default.nix). T1 keeps
  the verify site green under load; no annotation change.
- `r[sched.trace.assignment-traceparent]` — same subtest verifies this at
  [`nix/tests/default.nix:505`](../../nix/tests/default.nix). Same: T1
  keeps it green, no annotation change.

No new markers. Timeout tuning is test-infrastructure, not a spec
behavior.

## Known-flake entry

```json
{"test":"vm-test-run-rio-observability/trace-id-propagation","symptom":"600s globalTimeout no assertion failure","root_cause":"timing under remote builder load","fix_owner":"P989733303","fix_description":"raise globalTimeout 600→900s to match sibling scenarios","retry":"Once"}
```

## Files

```json files
[
  {"path": "nix/tests/scenarios/observability.nix", "action": "MODIFY", "note": "T1: globalTimeout 600→900 at :54 + comment naming P0499 keynes-load cause"},
  {"path": ".claude/known-flakes.jsonl", "action": "MODIFY", "note": "T2: add trace-id-propagation retry-once entry via onibus flake add"}
]
```

```
nix/tests/scenarios/
└── observability.nix    # T1: globalTimeout 600→900
.claude/
└── known-flakes.jsonl   # T2: retry-once entry
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "No plan dep. P0499 DISCOVERED this (hit the timeout on retry 2) but didn't introduce it — observability.nix:54 timeout predates P0499. discovered_from=coordinator (CI retry observation, not a review finding)."}
```

**Depends on:** none. The 600s timeout has been at
[`observability.nix:54`](../../nix/tests/scenarios/observability.nix)
since the scenario was written; it became load-sensitive when keynes
started running concurrent CI in 2026-03.

**Conflicts with:** none. `observability.nix` not in `onibus collisions
top 30`. `known-flakes.jsonl` is append-only JSONL.
