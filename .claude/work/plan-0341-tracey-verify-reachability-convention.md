# Plan 0341: Tracey `r[verify]` reachability — marker-exists ≠ test-runs

Coordinator-surfaced structural gap from the [P0289](plan-0289-port-specd-unlanded-test-trio.md) post-merge review. A col-0 `# r[verify X]` comment in `nix/tests/scenarios/*.nix` satisfies `tracey query untested` even when the fragment the marker describes is never wired into [`default.nix`](../../nix/tests/default.nix)'s `subtests = [...]` list. Tracey sees the file, parses the marker, marks the rule "tested" — but the NixOS test driver never executes that subtest. **Marker-exists ≠ test-runs.**

P0289 T1 landed a `build-timeout` fragment in [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) with col-0 `r[verify]` markers. The subtest name was wired correctly in that case — but nothing structurally *forced* that. A future plan could add `# r[verify X]` above a new fragment, forget to extend `subtests = [...]`, and tracey would show X as covered while `.#ci` never runs the test. Silent green.

Two fix shapes: **(a) parser change** — have tracey cross-check each scenario-file marker against a subtest-name registry in `default.nix`; **(b) convention change** — move `r[verify]` markers FROM scenario fragments TO `default.nix`, placed at the `subtests = [...]` entry that wires the fragment. Coordinator guidance: **(b) is simpler.** If the marker lives at the wiring point, it's structurally impossible to have a marker-without-wiring (the marker IS at the wiring). Tracey doesn't change; only where markers live changes.

## Tasks

### T1 — `docs:` convention update — markers at subtests entry, not fragment header

MODIFY [`CLAUDE.md`](../../CLAUDE.md) — add to the `## Spec traceability (tracey)` section (after the "When adding spec text" paragraph):

```markdown
**VM-test `r[verify]` placement:** for NixOS VM tests under `nix/tests/`,
place `# r[verify ...]` markers in `default.nix` at the `subtests = [...]`
entry that wires the fragment — NOT in the scenario file's col-0 header
block. A marker in a scenario header tells tracey the rule is tested;
it does not tell tracey the fragment runs. A marker at the subtests entry
structurally couples the two: no wiring → no marker → tracey catches it.

```nix
subtests = [
  "gc-sweep"       # r[verify store.gc.two-phase] r[verify store.gc.tenant-retention]
  "refs-end-to-end"  # r[verify worker.upload.references-scanned] r[verify worker.upload.deriver-populated]
];
```

