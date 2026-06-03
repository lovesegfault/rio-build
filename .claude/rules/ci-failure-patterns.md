# CI failure patterns

Reference catalog of CI-gate failure signatures that have bitten this project at least once. Check here when `nix-fast-build .#checks` is red and the cause isn't obvious from the log tail.

## Deterministic failures

| Pattern | Symptom | Fix |
|---|---|---|
| **Nightly-only syntax** | compiles in `nix develop`, fails in `checks.clippy-*` | `if let` chain guards, `let_chains`, etc. — devshell is nightly, CI is stable. Rewrite or use `nix develop .#stable`. |
| **Stale fuzz Cargo.lock** | fuzz build fails, `fuzz/rio-nix/Cargo.lock` or `fuzz/rio-store/Cargo.lock` out of sync | Per-crate fuzz workspaces have independent lockfiles. `cd fuzz/<crate> && cargo update -p <crate>`. |
| **codecov after_n_builds drift** | `codecov-matrix-sync` check fails: `after_n_builds=N but coverage matrix has M entries` | Added/removed a VM coverage target without bumping `.github/codecov.yml`. Update `after_n_builds` to match. |
| **tracey broken ref** | `tracey-validate` fails: `r[impl X]` has no matching spec marker | Code has `// r[impl foo.bar]` but `docs/spec/**/*.typ` lacks `#r("foo.bar")`. Either add the spec marker (`#r("...")[body]` call) or fix the typo. |
| **pyflakes f-string** | VM test fails at lint, not runtime: `F541 f-string without placeholders` | nixos-test-driver runs pyflakes on `testScript`. `f"foo"` with no `{...}` is a pyflakes error. Drop the `f` prefix. |
| **IFD × non-determinism** | VM test cert mismatch: `x509: certificate signed by unknown authority (crypto/rsa: verification error)` — but the CA CN matches | `builtins.readFile(runCommand ... ${nondeterministic})` pulls eval-time build contents into a string; remote builder rebuilds with DIFFERENT contents. Convert to a `runCommand` that takes the derivation as a regular build input. |
| **Coverage profraw timeout** | `.#coverage` or `coverage.vm-*` hits `globalTimeout` | Coverage-mode k3s tests have builder-disk I/O variance. Bump `globalTimeout` with headroom. Check if tmpfs is wired for the containerd store. |
| **Unregistered metric** | test passes but metric is always zero in production | Metric is `emit!()`ed but never `.register()`ed in the component's `lib.rs`/`main.rs`. Grep for the metric name — registration and emission are two separate call sites. |
| **helm template fails** | `helm-lint` check fails | `infra/helm/rio-build/` chart doesn't render with one of the `values/*.yaml` files. `helm template rio . -f values/dev.yaml` locally to see the error. |
| **Rustdoc intra-doc lint** | `cargo doc` fails on `[nonexistent]` | broken `[Type]` links in doc comments — use `` [`Type`](path) `` or escape as `\[...\]`. |
| **rustfmt drift** | `treefmt` check fails | nightly rustfmt vs stable rustfmt format differently; run `nix develop .#stable -c cargo fmt`. |
| **Nix `''` in Python comment** | VM test: syntax error at a comment line | `''` in a Python comment inside a Nix `''...''` string is a string terminator. Reword the comment. |
| **statix style** | statix lint → shows under the `pre-commit` check, not standalone | `inherit (pkgs) lib` not `lib = pkgs.lib`. Mechanical fix. |
| **stdenv pipefail SIGPIPE** | `runCommand` build fails exit-1 with no stderr; buildCommand has a `producer | head -c N` shape | stdenv sets `set -o pipefail`; head closes pipe → producer SIGPIPE → exit≠0. Reverse to `head -c N input | consumer`, or prefix `set +o pipefail;`. |
| **Cilium config no-restart** | `helm upgrade cilium --reuse-values --set X` succeeds but `cilium-dbg config` still shows old value | Chart doesn't checksum-annotate `cilium-config` ConfigMap → DS pods don't restart. `kubectl -n kube-system rollout restart ds/cilium` after. |
| **Torn worktree eval under concurrent agents** | Local gate red on a build that can't fail from the diff (e.g. docs-pdf "file not found …/rio-docs-root/…" for a tracked, present file); same HEAD builds the target green afterwards | Flake eval snapshotted the worktree mid-commit (or while sibling worktrees churned the shared .git). Diagnose: failing drv hash ≠ clean-HEAD drv hash for the same target. Fix: re-run the gate from clean HEAD; don't debug the failing drv's contents. |

