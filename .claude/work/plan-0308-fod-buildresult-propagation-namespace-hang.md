# Plan 0308: FOD BuildResult propagation — nix-daemon-in-namespace hang

**Investigation plan.** [P0243](plan-0243-vm-fod-proxy-scenario.md)'s denied-domain assertion at [`fod-proxy.nix:285`](../../nix/tests/scenarios/fod-proxy.nix) (p243 worktree) hits a `timeout 90` shell-level bound instead of receiving a `BuildResult`. The comment block there:

> "the build DOES dispatch, wget DOES get 403 ("wget: server returned error: HTTP/1.1 403 Forbidden" — stderr streams correctly to the client). nix-build waits forever. `timeout 90` is the bound."

**What's known to work:** The **success** path works end-to-end (allowed domain → wget succeeds → $out written → hash verified → `BuildResult{Built}` propagates → nix-build exits 0). Stderr streams correctly (STDERR_NEXT frames arrive). The reviewer traced gateway/scheduler/worker paths and found no obvious drop.

**Hypothesis (from the P0243 reviewer):** `spawn_daemon_in_namespace` runs the local nix-daemon inside a mount namespace where `$out` is an overlayfs upper-dir path. When the FOD builder exits non-zero (wget 403 → builder script exits 1), `nix-daemon` tries to read `$out` to compute the FOD hash for the `BuildResult` error message — but `$out` was never created. Inside the namespace, the ENOENT on `$out` may cause the daemon to spin/retry rather than propagate. The `BuildResult` terminal frame never gets written to the worker's stdio pipe.

**Why success works but failure doesn't:** Success → `$out` exists → daemon reads it → hash check → `BuildResult` sent. Failure → `$out` missing → daemon error path taken, which may have a different codepath that blocks.

This is an **investigation** plan: T1 reproduces and pinpoints; T2 is the fix once T1 finds it. If T1 concludes the hypothesis is wrong, write the real root cause into this doc and adjust T2.

## Entry criteria

- [P0243](plan-0243-vm-fod-proxy-scenario.md) merged (fod-proxy.nix scenario exists, `TODO(P0243-followup)` comment at `:285` is the anchor)

## Tasks

### T1 — `test(worker):` isolate reproduction — daemon-only, no k3s

NEW integration test in `rio-worker/tests/` (or reuse an existing daemon-spawn test harness — check for `spawn_daemon` in test code):

```rust
// r[verify worker.fod.verify-hash]
// Isolates the FOD-failure BuildResult-hang from the full VM fixture.
// If this test reproduces the hang, the bug is worker↔daemon, not
// gateway/scheduler. If it doesn't hang, the bug is upstream.
#[tokio::test]
async fn fod_builder_failure_returns_buildresult_timely() {
    // 1. Spawn local nix-daemon via spawn_daemon_in_namespace (or the
    //    direct daemon-spawn primitive — find it).
    // 2. Submit a FOD with a builder that exits 1 WITHOUT writing $out:
    //      outputHash = "sha256-AAAA...";  // valid but unmet
    //      builder = "${busybox}/bin/sh";
    //      args = ["-c", "echo fail >&2; exit 1"];
    // 3. tokio::time::timeout(Duration::from_secs(30), read_build_result())
    //    — if timeout fires, the bug reproduces at worker level.
    // 4. Assert BuildResult arrives with status indicating builder failure,
    //    error_msg containing "exit 1" or "hash mismatch" (either is fine;
    //    the point is it ARRIVES).
}
```

If the timeout fires: **bug confirmed at worker↔daemon boundary.** Proceed to T1b (strace).
If BuildResult arrives: **bug is NOT in the worker.** Escalate investigation to gateway stream-forward path (the `wopBuildDerivation` result-frame relay).

### T1b — `test(worker):` strace the daemon (if T1 reproduces)

Wrap the daemon spawn in `strace -f -e trace=open,openat,read,write -o /tmp/daemon-strace.log`. Look for:
- A read/stat loop on a `$out`-like path (`/build/...` or the overlay upper-dir)
- The last `write()` to the worker's stdio pipe (the stderr 403 line) and what syscall follows
- Any `futex` or `poll` with infinite timeout

The strace output tells you which syscall the daemon is stuck on. Write the finding into this doc.

### T2 — `fix(worker):` the actual fix (scope depends on T1)

