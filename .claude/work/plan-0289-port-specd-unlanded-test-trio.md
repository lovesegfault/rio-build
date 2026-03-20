# Plan 0289: Port 3 spec'd-but-unlanded verify fragments (tracey-untested cleanup)

**Retro P0188+P0194+P0200 finding.** Three `r[impl]`-tagged rules have NO `r[verify]` — all three have **fully-written test fragments** sitting in remediation docs, never transplanted:

| Rule | `r[impl]` at | Spec'd fragment at | Target |
|---|---|---|---|
| `worker.cgroup.kill-on-teardown` | [`rio-worker/src/executor/mod.rs:639`](../../rio-worker/src/executor/mod.rs) | [`09-build-timeout-cgroup-orphan.md:605-700`](../../docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md) | `nix/tests/scenarios/lifecycle.nix` |
| `worker.shutdown.sigint` | [`rio-worker/src/main.rs:498-500`](../../rio-worker/src/main.rs) | [`15-shutdown-signal-cluster.md:382-470`](../../docs/src/remediations/phase4a/15-shutdown-signal-cluster.md) | `nix/tests/scenarios/lifecycle.nix` |
| `worker.fuse.passthrough` | [`rio-worker/src/fuse/mod.rs:8`](../../rio-worker/src/fuse/mod.rs) | [`21-p2-p3-rollup.md:66`](../../docs/src/remediations/phase4a/21-p2-p3-rollup.md) | `rio-worker/src/fuse/mod.rs` (`#[ignore]` unit stub) |

`tracey query untested` flags all three. `worker.fuse.passthrough` is the ONLY untested rule in the entire project (1 of 187).

**The sigint fragment also guards `.#coverage-full`:** per CLAUDE.md §Coverage step 3, worker `main()` returning normally on SIGINT flushes profraw via atexit. A `main.rs` refactor that breaks the cancellation arm would **silently zero out worker VM coverage.**

**Dep on P0294 (Build CRD rip):** the 09-doc fragment at `:628-645` uses `kubectl apply -f Build` + `spec.timeoutSeconds`. After P0294 there is no Build CR. The existing [`lifecycle.nix:878`](../../nix/tests/scenarios/lifecycle.nix) `cancel-cgroup-kill` fragment faces the SAME retarget (delete Build CR → finalizer → CancelBuild RPC becomes direct gRPC `CancelBuild`). P0294 does the cancel-cgroup-kill retarget; this plan inherits that pattern for `build-timeout`. Hence deps=[294].

**Source-doc conflict resolved:** the 15-doc at `:370` says `sigint-graceful` slots into `scheduling.nix`. The retrospective says `lifecycle.nix`. We go with `lifecycle.nix` — it batches with `build-timeout` (both are `vm-lifecycle-*` composable), and `lifecycle.nix` already has the k3s fixture the cgroup-kill assertion needs. The 15-doc's rationale ("scheduling.nix runs rio-worker as systemd on real VMs") is still satisfiable — `lifecycle.nix` has the same standalone-fixture capability via the fragments attrset.

> **[IMPL NOTE — resolved backwards]:** fragment landed in `scheduling.nix`
> (at `:1095`), not `lifecycle.nix`. The 15-doc's rationale ("scheduling.nix
> runs rio-worker as systemd on real VMs — k3s pods are distroless, no
> shell, no systemctl") was correct and load-bearing. lifecycle.nix's k3s
> fixture can't deliver SIGINT to a worker PID. See `scheduling.nix:1124-1126`
> for the impl's explanation.

## Entry criteria

- [P0294](plan-0294-build-crd-full-rip.md) merged (cancel-cgroup-kill retarget pattern feeds in; Build CR no longer exists so `build-timeout` must use gRPC SubmitBuild+timeout directly)

## Tasks

### T1 — `test:` port `build-timeout` fragment to lifecycle.nix (09-doc, RETARGETED)