## Flaky tests

| Pattern | Signature | Fix strategies |
|---|---|---|
| **k3s airgap import timing** | VM test flakes on agent readiness — airgap imports serially/alphabetically before kubelet | Gate on server-node-exists (validated 3/3 — agent-Ready 106.70→1.9s, 56×). Budget for tail, not typical (builder variance 5×). |
| **flannel subnet race** | `loadFlannelSubnetEnv failed: open /run/flannel/subnet.env` early in boot | Gated since 7679316a (`k3s-full.nix:720`). The log line is now a benign blip that recovers in <1s; if a subtest still times out, look past the flannel error for the real cause. |
| **job-tracking finalizer orphan** | `ephemeral-pool` pod-phase wait at 180s; pod cleaned on node but `phase=Running` in apiserver | Background-delete on a Job races Job-Complete → pod's `batch.kubernetes.io/job-tracking` finalizer orphaned. Both `reap_excess_pending` and `reap_orphan_running` now use foreground propagation; no `DeleteParams::background()` callsites remain on Jobs. If seen again, audit any new Job-delete callsite. |
| **Machine.succeed() thread-unsafe** | `rc int-on-empty` when bg+main threads both call succeed | Use `--wait=false` instead of threading. |
| **kubectl logs poll churn** | `http2: stream closed` errors under TCG — `kubectl logs\|grep` in wait_until_succeeds triggers kubelet churn | Don't poll logs for readiness — use cgroup/kernel/metric state instead. |
| **Wall-clock gate under load** | `assert!(elapsed < Ns)` flakes under builder CPU contention | **(a)** retry-N-times; **(b)** widen gate with documented slack budget; **(c)** convert to structural assertion — count ops, not wall-clock. Prefer (c). The four recorded recurrences are converted as of round-14: vm-sla-sizing-kwok setup-tier waits (3 sites — bootstrap pool-status / Registered / wait_worker_pod; budget→tail; structural condition unchanged), vm-fetcher-split fod-fail (`elapsed<60` → `rc!=124`; the property is termination, not latency), hw_bench alu rel band (0.20→0.35; band IS the property, documented-slack), vm-lifecycle-recovery post-recovery dispatch (120→240; convergence wait). Surviving `elapsed<N` members enumerated by census in the round-14 commit body — triage a new flake against that list before treating as novel. |
| **Parallel test order-dependence** | Passes solo, fails under `nextest` parallelism | Shared fs state or global mutable. Add a nextest `[test-groups.<name>]` with `max-threads = 1` in `.config/nextest.toml`, then `[[profile.default.overrides]]` filter (see `golden-daemon`, `postgres` groups). Or actually fix the shared state. |
| **Envoy LB to standby replica** | `dashboard-gateway` body-grep for `grpc-status:0` finds nothing; HTTP 200 | `scheduler.replicas=2` → envoy load-balances; standby returns `Unavailable` as Trailers-Only (status in HTTP *headers*, empty body). Fixed via `BackendTrafficPolicy` retry-on-unavailable (`dashboard-gateway-policy.yaml`). If seen again, check the policy's `Accepted` status. |
| **nginx LB to standby replica** | `vm-dashboard-k3s` "gRPC-Web … via nginx" 60s timeout; nginx access log all `200 0` (zero body bytes) | Same standby Trailers-Only as above, but nginx has no retry policy. Fixed structurally: nginx's upstream is the leader-labeled `rio-scheduler-leader` Service (rio-lease stamps `rio.build/scheduler-role=leader` on the holder's pod) and `dashboard.nix` asserts the Service has exactly 1 ready endpoint before the nginx subtests. If the nginx gRPC-Web subtest times out again, check the EndpointSlice / lease holder / rio-lease label sweep — not LB retry or replica count. |
| **Controller probes standby scheduler** | `vm-lifecycle-gc-k3s` (or other controller-driven scenario) pool-status wait at 120s; controller log shows `rio_proto::client::balance` retrying against a `NotServing` scheduler endpoint | The controller→scheduler client variant of the standby-replica family: with `scheduler.replicas=2`, the controller's client-side balancer can park on the standby (which answers health checks but returns NotServing/Unavailable to RPCs) under builder contention. Same root family as the envoy/nginx rows; the structural fix direction is routing controller traffic through the leader-labeled Service (as nginx does). Until then: re-run — identical rebuild discriminates flake from regression (observed 2026-05-30: failed once, passed twice on the same drv). |
| **dashboard live-tail session idle-abort** | `vm-dashboard-k3s` "live tail via nginx" — the post-open `dash-live-00003` grep times out at 90s (or the one-shot batch-1 gate at 150s) under full-gate load; always solo-green | NOT the scheduler-lease family: the backend is the single-replica store, and the lease that churned was the store's PG ingest-session lease. The subtest's parked FIFO writer went mute after batch 1, so the 60s cut emptied the buffer and the driver's `inbound_idle` abort (60s) killed the session while slow gates burned the budget — batch 2 then had no live session. Fixed structurally (`dashboard.nix`): the parked writer emits empty keepalive batches (~5s; `accept()`'s explicit keepalive shape), and the subtest gates on the `log_ingest_sessions` fresh-heartbeat row (lookup_live's own predicate) before the follow-open and before the flag-touch. If a lease gate times out, the ingest chain (writer → FIFO → grpcurl → port-forward → store) died — check those, not nginx. |
| **procsub tee capture truncation** | vm-substitute stop-parity assert: starts without stops, cap.json ends mid-progress though nix exited 0; coverage variant | Plain redirect instead of `2> >(tee ...)` — bash doesn't wait for the procsub tee to flush. If it recurs WITH the direct redirect, suspect a lost Cached event (state-channel Lagged) and check drain ordering vs the final result frame. |
| **KWOK Stage machinery silent after post-CRD restart** | `vm-sla-sizing-kwok` `Registered=True` wait times out at 60s; on-failure dump shows `nodeclaims items: []` (health reap deleted the unregistered claim) and the restarted kwok-controller log has only its 4 startup lines (no Stage activity); coverage-mode run of the same commit passes | Canary gate landed 2026-06-01 (`forecast-provisioning.nix`): after the post-CRD restart, a throwaway unlabelled `canary-stage-liveness` NodeClaim must reach `Launched=True` within 15s; if not, kwok-controller is restarted and the canary retried (3 attempts), then the test fails hard with Stage objects + kwok-controller log dumped. If THAT failure appears, it is not a flake — the Stage machinery genuinely cannot see NodeClaims; check the Stage `resourceRef.apiGroup` discovery and the kwok image. Do NOT widen the 60s wait (the dropped resourceRef is permanent). |

**Strategy preference:** structural > retry > widen. Retry is cheap but hides drift; structural fixes the root.

## Reproducing a flake

For unit tests, run three ways and capture flake rate:
- Solo, serial, 10×: `nix develop -c cargo nextest run <test> --run-ignored all -j 1 --retries 0` — loop it, count fails
- Full parallelism, 10×: same with `-j $(nproc)`
- Under artificial load: `stress-ng --cpu $(nproc) &` in the background

For VM tests: `nix build .#checks.x86_64-linux.vm-<name>` 3-5×. VM tests are expensive; can't loop 20×.

## Diagnostics

- `nix log <drv-path>` for the failing derivation's full log
- For VM tests: `nix build .#checks.x86_64-linux.vm-<name>.driverInteractive` runs mypy+pyflakes on the testScript without booting a VM (~10s)
- Bisect if needed: `git bisect start HEAD <last-green-hash>`
