# CI failure patterns

Reference catalog of `.#ci` failure signatures that have bitten this project at least once. Check here when CI is red and the cause isn't obvious from the log tail.

## Deterministic failures

| Pattern | Symptom | Fix |
|---|---|---|
| **Nightly-only syntax** | compiles in `nix develop`, fails in `.#ci` | `if let` chain guards, `let_chains`, etc. — devshell is nightly, CI is stable. Rewrite or use `nix develop .#stable`. |
| **Stale fuzz Cargo.lock** | fuzz build fails, `rio-nix/fuzz/Cargo.lock` or `rio-store/fuzz/Cargo.lock` out of sync | Per-crate fuzz workspaces have independent lockfiles. `cd <crate>/fuzz && cargo update -p <crate>`. |
| **codecov after_n_builds drift** | `codecov-matrix-sync` check fails: `after_n_builds=N but coverage matrix has M entries` | Added/removed a VM coverage target without bumping `.github/codecov.yml`. Update `after_n_builds` to match. |
| **tracey broken ref** | `tracey-validate` fails: `r[impl X]` has no matching spec marker | Code has `// r[impl foo.bar]` but `docs/src/components/*.md` lacks `r[foo.bar]`. Either add the spec marker (standalone paragraph, blank line before, col 0) or fix the typo. |
| **pyflakes f-string** | VM test fails at lint, not runtime: `F541 f-string without placeholders` | nixos-test-driver runs pyflakes on `testScript`. `f"foo"` with no `{...}` is a pyflakes error. Drop the `f` prefix. |
| **IFD × non-determinism** | VM test cert mismatch: `x509: certificate signed by unknown authority (crypto/rsa: verification error)` — but the CA CN matches | `builtins.readFile(runCommand ... ${nondeterministic})` pulls eval-time build contents into a string; remote builder rebuilds with DIFFERENT contents. Convert to a `runCommand` that takes the derivation as a regular build input. |
| **Coverage profraw timeout** | `coverage-full` or `cov-vm-*` hits `globalTimeout` | Coverage-mode k3s tests have builder-disk I/O variance. Bump `globalTimeout` with headroom. Check if tmpfs is wired for the containerd store. |
| **Unregistered metric** | test passes but metric is always zero in production | Metric is `emit!()`ed but never `.register()`ed in the component's `lib.rs`/`main.rs`. Grep for the metric name — registration and emission are two separate call sites. |
| **helm template fails** | `helm-lint` check fails | `infra/helm/rio-build/` chart doesn't render with one of the `values/*.yaml` files. `helm template rio . -f values/dev.yaml` locally to see the error. |
| **Rustdoc intra-doc lint** | `cargo doc` fails on `[nonexistent]` | broken `[Type]` links in doc comments — use `` [`Type`](path) `` or escape as `\[...\]`. |
| **rustfmt drift** | `treefmt` check fails | nightly rustfmt vs stable rustfmt format differently; run `nix develop .#stable -c cargo fmt`. |
| **Nix `''` in Python comment** | VM test: syntax error at a comment line | `''` in a Python comment inside a Nix `''...''` string is a string terminator. Reword the comment. |
| **statix style** | statix lint → shows under the `pre-commit` check, not standalone | `inherit (pkgs) lib` not `lib = pkgs.lib`. Mechanical fix. |

## Flaky tests

| Pattern | Signature | Fix strategies |
|---|---|---|
| **k3s airgap import timing** | VM test flakes on agent readiness — airgap imports serially/alphabetically before kubelet | Gate on server-node-exists (validated 3/3 — agent-Ready 106.70→1.9s, 56×). Budget for tail, not typical (builder variance 5×). |
| **flannel subnet race** | `loadFlannelSubnetEnv failed: open /run/flannel/subnet.env` early in boot | Gated since 7679316a (`k3s-full.nix:720`). The log line is now a benign blip that recovers in <1s; if a subtest still times out, look past the flannel error for the real cause. |
| **job-tracking finalizer orphan** | `ephemeral-pool` pod-phase wait at 180s; pod cleaned on node but `phase=Running` in apiserver | `reap_excess_pending` background-delete raced Job-Complete → pod's `batch.kubernetes.io/job-tracking` finalizer orphaned. Fixed at source (`job_common.rs:reap_excess_pending` → foreground propagation). If seen again, check for another `DeleteParams::background()` on a Job (e.g. `reap_orphan_running`). |
| **Machine.succeed() thread-unsafe** | `rc int-on-empty` when bg+main threads both call succeed | Use `--wait=false` instead of threading. |
| **kubectl logs poll churn** | `http2: stream closed` errors under TCG — `kubectl logs\|grep` in wait_until_succeeds triggers kubelet churn | Don't poll logs for readiness — use cgroup/kernel/metric state instead. |
| **Wall-clock gate under load** | `assert!(elapsed < Ns)` flakes under builder CPU contention | **(a)** retry-N-times; **(b)** widen gate with documented slack budget; **(c)** convert to structural assertion — count ops, not wall-clock. Prefer (c). |
| **Parallel test order-dependence** | Passes solo, fails under `nextest` parallelism | Shared fs state or global mutable. Add a nextest `[test-groups.<name>]` with `max-threads = 1` in `.config/nextest.toml`, then `[[profile.default.overrides]]` filter (see `golden-daemon`, `postgres` groups). Or actually fix the shared state. |
| **Envoy LB to standby replica** | `dashboard-gateway` body-grep for `grpc-status:0` finds nothing; HTTP 200 | `scheduler.replicas=2` → envoy load-balances; standby returns `Unavailable` as Trailers-Only (status in HTTP *headers*, empty body). Fixed via `BackendTrafficPolicy` retry-on-unavailable (`dashboard-gateway-policy.yaml`). If seen again, check the policy's `Accepted` status. |

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
