# Plan 993342103: build_timeout reachability — resolve wopSetOptions contradiction

Correctness investigation. [P0214](plan-0214-per-build-timeout.md) T3 was skipped (VM integration test for timeout→CancelSignal→SIGKILL never written). The reviewer flagged this as a test-gap, but digging reveals the gap may be irrelevant because **the feature may be CLI-unreachable in production.** Two pieces of in-tree evidence contradict each other:

**Claim A** — [`handler/mod.rs:82-90`](../../rio-gateway/src/handler/mod.rs) comment (under `max_silent_time()`):
> Empirically: `nix-build --option max-silent-time 5 --store ssh-ng://...` sends positional=0, overrides=[("max-silent-time", "5")].

If true, `--option build-timeout N` lands in `ClientOptions.overrides`, `build_timeout()` at [`:74-80`](../../rio-gateway/src/handler/mod.rs) extracts it, it propagates via `SubmitBuildRequest`, and [`worker.rs:584`](../../rio-scheduler/src/actor/worker.rs)'s `build.options.build_timeout > 0` check fires.

**Claim B** — P0215's independent verification (memory; validated during [P0215](plan-0215-max-silent-time.md) implementation):
> The `tracing::info!(... "wopSetOptions")` at [`opcodes_read.rs:226`](../../rio-gateway/src/handler/opcodes_read.rs) never fired across a full VM test run. ssh-ng clients never send wopSetOptions. All client-side build options silently non-functional.

If true, `handle_set_options` never runs, `ClientOptions` is never `Some(...)`, `build_timeout()` is never called (or called on a default where `overrides` is empty), and **P0214's entire per-build-timeout feature is dead code at the CLI level.** The `:570-609` block in `handle_tick` fires only if some non-CLI path (gRPC direct `SubmitBuildRequest`?) sets `build_timeout > 0`.

**These cannot both be true under the same conditions.** Either:
1. Claim A's "empirically" was under a different client path (not ssh-ng? a unit test with a hand-crafted wire stream?), or
2. Claim B's verification was narrower than stated (info-log didn't fire in the VM tests AS WRITTEN, but `--option` was never passed in those tests), or
3. ssh-ng behavior changed between P0215's verification and Claim A's writing (Nix version bump?)

This plan resolves the contradiction with a single instrumented VM run, then routes:
- **If Claim B holds** → P0214 is dead code for CLI users. Decide: delete, or fix the plumbing.
- **If Claim A holds** → P0215's finding was narrower than stated; update the memory/comments; unblock [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T11 (the VM test).

## Entry criteria

- [P0214](plan-0214-per-build-timeout.md) merged (DONE) — [`worker.rs:570-609`](../../rio-scheduler/src/actor/worker.rs) exists
- [P0215](plan-0215-max-silent-time.md) merged (DONE) — the [`opcodes_read.rs:226`](../../rio-gateway/src/handler/opcodes_read.rs) info-log exists

## Tasks

### T1 — `test(gateway):` instrumented VM run — does wopSetOptions fire under `--option`?