Scenario-file header blocks MAY keep prose descriptions of what each
marker covers (those are useful for humans); they MUST NOT carry the
marker token itself.
```

Also MODIFY [`.claude/agents/rio-implementer.md`](../../.claude/agents/rio-implementer.md) — if it has a "tracey annotation placement" section, add the same guidance. If not, add a single line under the existing tracey mention: "VM-test `r[verify]`: at `default.nix` subtests entry, not scenario header. See the convention paragraph in `.claude/agents/rio-implementer.md`."

### T2 — `test(nix):` migrate existing scenario-header markers to default.nix

The fresh count is **29 col-0 markers** across 6 scenario files (lifecycle:8, scheduling:10, cli:5, leader-election:3, security:2, observability:1); `default.nix` currently has **0**. **P0260 adds more to security.nix** (p260 worktree review found `gw.jwt.dual-mode` at `:6` and possibly `:15,:19,:24` — re-grep col-0 count at dispatch; the actual count may be 30+). Two scheduling markers at [`scheduling.nix:684`](../../nix/tests/scenarios/scheduling.nix) and `:777` are NOT col-0 (they're inline at the subtest body) — those are correctly placed and stay.

MODIFY [`nix/tests/default.nix`](../../nix/tests/default.nix) — add trailing-comment markers to each `subtests = [...]` list entry:

| default.nix block | Scenario source | Markers to migrate |
|---|---|---|
| `vm-scheduling-core-standalone` `:210` | [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) `:23-45` | 8 of 10 (fanout→`worker.overlay.stacked-lower`, `worker.ns.order`; chunks→`store.inline.threshold`, `obs.metric.transfer-volume`; fuse-slowpath→`worker.fuse.lookup-caches`; etc. — grep-match subtest name to header prose) |
| `vm-scheduling-disrupt-standalone` `:227` | scheduling.nix `:31-33` | `obs.metric.{scheduler,worker,store}` attach to load-50drv (the metrics-coverage fragment) |
| `vm-security-standalone` `:247` | [`security.nix`](../../nix/tests/scenarios/security.nix) `:6,10` + p260 adds | `sec.boundary.grpc-hmac`, `store.tenant.narinfo-filter` + `gw.jwt.dual-mode` (P0260 adds at `:6` — p260 worktree ref; may also add at `:15,:19,:24`; re-grep col-0 count at dispatch). Single-fragment file; all at the `{...}` invocation |
| `vm-observability-standalone` `:281` | [`observability.nix`](../../nix/tests/scenarios/observability.nix) `:35` | `obs.metric.gateway` |
| `vm-lifecycle-core-k3s` `:309` | [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) `:27-92` | `obs.metric.controller`→health-shared; `worker.cancel.cgroup-kill`→cancel-cgroup-kill; `store.gc.two-phase`+`store.gc.tenant-retention`→gc-sweep; `worker.upload.{references-scanned,deriver-populated}`→refs-end-to-end; `ctrl.probe.named-service`+`ctrl.autoscale.skip-deleting`→reconciler-replicas |
| `vm-leader-election-k3s` (grep location) | [`leader-election.nix`](../../nix/tests/scenarios/leader-election.nix) `:21-27` | `sched.lease.{k8s-lease,graceful-release,deletion-cost}` |
| `vm-cli-standalone` (grep location) | [`cli.nix`](../../nix/tests/scenarios/cli.nix) `:13-17` | `sched.admin.{create-tenant,list-tenants,list-workers,list-builds,clear-poison}` |

**Format at destination:** inline trailing comment. Tracey's nix parser scans col-0 AND trailing comments (verify at dispatch with a spot-check: add one, `pkill tracey`, `tracey query rule X` should show the verify site moved). If tracey's nix parser is col-0-only, fall back to a col-0 block immediately above each `subtests = [...]`:

```nix
# r[verify store.gc.two-phase]
# r[verify store.gc.tenant-retention]
#   — gc-sweep fragment
subtests = [
  "health-shared"
  ...
  "gc-sweep"
  ...
];
```

The col-0-above-subtests form is weaker (one-step-removed from the list entry) but still structurally coupled: deleting the `subtests` list deletes the markers with it. Deleting a single entry from the list leaves the marker, but that's the same false-positive class as a Rust `#[test] fn` that `return`s early — a class tracey never claimed to catch.

### T3 — `test(nix):` strip marker tokens from scenario headers, keep prose

MODIFY each scenario file — the col-0 header blocks at the top describe what each marker covers (e.g., [`lifecycle.nix:78-84`](../../nix/tests/scenarios/lifecycle.nix) explains *why* refs-end-to-end proves `store.gc.two-phase`). That prose is useful. Only the `# r[verify X]` token line itself moves. Replace with a cross-reference:

Before:
```nix
# r[verify store.gc.two-phase]
#   refs-end-to-end pins ONLY the consumer, backdates both paths past
#   grace, sweeps, and asserts the dep SURVIVES. Proves mark's recursive
#   CTE actually walks narinfo."references" ...
```

After:
```nix
# store.gc.two-phase — verify marker at default.nix:subtests[refs-end-to-end]
#   refs-end-to-end pins ONLY the consumer, backdates both paths past
#   grace, sweeps, and asserts the dep SURVIVES. Proves mark's recursive
#   CTE actually walks narinfo."references" ...
```

The prose stays; the token moves; a breadcrumb remains. `grep 'r\[verify' nix/tests/scenarios/*.nix` after this should find ONLY the two inline-at-body markers in scheduling.nix (`:684`, `:777`) — those are correctly placed (at the assertion, not the header) and stay.

### T4 — `build(tracey):` config.styx glob narrowing — default.nix only

