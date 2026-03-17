# Remediation 21: P2/P3 rollup — batched execution plan

**Parent findings:** [§3 MEDIUM](../phase4a.md#section-3-medium-remediations-p2) + [§4 LOW](../phase4a.md#section-4-low-remediations-p3)
**Scope:** 56 findings batched into 6 one-PR-each batches + deferred remainder
**Target:** operator picks up a batch, ships in <1 day, moves to the next

---

## How to use this document

Each batch is self-contained: all findings touch the same crate (or a tight
cross-crate pair), share a single `cargo nextest run -p <crate>` verification
loop, and have no inter-batch ordering dependencies. Pick any batch; ship it;
pick another.

**Ordering constraint (the only one):** Batch E (config) adds CRD fields. If
you intend to land Batch E and Batch B (worker) in the same release cycle, land
E **first** — the worker-side config reads that B touches are the same fields
E plumbs through `WorkerPoolSpec`. Landing B first is fine; landing E second
just means a second CRD schema bump in the same cycle.

Items NOT in any batch are in [§Remainder](#remainder--unbatched-findings) with
disposition notes. Three §3 findings turned out stale/mislocated on
verification; flagged inline.

---

## Batch A: rio-scheduler — 6 items, ~120 LoC, no external deps

All changes are scheduler-local: one SQL tweak, one `.trim()`, two
metric/spawn hygiene fixes, one compile-time assert, one metric label. Single
`cargo nextest run -p rio-scheduler` loop.

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| `sched-failed-workers-pg-unbounded` | `db.rs:443` | `array_append` → guard `WHERE NOT ($1 = ANY(failed_workers))`. Cap at 100 with `failed_workers[array_upper-99:array_upper]` slice on overflow. | ~15 |
| `sched-resolve-tenant-no-trim` | `grpc/mod.rs:432` | `.trim()` before lookup. **Don't** add a `NormalizedName` newtype in this batch — §3 says "4th time patching this class" but the newtype is a cross-crate refactor (gateway, store, controller all have tenant lookups). Do the `.trim()` here; file a `TODO(phase4b)` for the newtype. | ~3 |
| `sched-terminal-statuses-no-enforce` | `db.rs:25` | Unit test, not `const_assert!` — the const is `&[&str]` and `is_terminal()` is a method, so compile-time proof is awkward. Test: `for v in DerivationStatus::iter() { assert_eq!(v.is_terminal(), TERMINAL_STATUSES.contains(&v.as_str())) }`. Requires `strum::IntoEnumIterator` (already a dep). Cross-ref: §2.6 fix #3 inlines `TERMINAL_STATUSES` into SQL — this test becomes the drift-guard for **both** copies. | ~20 |
| (no ID — new) reconcile-dropped metric | `actor/mod.rs:546` | The `warn!("reconcile command dropped (channel full)")` has no metric. Add `metrics::counter!("rio_scheduler_reconcile_commands_dropped_total").increment(1)` next to the warn. Add `describe_counter!` in `lib.rs`. This is the post-recovery reconcile — if it's dropped, assigned-but-worker-gone derivations leak until the **next** recovery. Rare (channel is 1024 deep) but when it happens it's silent. | ~5 |
| (no ID — new) event-log-sweep spawn_monitored | `actor/worker.rs:588` | Bare `tokio::spawn` for the hourly `DELETE FROM build_event_log` sweep. If this panics (PG connection weirdness), there's no log — the task just disappears and the next hour's sweep spawns a new one. Switch to `spawn_monitored("event-log-sweep", ...)`. Pairs with §2.8's span-propagation fix: after both land, the sweep's error log has `component=scheduler`. | ~2 |
| (no ID — new) backstop cancel metric semantics | `actor/worker.rs:531,547` | `rio_scheduler_backstop_timeouts_total` increments unconditionally at :531. `rio_scheduler_cancel_signals_total` increments at :547 — but only if the worker is still in `self.workers` AND has a `stream_tx`. When the backstop fires because the worker is **gone** (the common case — backstop = "worker went silent"), the cancel-signals counter **doesn't** increment. This is arguably correct (no signal was sent) but the two metrics look like they should track together. Fix: add a `reason` label to `cancel_signals_total` — `reason=backstop` at :547, `reason=force_drain` at :255. Makes the gap between the two metrics explainable in a dashboard. | ~8 |

**Also in this crate, also small, roll in:**

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| `obs-recovery-histogram-success-only` | `actor/recovery.rs:329,379` | Histogram recorded only on success path. Lift `let _timer = HistogramTimer::new("rio_scheduler_recovery_duration_seconds")` to the top of the function — `Drop` records on **all** exit paths. Add `outcome` label: wrap the return value, record `outcome=ok|err` before return. | ~15 |
| `sched-leader-gauge-never-zero` | `lease/mod.rs` (search `is_leader.*gauge`) | Gauge set to 1 on acquire, never set to 0 on loss. Two replicas during transition → `sum(rio_scheduler_is_leader)` = 2. Set to 0 in the lease-lost path. Cross-ref: §2.2's local-self-fence adds the "lost" path — coordinate placement if both land together. | ~3 |

**Verification:** `nix develop -c cargo nextest run -p rio-scheduler`. The
`TERMINAL_STATUSES` test is the only new test; the metric changes are
observational (no assertion) — verify by grepping `describe_` output count
before/after.

**Estimated total:** ~70 LoC code, ~50 LoC test. One afternoon.

---

## Batch B: rio-worker — 4 items, ~90 LoC, one proptest dep (already present)

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| `wkr-scan-unfiltered` | `upload.rs:73-94` | Current: scan `$out`-prefixed paths, upload all, use basename as `output_name`. Stray tempfile under the output prefix → uploaded as a phantom output. Fix: build `drv.outputs().keys()` into a `HashSet<&str>` before the scan loop; `continue` if `output_name` isn't in the set. Log at WARN when a path is skipped (it's a real tempfile leak worth knowing about). | ~15 |
| `wkr-fod-flag-trust` | `executor/mod.rs:612` | `assignment.is_fixed_output` (scheduler-sent) vs `drv.is_fixed_output()` (derived from the `.drv` itself). The `.drv` is ground truth — it's what the worker actually executes. Replace the assignment read with `drv.is_fixed_output()`. Keep the assignment field for one release cycle with a `debug_assert_eq!(assignment.is_fixed_output, drv.is_fixed_output())` to catch scheduler-side drift; remove the proto field in a follow-up. | ~8 |
| `wkr-cancel-flag-stale` | `runtime.rs:193` | Flag set **before** kill syscall. `kill()` returns `ESRCH`/`ENOENT` when the process already exited (race: build finished naturally between "decide to cancel" and "issue kill"). Flag stays `true` → next assignment on this slot sees a stale cancel. Fix: set flag only after `kill()` returns `Ok`. On `ESRCH`, **reset** the flag (process is gone — cancellation is moot). | ~10 |
| (no ID — tracey gap) passthrough `r[verify]` | `fuse/mod.rs:8` + new test | `r[impl worker.fuse.passthrough]` exists; no `r[verify ...]` anywhere. `tracey query untested` flags it. The passthrough mode is the perf-critical path (kernel handles reads directly, no userspace copy) — it deserves an assertion. Minimum viable: unit test that mounts with `passthrough=true`, opens a file, asserts `passthrough_failures` atomic stays 0. Full verification needs a VM test (passthrough requires `CAP_SYS_ADMIN` + modern kernel) — defer to VM-test batch, but land the unit-test stub with `#[ignore]` + `r[verify worker.fuse.passthrough]` now so tracey stops flagging it. | ~25 |

**Also in this crate, also small, roll in:**

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| `wire-synth-db-narhash-format-unverified` | `synth_db.rs:68` | `debug_assert!(hash.len() == 64 && hash.chars().all(|c| c.is_ascii_hexdigit()))` before write. | ~3 |
| `wkr-proc-stat-parse-panic` | `heartbeat.rs` (search `.unwrap()` near `/proc/stat`) | `.unwrap()` on field count → `.ok_or_else(|| anyhow!("malformed /proc/stat"))`. Only fails on non-Linux (unsupported) but `unwrap` in a background loop is a crash-on-parse-error; the `?` path degrades to "skip this heartbeat's resource sample." Cross-ref: §2.4 fix #1 **adds** the `/proc/stat` sampling — if that lands first, this finding applies to the **new** code. | ~5 |

**Verification:** `nix develop -c cargo nextest run -p rio-worker`.
`wkr-scan-unfiltered` wants a test: seed the executor's output dir with a
legit output + a stray tempfile, assert only the legit one is in the upload
list.

**Ordering note:** `wkr-fod-flag-trust` removes a read of
`assignment.is_fixed_output`. §2.4 (plan `10-proto-plumbing-dead-wrong.md`)
touches the same `WorkAssignment` proto. If plan 10 lands first and removes
the field, this batch's `debug_assert_eq!` becomes dead code — fine, just drop
it.

**Estimated total:** ~65 LoC code, ~40 LoC test. One afternoon.

---

## Batch C: rio-store — 4 items, ~80 LoC, no external deps

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| `store-append-sigs-unbounded-growth` | `queries.rs:232` | `array_cat(sigs, $1)` — no dedup, no bound. Fix: `array(SELECT DISTINCT unnest(array_cat(sigs, $1::text[])) LIMIT 32)`. The `LIMIT 32` inside the subquery is the cap; `DISTINCT` deduplicates. 32 is generous — real stores have 1–3 signers. | ~8 |
| `store-pin-unvalidated-and-leaks-sqlx` | `grpc/admin.rs:320-400` | Two fixes: (1) `validate_store_path(&req.path)?` at the top, mapped to `Status::invalid_argument`. (2) The `sqlx::Error::RowNotFound` arm currently does `Status::internal(e.to_string())` which leaks `"no rows returned by a query that expected to return at least one row"` — map to `Status::not_found("path not pinned")` instead. Grep the whole `admin.rs` for `e.to_string()` in `Status::` — there are likely siblings. | ~20 |
| `store-grace-i32-cast-truncation` | `gc/mark.rs:113` | `grace_hours: u32 as i32`. At `u32::MAX` this wraps negative → interval arithmetic goes negative → sweep deletes everything. Fix at **two** sites: (1) config-load time: `ensure!(grace_hours <= i32::MAX as u32, "grace_hours out of range")` — fail fast, clear error. (2) at the cast: `.min(i32::MAX as u32) as i32` — defense-in-depth. Don't just do (2); a silently-clamped 245-year grace period is confusing. | ~10 |
| (no ID — new) drain gauge pre-register | `lib.rs:91-98` + `main.rs` | `rio_store_s3_deletes_pending` and `rio_store_s3_deletes_stuck` are **described** (`lib.rs:91-98`) but only **set** inside `drain_once` (`gc/drain.rs:183-184`), which runs on a 30s timer. For the first 30s after store startup, the gauges are **absent** from `/metrics` (metrics-rs only materializes on first `set`). PromQL `rio_store_s3_deletes_stuck > 0` can't distinguish "0" from "store didn't report yet." Fix: eagerly `gauge!("rio_store_s3_deletes_pending").set(0.0)` (and `_stuck`) right after `describe_metrics()` in `main.rs`. Zero is the correct initial value. | ~4 |

**Also in this crate, also small, roll in:**

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| `store-client-hash-trusted` | `grpc/put_path.rs:~180` | Downgraded from HIGH because no producer sends non-empty `nar_hash`. Defensive: delete the `if !req.nar_hash.is_empty()` branch entirely; always compute server-side. Reduces code. | ~-10 |
| `pg-drain-max-attempts-const-drift` | `gc/drain.rs:22` | **[Index correction: §3 table says `rio-scheduler/src/drain.rs:22` — that file doesn't exist; actual location is `rio-store/src/gc/drain.rs:22`.]** `const MAX_ATTEMPTS: i32 = 10`. §3 claims migration 009 has a matching `CHECK` constraint. **Verify this on pickup** — grep of `migrations/009_phase4.sql` for `CHECK.*attempt` found nothing; the constraint may be in a different migration or the claim is stale. If the CHECK exists: add `#[test] fn max_attempts_matches_migration() { assert_eq!(MAX_ATTEMPTS, 10) }` with a comment pointing at the migration line. If it doesn't: close as stale. | ~10 or 0 |

**Verification:** `nix develop -c cargo nextest run -p rio-store`. The
sigs-dedup fix wants a test: insert a path, call `append_sigs` twice with the
same signature, assert `sigs.len() == 1`.

**Estimated total:** ~50 LoC code (net, after the `-10` from deleting the
client-hash branch), ~40 LoC test. Half a day.

---

## Batch D: rio-common — 5 items, ~60 LoC, one crosses into rio-controller

The first four are pure `rio-common` — all in `config.rs` or
`observability.rs`. The fifth touches `rio-controller/src/main.rs` but is the
same **class** of fix (config validation at load time) and is ~5 lines.

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| `common-redact-password-at-sign` | `config.rs:160` | `after_scheme.find('@')` → `after_scheme.rfind('@')`. RFC 3986: the userinfo delimiter is the **last** `@` before the host. Password containing `@` (valid — percent-encoding is "should" not "must") currently truncates at the first `@`, leaking the tail. Add test case: `postgres://user:p@ss@host/db` → `postgres://user:***@host/db`. | ~5 |
| `common-otel-empty-endpoint` | `observability.rs:149` | `OTEL_EXPORTER_OTLP_ENDPOINT=""` currently initializes the exporter with an empty URL. Every export fails; logged at DEBUG (so: silent). Treat empty-string as unset: `env::var(...).ok().filter(|s| !s.is_empty())`. | ~3 |
| `common-otel-sample-rate-nan` | `observability.rs:155` | `f64::clamp(NaN, 0.0, 1.0)` returns `NaN` (clamp is `min(max(x, lo), hi)`; both `min` and `max` propagate NaN). Guard: `if !rate.is_finite() { warn!(...); default } else { rate.clamp(0.0, 1.0) }`. | ~5 |
| `common-log-format-case-sensitive` | `observability.rs` (search `"json"` in match arm) | `RIO_LOG_FORMAT=JSON` rejected; expects `json`. `.to_lowercase()` before match. Alternatively `eq_ignore_ascii_case` per arm. The former is one edit; the latter is future-proof against adding arms. Pick the former. | ~2 |
| (no ID — new) controller `store_addr` early warn | `rio-controller/src/main.rs:76` | `Config::default()` has `store_addr: String::new()`. If the operator forgets to set `RIO_STORE_ADDR` (or the Helm value), the first failure is at `build.rs:201` `connect_store("")` — deep inside a reconcile loop, wrapped in the `error_policy` retry backoff. The actual error is `tonic` complaining about a malformed URI. Add after figment merge: `ensure!(!cfg.store_addr.is_empty(), "RIO_STORE_ADDR not set — controller cannot pin build outputs")`. Fail-fast at startup. **Note:** this is distinct from `ctrl-connect-store-outside-main` (§3 controller) which is about **where** the connect happens — see [§Remainder](#remainder--unbatched-findings). | ~5 |

**Verification:** `nix develop -c cargo nextest run -p rio-common -p rio-controller`.
`redact_db_url` already has a test module at `config.rs:554` — add the `@`-in-
password case there. The three otel/log items can share one test: table-driven
`#[test_case("", None)] #[test_case("http://x", Some("http://x"))]` etc.

**Estimated total:** ~25 LoC code, ~35 LoC test. Two hours.

---

## Batch E: config — 3 items, ~150 LoC (the CRD one is the bulk), touches 3 crates

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| `cfg-worker-knobs-unreachable-in-k8s` | `rio-controller/src/crds/workerpool.rs:45` + `builders.rs:459-488` | Six worker config fields exist in `rio-worker/src/config.rs` but are **not** in `WorkerPoolSpec`, so operators can only set them via raw pod env-var overrides (a kustomize patch on the StatefulSet — which the CRD comment at `crds/workerpool.rs:120-122` explicitly says **doesn't work** for controller-managed STSes). The six: `daemon_timeout_secs`, `fuse_threads`, `fuse_passthrough`, `log_rate_limit`, `log_size_limit`, `max_leaked_mounts`. **Decision per field:** (a) `daemon_timeout_secs` — **plumb it.** Operators tuning this is real (some builds legitimately take >2h). (b) `fuse_threads`, `fuse_passthrough` — **plumb it.** Perf-critical; wrong value = 2× latency. Default-on, but operators need the escape hatch. (c) `log_rate_limit`, `log_size_limit` — **document as defaults-only.** The defaults (10k lines/sec, 100MiB) are already generous; if a build hits them, the build is broken, not the limit. Add a comment in `config.rs` explaining why these aren't in the CRD. (d) `max_leaked_mounts` — **document as defaults-only.** This is "when to give up" — 3 is fine everywhere. For the plumbed ones: add `#[serde(default)]` optional fields to `WorkerPoolSpec`, conditional `env()` pushes in `builders.rs` (same pattern as `scheduler.balance_host` at :503-509), regenerate CRD yaml. | ~100 |
| `cfg-zero-interval-tokio-panic` | `rio-scheduler/src/main.rs:481` | **[Index correction: §3 table says `rio-worker/src/main.rs:481` with the claim `heartbeat_interval_secs=0` — but heartbeat interval is a `const` (`rio-common/src/limits.rs:44`), not configurable. The actual zero-vulnerable field is `tick_interval_secs` at `rio-scheduler/src/main.rs:481`: `Duration::from_secs(cfg.tick_interval_secs)` → `tokio::time::interval(ZERO)` → panic.]** Fix: `ensure!(cfg.tick_interval_secs >= 1, "tick_interval_secs must be >= 1")` after figment merge. While here, grep `from_secs(cfg\.` across the workspace — any other `Duration::from_secs(config_field)` fed into `tokio::time::interval` has the same panic. | ~8 |
| (no ID — new) lease config → figment | `rio-scheduler/src/lease/mod.rs:101` | `LeaseConfig::from_env()` reads `std::env::var("RIO_LEASE_NAME")` directly. Every other config knob goes through figment (file + env + CLI merge with precedence). Lease config is the outlier. Move `lease_name`, `lease_namespace`, `identity` into the scheduler's main `Config` struct (optional fields; `None` → single-scheduler mode, same semantics as today's `from_env() -> Option<Self>`). Delete `from_env()`. Bonus: this makes lease config testable via `Config::default()` instead of `env::set_var` dances. | ~40 |

**Ordering inside this batch:** CRD change first (biggest diff, most likely to
need iteration on the yaml regen). The other two are tail-end polish.

**CRD yaml regen:** `nix develop -c cargo run -p rio-controller --bin crdgen > k8s/crds/workerpool.yaml` (or whatever the project's crdgen invocation is — grep existing make target). The diff should show **only** the three new optional fields; if it shows more, stale crdgen baseline — regenerate in a separate commit first.

**Verification:** `nix develop -c cargo nextest run -p rio-controller -p rio-scheduler`.
The builders.rs change has an existing test module at `workerpool/tests.rs:329`
(asserts on `envs.get("RIO_STORE_ADDR")`) — add assertions for the new env
vars there.

**Estimated total:** ~150 LoC code, ~30 LoC test. One day (CRD iteration is the wildcard).

---

## Batch F: docs — 4 items + 2 sweep items, ~80 LoC prose, zero code

No compile loop. Just `nix build .#checks.x86_64-linux.tracey-validate` at the end.

| ID | File:Line | Fix | LoC |
|---|---|---|---|
| (no ID — new) `proto.md` tenant_id → tenant_name | `docs/src/components/proto.md:194,234` | The proto migrated `tenant_id` → `tenant_name` (`types.proto:600` reserves `"tenant_id"`, :611 is `tenant_name`). `proto.md:194` still shows `string tenant_id = 1;` in the `SubmitBuildRequest` block; :234 still explains `tenant_id` in prose. Update both. While there, check if `types.proto:685,764` still having `tenant_id` is intentional (different messages — `Tenant` and `TenantQuota` — where "id" is the UUID, not the name) or another drift. | ~10 |
| (no ID — new) `proto.md` StoreAdminService section | `docs/src/components/proto.md` | `store.proto:80` defines `service StoreAdminService` (TriggerGC, PinPath, UnpinPath per `store.md:354`). `proto.md` has no section for it. Add one mirroring the existing `SchedulerService` / `StoreService` sections: RPC table, request/response shapes, one-line semantics per RPC. ~30 lines. | ~30 |
| `doc-fuse-five-constraint-not-colocated` | — | **Already covered by plan `16-fuse-waiter-timeout.md` §4** ("lift the 5-constraint doc into module scope" — inline diff showing the module docstring). Close here as DUP; land via plan 16. | 0 |
| `doc-observability-metric-name-drift` | `docs/src/observability.md` | Spec lists `rio_scheduler_dag_nodes`; code emits `rio_scheduler_dag_nodes_total`. Grep `observability.md` for **every** metric name, cross-check against `describe_*!` calls workspace-wide. Expect 2–5 drifts. | ~10 |
| `doc-grace-hours-default` | `docs/src/components/store.md` | Spec says default `grace_hours=6`; config default is `2`. One of them is wrong. **Check git blame on both** — if the config changed recently and the doc didn't follow, fix the doc. If the spec was the intent and the config drifted, that's Batch C territory (actual behavior change). Most likely: doc fix. | ~2 |
| (sweep) remove stale comments | `rio-scheduler/src/actor/recovery.rs:169-174`, `rio-scheduler/src/db.rs:23`, `CLAUDE.md` coverage section | Three `doc-*-lies` items in §4 point at comments fixed by P0/P1 plans (`01-poison-orphan-recovery.md`, §2.6, `09-build-timeout-cgroup-orphan.md` respectively). **Don't touch these here** — they'll conflict. This row is a reminder to **verify** they were removed when the P0/P1 plans land. | 0 |

**Verification:** `nix build .#checks.x86_64-linux.tracey-validate` (catches
broken `r[...]` refs if any metric-name edits touched annotated lines).
`mdbook build docs/` to check the markdown renders.

**Estimated total:** ~50 LoC prose. Two hours.

---

## Batch G: test-coverage — DEFERRED

The §4 test-coverage gaps are real but overlap heavily with P0/P1 plan
verification steps:

| §4 item | Where it's actually covered |
|---|---|
| `nix-hash-no-proptest` | §5 `nix-fod-hash-modulo-omits-outpath` fix requires new golden hashes → proptest the corrected function |
| `wire-stderr-no-proptest` | Plan `07-stderr-error-desync.md` hardens `StderrWriter` → proptest the state machine (finished/error XOR) there |
| `wire-no-golden-query-substitutable-paths` | — | Not covered. Standalone golden; add opportunistically. |
| `wire-no-golden-query-valid-derivers` | — | Same. |
| `wire-no-golden-add-signatures` | Plan `07` touches the sigs path; add golden there |
| `fuse-no-enospc-test` | Plan `16-fuse-waiter-timeout.md` restructures fetch — ENOSPC test fits naturally |
| `gc-no-concurrent-putpath-test` | §2.14 `pg-putpath-shared-lock-pool-exhaustion` fix changes the lock semantics → the concurrent test IS the verification |
| `store-sign-no-proptest` | §1.2 `store-sign-fingerprint-empty-refs` (plan `02`) rewrites sign — proptest there |
| `sched-dag-no-cycle-proptest` | — | Not covered. Standalone; add opportunistically. |
| `nix-proptest-escapes` | — | Not covered. Standalone; add opportunistically. |

**Disposition:** close the "covered by P0/P1" rows here; track the three
standalone goldens + two standalone proptests as a single `TODO(phase4b)`
opportunistic-test PR. ~200 LoC test, no code.

---

## Remainder — unbatched findings

Items from §3/§4 that didn't fit the six batches above, with disposition.

### Stale / mislocated — close or re-verify

| ID | Status | Notes |
|---|---|---|
| `obs-fallback-reads-unregistered` | **Likely STALE** | §3 claims `rio_store_fallback_reads_total` is incremented but never described. Workspace grep for `fallback_reads` and `fallback.*read` in `rio-store/src/`: zero hits. Metric either renamed or removed. Re-verify; if absent, close. |
| `cfg-zero-interval-tokio-panic` | **Mislocated** | Index says `rio-worker/src/main.rs:481` with `heartbeat_interval_secs` — but that's a `const`, not config. Real vulnerability is scheduler `tick_interval_secs`. Moved to Batch E with corrected location. |
| `pg-drain-max-attempts-const-drift` | **Mislocated, possibly stale** | Index says `rio-scheduler/src/drain.rs` — doesn't exist. Actual const is `rio-store/src/gc/drain.rs:22`. The claimed `CHECK` constraint in migration 009 not found by grep. Moved to Batch C with verification step. |

### Controller items — hold for plan 03 coordination

| ID | Why held | Notes |
|---|---|---|
| `ctrl-cleanup-blocks-reconcile-worker` | Plan `03-controller-stuck-build.md` restructures the reconcile loop | The "spawn cleanup as separate task" fix is a ~50 LoC change that **will** conflict with plan 03's drain_stream rewrite. Coordinate: either roll into plan 03, or land after. |
| `ctrl-store-err-mislabeled` | Same file, same plan | `build.rs:201` — plan 03 touches this heavily. |
| `ctrl-connect-store-outside-main` | Same file, same plan | Plan 03 already moves clients into `Ctx` (the "balanced channel" pattern from `lang-gotchas.md`). This finding is a sibling: store client gets the same treatment. Verify plan 03 covers it; if not, one-line addendum. |
| `obs-watch-reconnects-unregistered` | Same file, trivially batchable with plan 03 | `rio_controller_build_watch_reconnects_total` incremented at `build.rs:824`; NOT in `describe_metrics()` at `main.rs:341-371`. One `describe_counter!` line. Land with plan 03. |
| `kube-last-sequence-negative-cast` | Cosmic-ray tier | `u64 as i64` wraps at 2⁶³. At one event per nanosecond, that's 292 years. Not worth a separate PR; if plan 03 touches the sequence handling, add `.try_into().expect("sequence overflow — 292yr uptime")` as a drive-by. |

### Gateway items — hold for plan 07 coordination

| ID | Why held | Notes |
|---|---|---|
| `gw-submit-build-bare-question-mark-no-stderr` | Plan `07-stderr-error-desync.md` | Same file (`build.rs`), same class (`STDERR_ERROR` discipline). Plan 07's `StderrWriter` hardening makes this class impossible; the three `?` sites at `:203,210,213` get the `stderr_err!` treatment as part of plan 07's sweep. |
| `wire-query-realisation-error-after-stderr-last` | Plan `07-stderr-error-desync.md` | §3 literally says "Same class as §2.1 but different site." Add `opcodes_read.rs:543-584` to plan 07's sweep list. |
| `gw-temp-roots-unbounded-insert-only` | Standalone — but decision required | `temp_roots` set: insert-only, never removed, never read by GC. Memory leak + dead feature. The fix is **either** wire it to GC roots (feature work, ~100 LoC) **or** delete it (~-20 LoC). The §3 table doesn't pick. Decision: delete. The set has been dead since phase2a; if GC temp-roots are needed later, build them right. One-line PR. |

### PG items — larger scope, separate PR each

| ID | Disposition | Notes |
|---|---|---|
| `pg-dual-migrate-race` | **Separate small PR** | Both scheduler and store run `sqlx::migrate!()`. The race is benign (one blocks on the advisory lock, the other proceeds) but it wastes ~1s of startup time and double-logs migration output. Fix: store is the migrator (it owns the most tables); scheduler does `SELECT MAX(version) FROM _sqlx_migrations` and `ensure!`s it's ≥ its expected version. ~30 LoC across two crates. |
| `pg-zero-compile-time-checked-queries` | **Opportunistic, not a batch** | 194 `query()` → 0 `query!()`. Converting all 194 is a multi-week slog. §3 says "start with the 12 queries that have had runtime SQL errors in git history" — **that** is a batch. Requires: `git log -p --all -S 'sqlx::Error' -- '**/*.rs'` to find the 12, then `DATABASE_URL` pointing at a real schema for `query!` macro expansion. ~2h research + ~1 day conversion. Separate PR. |

### P3 minor-robustness — opportunistic drive-bys

These are too small for their own PR. Land each as a drive-by when touching
the file for any other reason.

| ID | File | One-line fix |
|---|---|---|
| `cli-status-partial-print` | `rio-cli/src/status.rs` | Connect before printing the header. |
| `test-pg-db-name-collision` | `rio-test-support/src/pg.rs` | `{timestamp}_{pid}_{rand_suffix}`. 4-char base36 suffix. |
| `ssh-auth-timing-side-channel` | `rio-gateway/src/ssh/auth.rs` | §4 self-rates "negligible." Close WONTFIX unless a security review asks. |
| `store-narinfo-bom-reject` | `rio-store/src/narinfo.rs` | `.trim_start_matches('\u{FEFF}')` at parse entry. |
| `tonic-worker-reconnect-no-backoff` | `rio-worker/src/main.rs:522` | Fixed 1s → exponential capped at 10s. **Note:** index groups this under "§3 scheduler" but it's in `rio-worker`. Could roll into Batch B if it's convenient; it's isolated enough to be a drive-by otherwise. |

### P3 code-quality — defer to phase4b

Module splits (`gw-handler-mod-1200-lines`, `store-queries-mod-900-lines`)
are refactors that invalidate every open PR touching those files. Defer until
the P0/P1 churn settles.

| ID | Disposition |
|---|---|
| `wire-magic-numbers-unnamed` | `TODO(phase4b)` — drive-by when touching `rio-nix/src/protocol/` |
| `sched-dag-node-mut-unwrap` | `TODO(phase4b)` — `.unwrap()` → `.expect("msg")` sweep, 11 sites |
| `common-metrics-macro-duplication` | `TODO(phase4b)` — extract `describe_all!()` macro to `rio-common` |
| `gw-handler-mod-1200-lines` | `TODO(phase4b)` — split after plan 07 lands (touches same file) |
| `store-queries-mod-900-lines` | `TODO(phase4b)` — split after Batch C lands (touches same file) |
| `nix-wire-dead-read-bool` | Drive-by delete. One branch, verified-dead since 2024. Land with any `rio-nix/src/protocol/` PR. |

---

## Batch execution summary

| Batch | Crate(s) | Items | LoC | Time | Gate |
|---|---|---|---|---|---|
| A | rio-scheduler | 8 | ~120 | 1 afternoon | `nextest -p rio-scheduler` |
| B | rio-worker | 6 | ~105 | 1 afternoon | `nextest -p rio-worker` |
| C | rio-store | 6 | ~90 | half day | `nextest -p rio-store` |
| D | rio-common (+ctrl) | 5 | ~60 | 2 hours | `nextest -p rio-common -p rio-controller` |
| E | config (3 crates) | 3 | ~180 | 1 day | `nextest -p rio-controller -p rio-scheduler` + crdgen diff |
| F | docs | 6 | ~50 prose | 2 hours | `tracey-validate` + `mdbook build` |
| G | test-coverage | 10 | DEFER | — | covered by P0/P1 plans |
| — | remainder | 18 | — | — | see per-item disposition |

**Total batchable:** 34 items, ~600 LoC, ~4 days serial / ~1.5 days parallel (A/B/C/D are independent; E after B; F anytime).

**End-of-rollup gate:** `nix-build-remote -- .#ci` after all six batches land. The VM tests exercise the scheduler backstop path, the worker upload filter, and the store grace-hours clamp under real conditions — unit tests catch the logic, VM tests catch the integration.
