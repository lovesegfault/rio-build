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

### T11 — `docs:` integration.md Build CRD section — DELETED CRD (USER-FACING)

**USER-FACING PRIORITY.** MODIFY [`docs/src/integration.md`](../../docs/src/integration.md) at `:84-100` — the "Kubernetes: Build CRD" section with a full `kubectl apply` YAML example for a CRD that [P0294](plan-0294-build-crd-full-rip.md) deletes. Anyone following the doc post-P0294 gets `error: the server doesn't have a resource type "builds"`.

Replace the section with the gRPC-direct submission pattern (which is what replaces Build CRD). Check P0294's plan doc for what the replacement flow is — likely `grpcurl SubmitBuild` or `rio-cli submit`. The `nix-eval-jobs` guidance at `:82` is still valid; just the CRD apply example is dead.

### T12 — `docs:` failure-modes.md Build CRD behavior refs

MODIFY [`docs/src/failure-modes.md`](../../docs/src/failure-modes.md) at `:10` and `:15`:
- `:10` "Build CRD controllers reconnect via `WatchBuild(build_id, since_sequence=last_seen)`" — delete "Build CRD controllers"; gateways reconnect the same way.
- `:15` "no Build CRD status updates" — delete this clause from the controller-down impact list.

### T13 — `docs:` phase4a.md 10 OPEN rows → MOOT-P0294