MODIFY [`.config/tracey/config.styx`](../../.config/tracey/config.styx) at `:54`. Current:
```
nix/tests/*.nix
```

This glob (whether tracey interprets `*` as recursive or not) matches at minimum `default.nix` and `common.nix`. After T2/T3, `r[verify]` markers in VM tests live ONLY in `default.nix`. Narrow to:
```
nix/tests/default.nix
```

**This is the enforcement step.** Post-narrowing, a marker mistakenly added to a scenario header is invisible to tracey — `tracey query untested` will still list the rule as untested, and the author will notice. Without T4, the convention is documented but not machine-enforced.

**Breakage check:** `common.nix` has no `r[verify]` markers (grep shows 0). The two inline markers at `scheduling.nix:684/:777` will become invisible — migrate them to default.nix too (attach to `"max-silent-time"` and `"setoptions-unreachable"` in the disrupt `subtests` list). After that, narrowing is safe.

## Exit criteria

- `/nixbuild .#ci` green
- `grep -c '^# r\[verify' nix/tests/scenarios/*.nix` → 0 (all col-0 header markers migrated)
- `grep -c 'r\[verify' nix/tests/default.nix` ≥ 29 (all migrated + the 2 from scheduling.nix inline)
- `grep 'r\[verify' nix/tests/scenarios/scheduling.nix` → 0 (inline markers at :684/:777 also migrated per T4)
- `grep 'nix/tests/\*.nix\|nix/tests/scenarios' .config/tracey/config.styx` → 0 (narrowed to explicit default.nix)
- `grep 'nix/tests/default.nix' .config/tracey/config.styx` → 1
- Kill tracey daemon (`ps aux | grep 'tracey daemon' | grep -v grep | awk '{print $2}' | xargs kill` — NOT `pkill -f` which kills the MCP sidecar too), then `nix develop -c tracey query rule store.gc.two-phase` → shows `verify` site at `nix/tests/default.nix:<line>` (not `scenarios/lifecycle.nix:78`)
- `nix develop -c tracey query validate` → `0 total error(s)` (no markers orphaned by the migration)
- `nix develop -c tracey query untested | wc -l` — same count before and after (migration is neutral; no rules gained or lost coverage). **Record before-count at dispatch start.**

## Tracey

No new domain markers. This plan changes WHERE `r[verify]` annotations live, not what spec rules exist. The 29 migrated markers reference existing domain rules:

- `r[store.gc.two-phase]`, `r[store.gc.tenant-retention]` — [`store.md:210,217`](../../docs/src/components/store.md)
- `r[worker.upload.references-scanned]`, `r[worker.upload.deriver-populated]`, `r[worker.cancel.cgroup-kill]`, `r[worker.overlay.stacked-lower]`, `r[worker.ns.order]`, `r[worker.fuse.lookup-caches]`, `r[worker.silence.timeout-kill]` — `docs/src/components/worker.md`
- `r[sched.lease.k8s-lease]`, `r[sched.lease.graceful-release]`, `r[sched.lease.deletion-cost]`, `r[sched.admin.*]` — [`scheduler.md`](../../docs/src/components/scheduler.md)
- `r[obs.metric.{scheduler,worker,store,gateway,controller,transfer-volume}]` — `docs/src/observability.md`
- `r[sec.boundary.grpc-hmac]`, `r[store.tenant.narinfo-filter]`, `r[store.inline.threshold]`, `r[ctrl.probe.named-service]`, `r[ctrl.autoscale.skip-deleting]`, `r[gw.opcode.set-options.propagation+2]`

Migration is annotation-relocation. `tracey query untested` before == after (the "same-count" exit criterion above enforces).

## Files