**This is the entire investigation.** MODIFY [`nix/tests/scenarios/scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) (TEMP — revert before commit unless T2 keeps it):

```python
# In scheduling.nix, after the existing subtests — TEMPORARY probe.
# The info-log at opcodes_read.rs:226 fires on wopSetOptions receipt.
# Default JSON log format, info+ level, so it's in journalctl.
with subtest("PROBE: does --option build-timeout reach the gateway?"):
    # Submit via ssh-ng with --option. If Claim A holds, this triggers
    # wopSetOptions → info-log fires → journalctl has it.
    client.succeed(
        "nix-build --option build-timeout 10 "
        "--option max-silent-time 5 "
        "-E 'derivation { name=\"probe\"; system=\"x86_64-linux\"; "
        "builder=\"/bin/sh\"; args=[\"-c\" \"echo ok > $out\"]; }' "
        "--store ssh-ng://rio-gateway"  # adjust to VM's gateway address
    )
    # Grep gateway journalctl for the info-log. Literal "wopSetOptions"
    # is the message at opcodes_read.rs:233.
    setopts_log = gateway.succeed(
        "journalctl -u rio-gateway --since '1 minute ago' -o json "
        "| jq -r 'select(.MESSAGE | contains(\"wopSetOptions\")) | .MESSAGE'"
    )
    # DO NOT assert here — this is a probe, not a gate. Print and inspect.
    print(f"PROBE RESULT: wopSetOptions log entries:\n{setopts_log!r}")
    # If non-empty: also check overrides_head field for "build-timeout".
    if setopts_log.strip():
        assert "build-timeout" in setopts_log, \
            f"wopSetOptions fired but build-timeout NOT in overrides: {setopts_log}"
```

Run `/nixbuild .#checks.x86_64-linux.vm-scheduling-k3s` (or whatever the scheduling.nix VM target is — grep `flake.nix` for it). **Read the probe output.**

### T2 — `fix:` route by T1 outcome

**T1 answer: wopSetOptions fired, `build-timeout` in overrides (Claim A holds):**

P0215's finding was narrower than recorded. The feature IS reachable. Actions:
1. MODIFY [`handler/mod.rs:82-90`](../../rio-gateway/src/handler/mod.rs) — the comment is correct; leave it. Add a note: "Verified under scheduling.nix VM @ <date>; P0215's earlier 'never fires' observation was under VM tests that didn't pass `--option`."
2. MODIFY `docs/src/components/gateway.md` near `r[gw.opcode.set-options.propagation]` at [`:61`](../../docs/src/components/gateway.md) — add a sentence clarifying the ssh-ng override path.
3. UNBLOCK [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T11 — the VM test is now meaningful. Keep the T1 probe subtest (rename from PROBE to a real test name, assert instead of print).
4. DELETE P0214 T3's stale `--option timeout` name from [plan-0214](plan-0214-per-build-timeout.md) — it's `--option build-timeout` (hyphen, matches the `:77` key).

**T1 answer: wopSetOptions did NOT fire (Claim B holds):**

P0214's feature is dead code for ssh-ng clients. The `handler/mod.rs:82-90` comment's "empirically" was wrong or under a different path. Actions:
1. **Investigate further before deciding delete-vs-fix.** Check: does `nix-build --timeout N` (NOT `--option`) go through a different wire path? Grep Nix source for how `--timeout` (builtin flag) vs `--option build-timeout` (generic override) differ in serialization. The [`nix-wire-format.md`](../../.claude/notes/nix-wire-format.md) memory says ssh-ng options are silently non-functional — cross-check.
2. MODIFY [`handler/mod.rs:82-90`](../../rio-gateway/src/handler/mod.rs) — the "empirically" claim is FALSE. Replace with what T1 actually observed, citing the probe.
3. Decide:
   - **Fix path (preferred if a wire path exists):** find the ACTUAL wire path for `build-timeout` from ssh-ng and wire it. If no such path exists in the Nix protocol, this can't be done without a Nix patch.
   - **gRPC-only path:** `build_timeout` works via direct gRPC `SubmitBuildRequest` (e.g., rio-cli or a future API consumer). Document this clearly at `r[sched.timeout.per-build]` — "not reachable via ssh-ng; use gRPC or rio-cli". The VM test in P0311 T11 should submit via gRPC, not ssh-ng.
   - **Delete path (only if gRPC-path is also unused):** if NOTHING sets `build_timeout > 0` in practice, P0214 is truly dead. `TODO(P<future>)` on the block, but don't delete yet — it's correct dead code, not incorrect live code.

**T1 answer: ambiguous (log fired but overrides didn't contain build-timeout):**

`wopSetOptions` is sent but `--option build-timeout` specifically doesn't propagate. Narrowest case. Check: is `build-timeout` filtered somewhere between client and wire? Grep Nix source for option-name validation/filtering in the ssh-ng path.

### T3 — `docs:` fix P0214 plan doc — `--option timeout` → `--option build-timeout`

Regardless of T2 outcome, [plan-0214](plan-0214-per-build-timeout.md) T3 at [`:61`](plan-0214-per-build-timeout.md) says `--option timeout` but the gateway code at [`handler/mod.rs:77`](../../rio-gateway/src/handler/mod.rs) keys on `"build-timeout"`. The plan doc is wrong (or the code is — but `build-timeout` matches Nix's option name convention). One-line fix to the archaeology.

## Exit criteria

- **T1 probe has a clear answer** — one of three states, captured in the impl report: (a) fired with `build-timeout` in overrides, (b) did not fire, (c) fired but `build-timeout` absent
- If (a): `grep 'Verified under scheduling.nix VM' rio-gateway/src/handler/mod.rs` → 1 hit (T2 path-A annotation)
- If (b) or (c): `grep -v 'Empirically' rio-gateway/src/handler/mod.rs | grep ':87:'` — the line-87 "Empirically" claim is gone or rewritten (T2 path-B correction)
- `grep -- '--option timeout' .claude/work/plan-0214-per-build-timeout.md` → 0 hits (T3: name fixed to `--option build-timeout`)
- [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T11's blocker note is resolved one way or another — either "proceed via ssh-ng" or "proceed via gRPC" or "OBE: feature confirmed dead"
- `/nbr .#ci` green (T2 path-A keeps the probe subtest; path-B/C may involve comment-only changes in which case `.#ci` is clause-4(a) fast-pathable)

## Tracey

References existing markers:
- `r[gw.opcode.set-options.propagation]` — T1 probes this; T2 may update the spec text at [`gateway.md:61`](../../docs/src/components/gateway.md) if the propagation story is narrower than spec'd
- `r[sched.timeout.per-build]` — T2 may add a reachability caveat to [`scheduler.md:445`](../../docs/src/components/scheduler.md) if the answer is "gRPC-only"

No new markers. The contradiction is about whether an EXISTING spec claim is true, not a new behavior. If T2 adds a reachability caveat, that's a `tracey bump` on `r[sched.timeout.per-build]` (spec text changed meaningfully) — the existing `r[impl sched.timeout.per-build]` at [`worker.rs:570`](../../rio-scheduler/src/actor/worker.rs) becomes stale until someone re-verifies against the new text.

## Files

```json files
[
  {"path": "nix/tests/scenarios/scheduling.nix", "action": "MODIFY", "note": "T1: PROBE subtest — --option build-timeout + journalctl grep for wopSetOptions info-log. T2 path-A keeps it (renamed, with asserts); path-B/C reverts."},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "T2: :82-90 comment — either confirm (path-A: add VM-verified note) or correct (path-B: rewrite 'Empirically' claim to match probe result)"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "T2 path-A only: note at r[gw.opcode.set-options.propagation] clarifying ssh-ng override path"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "T2 path-B only: reachability caveat at r[sched.timeout.per-build] (gRPC-only, not ssh-ng) — IF that's the answer"},
  {"path": ".claude/work/plan-0214-per-build-timeout.md", "action": "MODIFY", "note": "T3: fix --option timeout → --option build-timeout at :61 :83"}
]
```

```
nix/tests/scenarios/scheduling.nix   # T1: PROBE subtest
rio-gateway/src/handler/mod.rs       # T2: confirm or correct :82-90 comment
docs/src/components/
├── gateway.md                       # T2-path-A: propagation note
└── scheduler.md                     # T2-path-B: reachability caveat
.claude/work/plan-0214-*.md          # T3: option name fix
```

**Conditional paths in the files fence are intentional** — this is an investigation, not a predetermined fix. The implementer does T1 → reads the probe → picks T2's path. The fence lists everything that MIGHT be touched; the impl touches a subset.

## Dependencies

```json deps
{"deps": [214, 215], "soft_deps": [311], "note": "Hard-dep P0214 (DONE): the feature under investigation. Hard-dep P0215 (DONE): the info-log at opcodes_read.rs:226 that the probe greps for. Soft-dep P0311 T11: this plan UNBLOCKS that task (either 'proceed' or 'OBE'). P0311 T11 should not dispatch until this resolves — writing a VM test for a possibly-dead feature is wasted work. discovered_from=214 (reviewer flagged the T3 skip; coordinator split the reachability question out). Priority: higher than P0311 (blocker), lower than correctness bugs in live code paths (this is investigating whether a path is live)."}
```

**Depends on:** [P0214](plan-0214-per-build-timeout.md) + [P0215](plan-0215-max-silent-time.md) — both DONE. Their code is the subject.

**Blocks:** [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md) T11 — don't write the VM test until we know the feature is reachable AND by which path (ssh-ng or gRPC).

**Conflicts with:** [`scheduling.nix`](../../nix/tests/scenarios/scheduling.nix) — P0214 T3 and P0215's T3 both TAIL-appended subtests here (neither landed — both skipped). Low traffic. [`handler/mod.rs`](../../rio-gateway/src/handler/mod.rs) — comment-only edit, low conflict. [`scheduler.md`](../../docs/src/components/scheduler.md) count=20 — [P0304](plan-0304-trivial-batch-p0222-harness.md) T25 also edits `r[sched.timeout.per-build]` (fixing the `Failed { status: TimedOut }` drift at `:451`); T2-path-B here edits the same marker's text (reachability caveat). **If both land, T2-path-B should rebase on P0304-T25 — same marker, different sentences.** Sequence P0304 first if both dispatch same-sprint.
