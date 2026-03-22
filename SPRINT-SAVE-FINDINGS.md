# sprint-save Findings — Latent Bugs Surfaced by Reliable KVM

## Summary

Cherry-picking sprint-1 commits onto a baseline with reliable KVM (via `kvmPreopen`) surfaced **15+ latent bugs** that were masked during sprint-1 because KVM was broken (the dual-`accel=` qemu bug, fixed at pos986/`7bd70aba`). Tests that were never validated under real KVM timing, or never validated at all, are now failing.

**Checkpoints:**
- `sprint-save-baseline`: STABLE 3/3 (27 commits)
- `sprint-save-pos186`: STABLE 3/3 (185 commits)
- `sprint-save-pos400`: STABLE 3/3 (411 commits, tag `e9265a08`)
- `sprint-save-pos600`: in progress (~625 commits, ~15 fix iterations)

## Bugs still in sprint-1 HEAD (never validated)

### 1. `lifecycle.nix` build-timeout — wrong metrics port (commit `71b42a9c`)
Test used scheduler port `9091` instead of worker port `9093`. Copy-paste error from the scheduler metrics scrape. Added at `3231206d`, never caught.

### 2. `lifecycle.nix` build-timeout assertion 4 — scheduler doesn't re-dispatch terminal drvs (commit `4d833832`)
After `TimedOut`, resubmitting the same `drvPath` returns a buildId but the scheduler never dispatches (DAG node is terminal). Test assumed re-dispatch works. Added at `3231206d`. **Genuine scheduler feature gap** — needs plan.

## KVM-speed timing races (surfaced by faster execution)

### 3. Bloom fill assertion — `0.0 < fill` (commit `cce09b1e`)
Under KVM the 3-drv chain completes <10s before first 10s heartbeat → `fill=0.0`. **rev-p288 predicted this at mc~153**. Fix was in `7bd70aba` (bundled with kvmOnly-disable which we skip).

### 4. Trace-id poll timeout — `spawn_monitored` holds `#[instrument]` span (commit `fea46124`)
`bridge_build_events → spawn_monitored` captures `Span::current()` and wraps child task with `.instrument(span)`. Span can't close until bridge task ends (= build duration ~62s). Span exported at VM-t=93.6s, past 60s poll. **Architectural smell**: parent-span extended to child-task lifetime.

### 5. `dashboard-gateway` grpc_web xDS convergence race (commits `4a7da8e0`, `8d6c6b0c`, `109f1eec`)
Test reached config_dump check before Envoy Gateway controller reconciled GRPCRoute → xDS → push. Still flaky — added diagnostic dump.

### 6. `sigint-graceful` idle-wait (commit `9261d0e5`)
Prior reassign subtest lands a sleepSecs=25 build on wsmall2; SIGINT-drain waits for it. Added `rio_worker_builds_active` gauge poll before SIGINT.

### 7-9. Observability OTEL-flush timing, leader-election metrics readiness, lifecycle-recovery subtest timing (pre-pos400)

## `pipefail` bugs (NixOS test-driver `set -eo pipefail`)

**Pattern:** Commands that encode result-in-exit-code (`ls` glob-no-match=2, `find` dir-missing=1, `systemctl is-active` inactive=3) break `cmd | filter` pipelines. **3 hits in sigint-graceful alone.**

### 10. `ls *.profraw | wc -l || echo 0` → output `"0\n0"` (commit `b7adaf1a`)
### 11. `find /dir | wc -l` → find exits 1 when dir doesn't exist (commit `ba0292aa`)
### 12. `systemctl is-active | grep inactive` → is-active exits 3 when inactive (commit `df66ab05`)

Replacement catalog in memory file `gotcha_nixos-test-pipefail.md`.

## crate2nix raw-libtest (not KVM-related)

### 13. DB name collision — nanos not unique under thread-per-test (commit `42eeea74`)
`SystemTime::now()...as_nanos()` alone: two parallel threads hit same nanosecond. Under crane's nextest (process-per-test) this was fine. Added `AtomicU64` counter.

## Inline YAML config mismatches (pre-pos400)

### 14. ephemeral-pool image tag — `rio-all` vs `rio-all:dev`
### 15. ephemeral-pool `tlsSecretName` missing

Inline WorkerPool/CRD YAML in tests must replicate Helm conditionals.

## Secondary-hunk hazard

`7bd70aba` was in SKIP-list (primary: kvmOnly-disable, N/A with kvmPreopen) but bundled the bloom-fill fix as SECONDARY hunk. **Per-commit skip decisions miss secondary hunks** — check `git show --stat` before skipping mixed commits.

## 3rd bug still in sprint-1 HEAD

### GetBuildLogs proto field encoding (commit `28b29151`)
Test added at pos518 with `drv_path` at field 1 (`0x0a`). Proto refactor at pos592 (`b643ab82`) moved `drv_path`→`derivation_path` at field 2; field 1 became `build_id`. Test's raw-bytes now send `build_id="nonexist"` (invalid UUID). Handler hangs or rejects without trailer. **Test-added-before-proto-refactor pattern** — raw-bytes encoding in tests is fragile across proto changes.

## Bugs 16-23 (pos600 v16-v23)

### disruption-drain — `kubectl logs` http2:stream-closed under poll load (v20-v23)
`kubectl logs` fails with `http2: stream closed` when called in a `wait_until_succeeds` poll loop. Every iteration opens a new stream that immediately closes. **Fix:** read `/var/log/pods/${ns}_${pod}-*/container/*.log` directly. Check both k3s nodes (replicas=2, podAntiAffinity).

### GetBuildLogs streaming — test design was WRONG (v20 diagnostic)
Diagnostic dump revealed: HTTP 200 with **EMPTY body**. `tonic::Status::not_found()` for a server-streaming RPC puts the gRPC status in **HTTP trailers**, not body frames. The `0x80` trailer-frame byte never appears in curl's body output. Test was checking the wrong place. **Still in sprint-1 HEAD.** Added at pos518, never passed.

### disruption-drain — added at pos493, never validated (v16)
30s timeout insufficient; 60s still fails due to kubectl http2 issue.

## Metrics

- pos400→pos600: 200 sprint-1 commits + **26 fix commits**
- Total since baseline: 637 commits
- pos600 validation: **23 iterations** to stabilize one batch
- Bugs still in sprint-1 HEAD: **4** (wrong-port, no-resubmit, proto-field, GetBuildLogs-design)

## Key Patterns Discovered

1. **pipefail** — commands that exit-non-zero by design (`ls` glob, `find` dir-missing, `systemctl is-active`) break `cmd | filter` pipelines. 3 hits in sigint-graceful.
2. **kubectl logs http2** — fails under poll-loop load. Read `/var/log/pods/` directly.
3. **Secondary-hunk skip hazard** — skipping a commit by its PRIMARY purpose misses SECONDARY hunks (7bd70aba bloom fix).
4. **Proto-refactor + raw-bytes tests** — raw protobuf encoding in tests is fragile across proto field reordering.
5. **Demote after N iterations** — if sibling/downstream check proves same property, demote flaky check to best-effort diagnostic.