**If daemon spins on missing $out:** The fix is likely in `spawn_daemon_in_namespace` — ensure the overlay upper-dir that serves as `$out`'s parent always exists, even when the builder didn't create anything. Or: the daemon may need a different `--store` argument inside the namespace so its error-path `$out` stat resolves.

**If the daemon exits but the worker doesn't notice:** Check the worker's stdio-read loop — it may be blocking on `read(stderr_fd)` after the daemon's process group died without closing the pipe (leaked fd in a grandchild?). Fix: `tokio::select!` on child-exit as well as stderr-read.

**If the BuildResult IS written but the worker drops it:** The worker's `STDERR_LAST` / result-frame parsing may be missing a case for FOD-failure BuildResult shape. Compare against the success-path BuildResult parsing.

Reserve `// r[impl worker.fod.verify-hash]` for wherever the fix lands — the spec at [`worker.md:297`](../../docs/src/components/worker.md) covers FOD hash verification, and the failure-propagation path is part of that.

### T3 — `test(nix):` unblock fod-proxy.nix denied-case assertion

MODIFY [`nix/tests/scenarios/fod-proxy.nix`](../../nix/tests/scenarios/fod-proxy.nix) at `:285` (post-P0243-merge line number — re-grep at dispatch):

Remove the `TODO(P0243-followup)` comment block. Change the `timeout 90` shell-wrap to a real assertion:
```python
# Before: timeout 90 nix-build ... || true  (shell-level bound)
# After:  nix-build ... ; assert build exits non-zero within reasonable
#         time (60s generous); assert stderr contains "403 Forbidden"
#         AND BuildResult error surfaces (check exit code and message).
```

## Exit criteria

- `/nbr .#ci` green — including the `vm-fod-proxy` scenario which currently hits `timeout 90` on the denied case
- T1's isolation test: `cargo nextest run -p rio-worker fod_builder_failure_returns_buildresult_timely` — passes in <30s, asserts BuildResult arrival
- `nix develop -c tracey query rule worker.fod.verify-hash` shows impl + verify (T2's annotation + T1's test)
- `grep 'TODO(P0243-followup)' nix/tests/scenarios/fod-proxy.nix` → 0 hits (T3 closes it)
- `grep 'timeout 90' nix/tests/scenarios/fod-proxy.nix` → 0 hits in the denied-case block (replaced with real assertion)

## Tracey

References existing markers:
- `r[worker.fod.verify-hash]` — T2 implements (failure-path half), T1 verifies. The marker at [`worker.md:297`](../../docs/src/components/worker.md) says "after build completion, the worker verifies `outputHash` matches" — this plan covers what happens when the build DOESN'T complete and `$out` is missing.

## Files

```json files
[
  {"path": "rio-worker/tests/fod_failure.rs", "action": "NEW", "note": "T1: isolated reproduction — daemon-only, no k3s. May live in an existing test file; check at dispatch."},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "MODIFY", "note": "T2: the fix (file path HYPOTHETICAL — depends on T1 finding; likely daemon spawn or stdio-read loop)"},
  {"path": "nix/tests/scenarios/fod-proxy.nix", "action": "MODIFY", "note": "T3: remove TODO(P0243-followup) at :285, replace timeout-90 with real assertion"}
]
```

```
rio-worker/
├── tests/fod_failure.rs          # T1: isolated repro (NEW, name tentative)
└── src/executor/daemon.rs        # T2: fix (path depends on T1 finding)
nix/tests/scenarios/fod-proxy.nix # T3: real denied-case assertion
```

## Dependencies

```json deps
{"deps": [243], "soft_deps": [], "note": "Investigation plan. P0243 proves the bug exists (fod-proxy.nix:285 TODO). Hypothesis: spawn_daemon_in_namespace $out-missing ENOENT-spin. T1 isolates; T2 scope depends on T1 finding. discovered_from=P0243. NOTE P0243 gated in merge-queue (codecov conflict with P0294) — verify P0243 has landed before dispatch."}
```

**Depends on:** [P0243](plan-0243-vm-fod-proxy-scenario.md) — `fod-proxy.nix` scenario + the `TODO(P0243-followup)` anchor. **P0243 is in the merge queue but gated** (codecov conflict with P0294) — verify it has merged before dispatch.
**Conflicts with:** `rio-worker/src/executor/` is hot territory but the specific file depends on T1. `fod-proxy.nix` is NEW in P0243 — no other plan touches it.