NEW fragment in [`nix/tests/scenarios/lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix). The 09-doc:605-700 spec **cannot be ported verbatim** — it uses `kubectl apply Build.spec.timeoutSeconds: 5`. After P0294, the retarget uses gRPC `SubmitBuildRequest.timeout_seconds` directly (same field, same scheduler-side enforcement, just a different entry point).

Add `# r[verify worker.cgroup.kill-on-teardown]` + `# r[verify worker.timeout.no-reassign]` to the col-0 file-header comment block (tracey `.nix` parser constraint — markers BEFORE the outer `{`, not inside the testScript string).

```python
build-timeout = ''
  # ══════════════════════════════════════════════════════════════════
  # build-timeout — gRPC timeout_seconds < build duration → TimedOut
  # ══════════════════════════════════════════════════════════════════
  # Post-P0294: no Build CR. Submit via gRPC SubmitBuild with
  # timeout_seconds=5 directly (same scheduler enforcement path).
  #
  # sleepSecs=30 vs timeout=5: wide gap for TCG dispatch lag.
  # Under TCG the timeout may fire at ~8-12s wall-clock; sleep is
  # nowhere near done. Narrower gaps flake.
  with subtest("build-timeout: gRPC timeout_seconds < sleep → TimedOut, cgroup cleaned"):
      drv_path = client.succeed(
          "nix-instantiate "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${timeoutDrv} 2>/dev/null"  # mkTrivial sleepSecs=30
      ).strip()
      client.succeed(f"nix copy --derivation --to 'ssh-ng://k3s-server' {drv_path}")

      # Submit via grpcurl: SubmitBuild with timeout_seconds=5.
      # Same client helper as P0294's cancel-cgroup-kill retarget.
      submit_resp = client.succeed(
          f"grpcurl -plaintext -d '{{"
          f'"dag":{{"nodes":[{{"drv_path":"{drv_path}"}}],"edges":[]}},'
          f'"timeout_seconds":5}}'
          f"' k3s-server:9001 rio.scheduler.SchedulerService/SubmitBuild"
      )
      build_id = json.loads(submit_resp.split('\n')[0])['buildId']

      # ── Assertion 1: cgroup exists (build actually started). ──
      # sanitize_build_id: "...lifecycle-timeout.drv" → "*lifecycle-timeout_drv"
      worker_vm = k3s_agent  # or resolve via pod nodeName
      cgroup_path = worker_vm.wait_until_succeeds(
          "find /sys/fs/cgroup -type d -name '*lifecycle-timeout_drv' "
          "-print -quit 2>/dev/null",
          timeout=120,
      ).strip()
      assert cgroup_path, "cgroup never appeared"

      # ── Assertion 2: cgroup GONE after timeout (kill-on-teardown fired). ──
      worker_vm.wait_until_fails(f"test -d {cgroup_path}", timeout=60)

      # ── Assertion 3: scheduler shows TimedOut, NOT reassigned. ──
      # Scrape rio_worker_builds_total{outcome="timed_out"} via apiserver proxy.
      metrics = k3s_server.succeed(
          "k3s kubectl -n ${ns} get --raw "
          "/api/v1/namespaces/${ns}/pods/default-workers-0:9091/proxy/metrics"
      )
      timed_out = [l for l in metrics.splitlines()
                   if 'rio_worker_builds_total' in l and 'outcome="timed_out"' in l]
      assert timed_out and float(timed_out[0].split()[-1]) >= 1, \
          f"no timed_out increment: {timed_out}"

      # ── Assertion 4: same drv, second build succeeds (no EEXIST). ──
      # Proves cgroup leak is closed, not just "rmdir warned."
      client.succeed(
          f"grpcurl -plaintext -d '{{"
          f'"dag":{{"nodes":[{{"drv_path":"{drv_path}"}}],"edges":[]}},'
          f'"timeout_seconds":60}}'  # full budget this time
          f"' k3s-server:9001 rio.scheduler.SchedulerService/SubmitBuild"
      )
      # Wait for completion (30s sleep + dispatch overhead).
      worker_vm.wait_until_succeeds(
          "find /sys/fs/cgroup -type d -name '*lifecycle-timeout_drv' "
          "-print -quit 2>/dev/null | xargs -r test ! -d",  # created then cleaned
          timeout=120,
      )
'';
```

**NOTE on `cancel-cgroup-kill` distinction:** the existing fragment at [`lifecycle.nix:878`](../../nix/tests/scenarios/lifecycle.nix) verifies `r[worker.cancel.cgroup-kill]` — the CANCEL RPC path (`runtime.rs` handler). **Different call site** from timeout-teardown at `executor/mod.rs:640`. This fragment covers the timeout path.

### T2 — `test:` port `sigint-graceful` fragment to lifecycle.nix (15-doc)

NEW fragment from [`15-shutdown-signal-cluster.md:382-470`](../../docs/src/remediations/phase4a/15-shutdown-signal-cluster.md). Uses `systemctl kill -s INT` on a real-VM worker (not k3s pod — distroless has no shell). Slots into a standalone fixture alongside `reassign`.

Add `# r[verify worker.shutdown.sigint]` to the col-0 file-header comment block.

```python
sigint-graceful = ''
  # ══════════════════════════════════════════════════════════════════
  # sigint-graceful — Ctrl+C path: FUSE unmounts, profraw flushes
  # ══════════════════════════════════════════════════════════════════
  # Before rem-15: main.rs:386 watched SIGTERM only. SIGINT hit
  # default handler → immediate term → no Drop, no atexit. FUSE
  # leaked (next start EBUSY), profraw lost (dev Ctrl+C = 0 cov).
  #
  # Two-layered assertion:
  #   1. mountpoint -q FAILS after unit stops → Mount::drop ran
  #      → fusermount -u ran → main() returned (stack unwound).
  #      PRIMARY — proves SIGINT caught, token cancelled, select!
  #      arm broke, main() returned.
  #   2. coverage mode: profraw count increased (atexit fired).
  #      SECONDARY — guards .#coverage-full. A main.rs refactor
  #      that breaks the cancellation arm would silently zero out
  #      worker VM coverage.
  with subtest("sigint-graceful: SIGINT → FUSE unmounts + profraw flushes"):
      # wsmall2 is the standalone-fixture worker VM (per 15-doc:380).
      mount_before = wsmall2.succeed("mountpoint -q /nix/store && echo mounted").strip()
      assert mount_before == "mounted", "FUSE not mounted pre-SIGINT"

      # Coverage mode: count profraws BEFORE.
      prof_before = int(wsmall2.succeed(
          "ls /var/lib/rio/cov/*.profraw 2>/dev/null | wc -l"
      ).strip() or "0")

      # Send SIGINT. systemd delivers to main PID.
      wsmall2.succeed("systemctl kill -s INT rio-worker")

      # Wait for unit to stop (Restart=always will bring it back).
      wsmall2.wait_until_succeeds(
          "systemctl show rio-worker -p ActiveState | grep -q inactive",
          timeout=30,
      )

      # PRIMARY: FUSE unmounted.
      wsmall2.fail("mountpoint -q /nix/store")

      # SECONDARY: profraw increased (only in coverage mode).
      prof_after = int(wsmall2.succeed(
          "ls /var/lib/rio/cov/*.profraw 2>/dev/null | wc -l"
      ).strip() or "0")
      if prof_before > 0 or "${if coverage then "1" else ""}":
          assert prof_after > prof_before, \
              f"profraw didn't flush: before={prof_before} after={prof_after}"

      # Let systemd restart it for subsequent fragments.
      wsmall2.wait_for_unit("rio-worker.service")
'';
```

### T3 — `test(worker):` `#[ignore]` passthrough stub (21-doc:66)

NEW test in [`rio-worker/src/fuse/mod.rs`](../../rio-worker/src/fuse/mod.rs) tests module — the ~25 LoC stub from [`21-p2-p3-rollup.md:66`](../../docs/src/remediations/phase4a/21-p2-p3-rollup.md):

```rust
// r[verify worker.fuse.passthrough]
/// Passthrough mode: kernel handles reads directly, no userspace copy.
/// The perf-critical path. Full verification needs CAP_SYS_ADMIN +
/// Linux 6.9+ (FUSE_PASSTHROUGH); this stub lands so tracey stops
/// flagging the rule. Full verify deferred to VM test.
#[test]
#[ignore = "passthrough requires CAP_SYS_ADMIN + Linux 6.9+; full verify in VM test"]
fn passthrough_mode_no_failures() {
    // Mount with passthrough=true, open a file, assert
    // passthrough_failures atomic stays 0.
    let tmp = tempfile::tempdir().unwrap();
    let cache = /* ... minimal Cache */;
    let fs = RioFs::new(cache, /* passthrough= */ true, /* ... */);
    // Minimal open cycle.
    // ...
    assert_eq!(fs.passthrough_failures.load(Ordering::Relaxed), 0);
}
```

`#[ignore]` means `cargo nextest run` skips it by default; `tracey-validate` only checks the annotation exists, not that the test runs. Closes the tracey gap.

## Exit criteria

- `/nbr .#ci` green
- `tracey query untested` → `worker.cgroup.kill-on-teardown`, `worker.shutdown.sigint`, `worker.fuse.passthrough` all GONE
- `grep 'r\[verify worker.cgroup.kill-on-teardown\]' nix/tests/scenarios/lifecycle.nix` → 1 hit (col-0 file header)
- `grep 'r\[verify worker.shutdown.sigint\]' nix/tests/scenarios/lifecycle.nix` → 1 hit
- `grep 'r\[verify worker.fuse.passthrough\]' rio-worker/src/fuse/mod.rs` → 1 hit

## Tracey

References existing markers (all three have `r[impl]`, gain `r[verify]` here):
- `r[worker.cgroup.kill-on-teardown]` — T1 adds verify (was: `r[impl]` at `executor/mod.rs:639`, NO verify)
- `r[worker.shutdown.sigint]` — T2 adds verify (was: `r[impl]` at `main.rs:~498`, NO verify)
- `r[worker.fuse.passthrough]` — T3 adds verify (was: `r[impl]` at `fuse/mod.rs:8`, NO verify)
- `r[worker.timeout.no-reassign]` — T1 also covers this (09-doc:613 paired marker)
- `r[worker.cancel.cgroup-kill]` — T1 is NOT this. Different call site. Existing `cancel-cgroup-kill` fragment at `lifecycle.nix:878` covers the cancel-RPC path; `build-timeout` covers the timeout-teardown path.

## Files

```json files
[
  {"path": "nix/tests/scenarios/lifecycle.nix", "action": "MODIFY", "note": "T1: build-timeout fragment (RETARGETED from 09-doc kubectl→gRPC); T2: sigint-graceful fragment (from 15-doc); col-0 header markers"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "T3: #[ignore] passthrough_mode_no_failures stub test"}
]
```

```
nix/tests/scenarios/lifecycle.nix  # T1+T2: two new fragments + col-0 markers
rio-worker/src/fuse/mod.rs         # T3: #[ignore] stub
```

## Dependencies

```json deps
{"deps": [294], "soft_deps": [], "note": "retro P0188+P0194+P0200 — discovered_from=188,194,200. All 3 fragments fully-spec'd in remediation docs, never ported. Dep 294: 09-doc fragment uses Build CR kubectl apply — needs P0294's gRPC retarget pattern. sigint guards .#coverage-full profraw flush (CLAUDE.md cov step 3). Source-doc conflict: 15-doc says scheduling.nix, retrospective says lifecycle.nix — went lifecycle (batches with build-timeout). tracey .nix constraint: col-0 before outer {."}
```

**Depends on:** [P0294](plan-0294-build-crd-full-rip.md) — the 09-doc fragment's `kubectl apply Build.spec.timeoutSeconds` pattern is dead after the CRD rip; this plan uses the gRPC `SubmitBuildRequest.timeout_seconds` path that P0294 establishes for `cancel-cgroup-kill`'s retarget.
**Conflicts with:** [`lifecycle.nix`](../../nix/tests/scenarios/lifecycle.nix) heavy — also touched by [P0285](plan-0285-drainworker-disruptiontarget-watcher.md) (disruption-drain fragment) and [P0294](plan-0294-build-crd-full-rip.md) (watch-dedup DELETE + cancel-cgroup-kill retarget). All are fragment-attrset appends/edits; P0294 first (it DELETES, others ADD).