```json files
[
  {
    "path": ".claude/agents/rio-implementer.md",
    "action": "MODIFY",
    "note": "T1: cross-ref to CLAUDE.md convention (one line)"
  },
  {
    "path": "nix/tests/default.nix",
    "action": "MODIFY",
    "note": "T2: +29 r[verify] markers at subtests entries (trailing-comment OR col-0-above-list form, dispatch-time tracey-parser check decides)"
  },
  {
    "path": "nix/tests/scenarios/lifecycle.nix",
    "action": "MODIFY",
    "note": "T3: strip 8 col-0 marker tokens :27-:86, keep prose with breadcrumb"
  },
  {
    "path": "nix/tests/scenarios/scheduling.nix",
    "action": "MODIFY",
    "note": "T3: strip 8 col-0 tokens :23-:45; T4: also migrate inline :684 :777"
  },
  {
    "path": "nix/tests/scenarios/cli.nix",
    "action": "MODIFY",
    "note": "T3: strip 5 col-0 tokens :13-:17"
  },
  {
    "path": "nix/tests/scenarios/leader-election.nix",
    "action": "MODIFY",
    "note": "T3: strip 3 col-0 tokens :21-:27"
  },
  {
    "path": "nix/tests/scenarios/security.nix",
    "action": "MODIFY",
    "note": "T3: strip 2 col-0 tokens :6 :10"
  },
  {
    "path": "nix/tests/scenarios/observability.nix",
    "action": "MODIFY",
    "note": "T3: strip 1 col-0 token :35"
  },
  {
    "path": ".config/tracey/config.styx",
    "action": "MODIFY",
    "note": "T4: narrow nix/tests/*.nix \u2192 nix/tests/default.nix at :54 (enforcement step)"
  }
]
```

```
.claude/agents/rio-implementer.md  # T1: cross-ref
nix/tests/
├── default.nix                    # T2: +29 markers at subtests entries
└── scenarios/
    ├── lifecycle.nix              # T3: -8 tokens
    ├── scheduling.nix             # T3: -8 col-0, T4: -2 inline
    ├── cli.nix                    # T3: -5
    ├── leader-election.nix        # T3: -3
    ├── security.nix               # T3: -2
    └── observability.nix          # T3: -1
.config/tracey/config.styx         # T4: glob narrowing
```

## Dependencies

```json deps
{"deps": [289], "soft_deps": [304, 334, 260], "note": "P0289 discovered_from — T1's build-timeout fragment had this exact gap (markers without structural wiring-check). P0289 is DONE; dep is provenance. SOFT P0304-T31: also touches config.styx (adds 4 crate test dirs to test_include) — different glob block (T31 adds rio-*/tests globs, this narrows the nix/tests glob). Non-overlapping but same file; sequence either way. SOFT P0334: touches 9 scenario files (covTimeoutHeadroom let-binding extraction) — T3 here edits col-0 header comments, P0334 edits let-bindings, different regions. CLAUSE-4 PROFILE: T2/T3/T4 are comment-only + glob-only. The default.nix edit adds only # comments (no attrset changes) → behavioral-drv-identity. The config.styx edit invalidates only tracey-validate drv. lifecycle.nix count=17 and scheduling.nix count=13 are the only warm files; edits are col-0-comment-stripping, zero body intersection with testScript strings. SOFT P0260: adds gw.jwt.dual-mode marker (and possibly others) to security.nix col-0 headers — T2's migration table updated to include them (p260 worktree review; discovered_from=260). If P0260 merges before this plan, sweep its additions; if after, they become follow-on."}
```

**Depends on:** [P0289](plan-0289-port-specd-unlanded-test-trio.md) (DONE) — the gap manifested there. No blocking dep; [`default.nix`](../../nix/tests/default.nix), the scenario files, and [`config.styx`](../../.config/tracey/config.styx) all exist and are stable.

**Conflicts with:** [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) count=17 — but T3 only strips col-0 comment lines before `{`, no intersection with `testScript` bodies that other plans edit. [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) count=13 — same. [P0304](plan-0304-trivial-batch-p0222-harness.md) T31 also touches `config.styx` (adds crate test globs); non-overlapping lines in the same `test_include` block. [P0334](plan-0334-hoist-covTimeoutHeadroom-common-nix.md) touches 9 scenario files for `let`-binding extraction — different region than T3's header comments. Sequence-independent. **Default.nix comment-only edit → behavioral-drv-identity (hash differs, execution identical).**