MODIFY [`docs/src/remediations/phase4a.md`](../../docs/src/remediations/phase4a.md) at `:1474` (overlaps T3's territory — coordinate) — 10 OPEN rows reference `rio-controller/src/reconcilers/build.rs` which P0294 deletes. Flip `OPEN` → `MOOT-P0294`. Also add a tombstone note to [`03-controller-stuck-build.md`](../../docs/src/remediations/phase4a/03-controller-stuck-build.md) header: "MOOT as of P0294 — Build CRD removed. Kept for provenance."

Also sweep for scattered `build.rs:NNN` line refs across `docs/` — `grep -rn 'reconcilers/build.rs' docs/` and tag each with `(file deleted in P0294)`.

### T14 — `docs:` 09-build-timeout-cgroup-orphan.md Build CR refs

MODIFY [`docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md`](../../docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md) at `:443`, `:618`, `:695` — references "Build CR status" and "Build CR conditions" in test-script prose. [P0289](plan-0289-port-specd-unlanded-test-trio.md) owns the test-port; these doc refs should say "build status (via `rio-cli builds` or `QueryBuildStatus` gRPC)" instead. Coordinate: P0289 may want to fix these alongside its test port — leave a `TODO(P0289)` if this lands first.

### T15 — `docs:` P0294 stale-comment sweep (5 files)

MODIFY per the P0294 review — stale comments referencing Build CRD behavior:
- [`rio-controller/Cargo.toml:9`](../../rio-controller/Cargo.toml) — likely a feature or dep comment
- [`rio-controller/src/main.rs:27`](../../rio-controller/src/main.rs) — module import comment
- [`nix/tests/default.nix:102`](../../nix/tests/default.nix) — test-registration comment
- [`flake.nix:534`](../../flake.nix) — VM-test or check-list comment
- [`nix/tests/scenarios/lifecycle.nix:650`](../../nix/tests/scenarios/lifecycle.nix) — subtest comment (P0294 deletes the subtest at `:518` onward; `:650` is inside that block so may disappear with P0294's rip — verify at dispatch)

Re-grep at dispatch — these are p294-worktree line numbers and P0294 itself deletes ~2030 LoC so post-merge line numbers shift dramatically.

### T16 — `docs:` gateway.md + data-flows.md + proto.md wopSetOptions 3-doc cluster

**P0215 proved ssh-ng never sends `wopSetOptions`.** Three docs still claim it does in present tense:

MODIFY [`docs/src/components/gateway.md`](../../docs/src/components/gateway.md) at `:45` — "`wopSetOptions` is **mandatory** as the first opcode after handshake --- Nix always sends it before any other operation." There's already a correction at `:515` for the mandatory-first-opcode claim. Add an **ssh-ng caveat** at `:45`: "The **daemon-protocol** client (`ssh://`) sends `wopSetOptions`. The **ssh-ng** client does NOT (empirically verified P0215) — client-side `--max-silent-time`/`--timeout` are silently non-functional over ssh-ng until [P0310](plan-0310-gateway-client-option-propagation.md) lands a gateway-side path." Run `tracey bump` on `r[gw.opcode.set-options.field-order]`.

MODIFY [`docs/src/data-flows.md`](../../docs/src/data-flows.md) at `:10` and `:71` — sequence diagram shows `Client→GW: wopSetOptions` unconditionally. Add `(ssh:// only; ssh-ng skips)` annotation.

MODIFY [`docs/src/components/proto.md`](../../docs/src/components/proto.md) at `:214` — "`// Client build options propagated from wopSetOptions`" comment in the proto. Amend to: "Client build options. For ssh:// these propagate from wopSetOptions; for ssh-ng they're populated gateway-side (P0310) or fall back to worker config defaults (P0215)."

### T17 — `docs:` fod-proxy.nix "three layers" → two (ssh-ng caveat)

**CROSS-WORKTREE — fix before P0243 merges, or fold into P0243's fix-impl round.** The file doesn't exist on sprint-1 yet; it's in the p243 worktree at [`fod-proxy.nix:199-201`](../../nix/tests/scenarios/fod-proxy.nix).

The comment says: "Three layers: wopSetOptions bounds (sent to the remote store via wopSetOptions). wget -T 15 inside the FOD bounds the network layer. `timeout 90`: shell-level."

P0215 proves the wopSetOptions layer doesn't exist for ssh-ng. Correct to: "Two layers: `wget -T 15` bounds the network leg inside the FOD builder. `timeout 90`: shell-level backstop. (wopSetOptions bounds would be a third layer, but ssh-ng doesn't send it — P0215.)"

**If P0243 has already merged when this dispatches:** apply directly to `nix/tests/scenarios/fod-proxy.nix:199-201` on sprint-1.
**If P0243 hasn't merged:** coordinator should send this fix as a message to the P0243 fix-impl round.

### T18 — `docs:` default.nix denied.invalid → k3s-agent

**P0243 worktree file.** [`nix/tests/default.nix:314`](../../nix/tests/default.nix) (p243 worktree) comment says "the denied case (`denied.invalid`) is a clean" — but [`3cb2c442`](https://github.com/search?q=3cb2c442&type=commits) changed the denied-case hostname to `k3s-agent` (the worker node's hostname, which squid's allowlist doesn't include). One-word fix. Same cross-worktree caveat as T17.

### T19 — `docs:` StoreServer → StoreServiceImpl (comment + plan doc)

MODIFY [`rio-store/src/main.rs`](../../rio-store/src/main.rs) at `:632` — test doc comment says "The 'value reaches StoreServer' half of this roundtrip" — the struct is `StoreServiceImpl` (confirmed: 7 other refs in the same file use the correct name). [P0218](plan-0218-nar-buffer-config.md)'s plan doc also uses the wrong name — find and correct.

### T20 — `docs:` dashboard.md SchedulerService.GetBuildLogs → AdminService

MODIFY [`docs/src/components/dashboard.md`](../../docs/src/components/dashboard.md) at `:22` — table row says `SchedulerService.GetBuildLogs` but the RPC is in [`admin.proto:21`](../../rio-proto/proto/admin.proto) under `AdminService`. One-word fix.

### T10 — `docs:` plan-0222 metric-name corrections (P0222)

MODIFY [`.claude/work/plan-0222-grafana-dashboards.md`](plan-0222-grafana-dashboards.md) at `:18`, `:20`, `:30`, `:33`, `:44-47`, `:55`, `:56` — the plan's T1-T4 tables reference **9 nonexistent metric names**. Plan line 5 says "DO NOT invent metric names"; line 22 says grep-verify. None of the listed names appear in `rio-*/src/`, [`observability.md`](../../docs/src/observability.md), or any other plan doc. The implementer correctly substituted per exit-criterion 3 and shipped what exists ([`6b723def`](https://github.com/search?q=6b723def&type=commits)). The plan doc should record what shipped, not what was guessed at planning time.

| Task | Plan said | Shipped (per [`observability.md`](../../docs/src/observability.md)) |
|---|---|---|
| T1 :18 | `rio_scheduler_derivations_completed_total` | `rio_scheduler_builds_total` (labeled `outcome`) |
| T1 :20 | `rio_scheduler_builds_by_status` | `rio_scheduler_builds_total` by `outcome` |
| T2 :30 | `rio_controller_workerpool_replicas{class}` | `rio_controller_workerpool_replicas{pool}` (label is `pool` not `class`) |
| T2 :33 | `rio_scheduler_ready_queue_depth` | `rio_scheduler_class_queue_depth` |
| T3 :44 | `rio_store_cache_hits_total` / `_misses_total` | check shipped [`store-health.json`](../../infra/helm/grafana/store-health.json) — 4 names flagged by validator |
| T4 :55 | `rio_scheduler_dispatch_latency` | `rio_scheduler_assignment_latency` |
| T4 :56 | `rio_scheduler_ready_queue_depth` | `rio_scheduler_derivations_queued` + `_derivations_running` (split gauge) |

Rewrite the T1-T4 PromQL tables to match shipped JSONs. Keep the prose ("verify each metric exists") — it was correct instruction; the table rows were wrong.

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
- `grep 'derivations_completed_total\|builds_by_status\|ready_queue_depth\|dispatch_latency' .claude/work/plan-0222*.md` → 0 hits (T10: nonexistent names removed)
- `grep 'kind: Build' docs/src/integration.md` → 0 hits (T11: CRD YAML removed)
- `grep 'Build CRD' docs/src/failure-modes.md` → 0 hits (T12)
- `grep 'MOOT-P0294' docs/src/remediations/phase4a.md` → ≥10 hits (T13: rows flipped)
- `grep 'ssh-ng' docs/src/components/gateway.md | grep -i 'wopSetOptions\|does NOT'` → ≥1 hit (T16: caveat present)
- `grep 'StoreServer' rio-store/src/main.rs` → 0 hits (T19)
- `grep 'SchedulerService.GetBuildLogs' docs/src/components/dashboard.md` → 0 hits (T20)

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
  {"path": ".claude/work/plan-0025-nar-streaming-refactor.md", "action": "MODIFY", "note": "T9: HashingReader cfg(test)-gated note (P0025)"},
  {"path": ".claude/work/plan-0222-grafana-dashboards.md", "action": "MODIFY", "note": "T10: rewrite T1-T4 PromQL tables to match shipped metric names (P0222)"},
  {"path": "docs/src/integration.md", "action": "MODIFY", "note": "T11: delete Build CRD kubectl-apply section :84-100 (USER-FACING — CRD deleted by P0294)"},
  {"path": "docs/src/failure-modes.md", "action": "MODIFY", "note": "T12: remove Build CRD refs at :10 :15"},
  {"path": "docs/src/remediations/phase4a/03-controller-stuck-build.md", "action": "MODIFY", "note": "T13: MOOT-P0294 tombstone header"},
  {"path": "docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md", "action": "MODIFY", "note": "T14: Build CR → rio-cli/gRPC refs at :443 :618 :695; leave TODO(P0289)"},
  {"path": "rio-controller/Cargo.toml", "action": "MODIFY", "note": "T15: stale Build CRD comment at :9"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "T15: stale Build CRD comment at :27"},
  {"path": "nix/tests/default.nix", "action": "MODIFY", "note": "T15: stale comment :102; T18: denied.invalid → k3s-agent :314 (p243-worktree line — CROSS-WORKTREE caveat)"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T16: ssh-ng wopSetOptions caveat at :45 + tracey bump r[gw.opcode.set-options.field-order]"},
  {"path": "docs/src/data-flows.md", "action": "MODIFY", "note": "T16: (ssh:// only) annotation at :10 :71"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "T16: wopSetOptions comment at :214 — ssh-ng caveat"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T17: three layers → two at :199-201 (CROSS-WORKTREE — p243; fix before merge or via P0243 fix-impl)"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T19: StoreServer → StoreServiceImpl at :632"},
  {"path": ".claude/work/plan-0218-nar-buffer-config.md", "action": "MODIFY", "note": "T19: same StoreServer typo in plan doc"},
  {"path": "docs/src/components/dashboard.md", "action": "MODIFY", "note": "T20: SchedulerService.GetBuildLogs → AdminService at :22"}
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
.claude/work/plan-0222*.md         # T10
```

## Dependencies

```json deps
{"deps": [204, 222, 294], "soft_deps": [215, 218, 243, 289], "note": "retro §Doc-rot (T1-T10) + sprint-1 sink (T11-T20). T11-T15 depend on P0294 (Build CRD rip — landmarks must be gone before we reference their absence). T16-T18 depend on P0215 finding (ssh-ng wopSetOptions). T17/T18 CROSS-WORKTREE with p243 — fix before P0243 merges or fold into P0243 fix-impl. T14 coordinates with P0289 (same file, leave TODO). No behavior change — docs/comments/plan-doc errata only."}
```

**Depends on:** [P0204](plan-0204-phase4b-doc-sync.md) — phase4b fan-out root. [P0294](plan-0294-build-crd-full-rip.md) — T11-T15 reference the CRD's absence; must land after the rip.
**Soft-deps:** [P0215](plan-0215-max-silent-time.md) (T16-T18 cite its finding), [P0218](plan-0218-nar-buffer-config.md) (T19 fixes its plan doc), [P0243](plan-0243-vm-fod-proxy-scenario.md) (T17/T18 cross-worktree), [P0289](plan-0289-port-specd-unlanded-test-trio.md) (T14 same file).
**Conflicts with:** [`security.md`](../../docs/src/security.md) also touched by [P0286](plan-0286-privileged-hardening-device-plugin.md) (spec additions — different section). [`gateway.md`](../../docs/src/components/gateway.md) count=21 — T16 edits `:45`; [P0310](plan-0310-gateway-client-option-propagation.md) T4 edits `:62` — non-overlapping. [`phase4a.md`](../../docs/src/remediations/phase4a.md) T3+T13 overlap — same file, coordinate. [`fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) T17 + [P0308](plan-0308-fod-buildresult-propagation-namespace-hang.md) + [P0309](plan-0309-helm-template-fodproxyurl-workerpool.md) — three plans, non-overlapping sections (`:199`, `:285`, `:103`).
