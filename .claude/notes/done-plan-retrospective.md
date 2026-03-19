# DONE Plan Retrospective — 2026-03-19

## Summary

- **8 phases scanned** (P0000–P0203 backfill, 204 plans)
- **26 findings** across 24 distinct plans →
  - **11 need code/plan remediation** (4 correctness, 1 perf, 6 tech-debt)
  - **11 doc-rot only** (plan-doc errata, stale comments, README)
  - **4 audited, no action** (still-fine)
- **0 phases aged with zero findings** — every phase surfaced at least one assessment, but phases 1b/2a/2b each have ≤1 non-doc issue (healthiest)
- **Pattern that emerges:** the 4a remediation sweep (P0180–P0203) left its own wake — 5 of the 11 code items are `r[impl]`-tagged-but-`r[verify]`-missing cleanup the sweep explicitly committed to but didn't land

---

## Remediation queue (ranked: correctness > perf > tech-debt)

### P0129 — deferred-unlanded (severity: **correctness**)

**The decision:** DrainWorker(force=true) was built as the preemption hook — scheduler iterates `running_builds`, sends `CancelSignal` per build → worker `cgroup.kill()`. Plan said controller would wire it "on DisruptionTarget condition (phase 4 wiring) or on WorkerPool delete."

**What aged:** Phase 4a landed. **Zero production callers of the `if force { … }` block** at `rio-scheduler/src/actor/worker.rs:211-258`. Exhaustive caller check: both `DrainWorkerRequest` construction sites (`rio-controller/src/reconcilers/workerpool/mod.rs:336`, `rio-worker/src/main.rs:694`) set `force: false`. No `DisruptionTarget` watcher exists (`grep -rn DisruptionTarget rio-controller/` = 0). No `preStop` hook in `builders.rs`.

**Four comment sites lie in present tense** — they assert the wiring exists:
- `rio-scheduler/src/actor/worker.rs:219-225` — "when the controller sees DisruptionTarget condition on a pod, it calls DrainWorker(force=true)"
- `rio-scheduler/src/actor/tests/worker.rs:342-344` — "controller sees DisruptionTarget on a pod, calls this"
- `rio-controller/src/reconcilers/workerpool/builders.rs:162-165` (PDB docstring) — "builds in flight on the evicting pod get reassigned (DrainWorker force)"
- `rio-controller/src/reconcilers/workerpool/mod.rs:148-151` — "evicting pod's builds get reassigned (via DrainWorker force → preemption)"

**Runtime impact:** On node drain / spot-interrupt / Karpenter consolidation, evicted worker pods self-drain with `force=false` and wait up to 2h (`termination_grace_period_seconds.unwrap_or(7200)` at `builders.rs:433`) for in-flight builds — then SIGKILL loses them anyway. Builds that could have been reassigned in seconds instead burn up to 2h of wall-clock before restarting cold.

**Fix:** Add Pod watcher in rio-controller filtered to workerpool-owned pods with `status.conditions[type=DisruptionTarget,status=True]` → call `DrainWorker{force:true}`. Simpler alternative: `preStop` exec hook in `builders.rs` STS spec that curls scheduler admin endpoint with `force=true` before SIGTERM lands. Rewrite the 4 present-tense comments to `TODO(phaseX)` until wired.

---

### P0007 — limitation-persists (severity: **correctness**)

**The decision:** P0007 closed phase-1a with "seccomp profile installed but unvalidated (bypassed by `privileged: true`)" noted as a minor tail-end deferral.

**What aged:** Eight phases later, **the deferral chain terminates in a void.**

1. `privileged: true` is still the shipped default — `infra/helm/rio-build/values.yaml:386`
2. `values.yaml:382` comment claims "ADR-012 defers to phase 5" — **fabricated**; ADR-012 (`docs/src/decisions/012-privileged-worker-pods.md`) never mentions phase 5
3. `phase5.md` (read in full, 48 lines) has no device-plugin/seccomp/privileged task
4. The "ADR-012 separate track" at `rio-worker/src/cgroup.rs:304` doesn't exist — no plan doc, no phase task, no DAG entry
5. `plan-0284` (UNIMPL) T6 will retag `TODO(phase5)` → `TODO(adr-012)` — relabeling into a tag with no schedule
6. `phase4c.md:59` schedules `seccomp-rio-worker.json` — but ADR-012:10 confirms `privileged: true` "disables seccomp profiles entirely"; the profile will be **silently inert**
7. `hostUsers: false` (ADR-012:25-32 recommendation) also unimplemented — no grep hits in `builders.rs`
8. **The kicker:** plan-0284 T4 will REMOVE `docs/src/introduction.md:50`'s "Multi-tenant deployments with untrusted tenants are unsafe before Phase 5" warning, while `privileged: true` (which ADR-012:18 calls "Unacceptable security posture for a multi-tenant build service") remains the default with no scheduled removal

**Fix:** Add a device-plugin + `hostUsers:false` + privileged-default-flip task to `phase5.md` (or a dedicated plan doc) and **gate plan-0284's introduction.md:50 warning-removal on it**. Fix `values.yaml:382`'s false "ADR-012 defers to phase 5" claim to point at the actual owner.

---

### P0151 — deferred-unlanded (severity: **correctness**)

**The decision:** P0151 was "the core observability gap phase 4a was chartered to close" — cross-service distributed tracing via `link_parent()`.

**What aged:** P0160 round-4 **proved** gateway→scheduler linkage NEVER worked (only scheduler→worker works, via round-3 data-carry). `nix/tests/scenarios/observability.nix:268-283` still has `TODO(phase4b)` with assertion that "stays GATEWAY-ONLY until this is fixed." 36 commits past phase-4a tag, headline goal still unlanded.

The buggy pattern is live: `rio-scheduler/src/grpc/mod.rs:350` `#[instrument]` decorates `submit_build`, then `:359` calls `link_parent(&request)` — `set_parent()` on an already-created `#[instrument]` span produces an orphan with its OWN trace_id (LINKED not parented). Jaeger shows two traces, not one.

**rem-14 (P0193, span-propagation remediation) does NOT cover this** — it's about `spawn_monitored` component-field propagation, a different bug (zero overlap with `interceptor.rs`/`grpc/mod.rs`/`observability.nix`). `docs/src/phases/phase4b.md` has no line item for this. Exhaustive grep of 78 UNIMPL plans P0205–P0284 for `link_parent|set_parent|GATEWAY-ONLY|observability.nix|trace.*linkage`: zero hits.

**Fix:** New phase4b plan implementing option (b) from `observability.nix:279-280` — return scheduler `trace_id` in `SubmitBuild` initial metadata, gateway emits THAT in `STDERR_NEXT` (same pattern as P0199's `build_id`-in-headers). Also fixes the P0160 doc-rot (lying docstring at `rio-proto/src/interceptor.rs:136-139`) by making the claim true again.

---

### P0188 — untested-marked (severity: **correctness**)

**The decision:** rem-09 core fix — `daemon.kill()` only kills direct child, builder grandchild survives in cgroup → EBUSY leak → EEXIST on next assignment. Fix: inline `build_cgroup.kill()` + bounded drain poll. Plan noted: "UNTESTED at phase-4a — VM test proposed in remediation but different marker verified."

**What aged:** `tracey query untested` still flags `worker.cgroup.kill-on-teardown`. `r[impl]` at `rio-worker/src/executor/mod.rs:639`, NO `r[verify]` anywhere. The proposed VM fragment (fully spec'd at `docs/src/remediations/phase4a/09-build-timeout-cgroup-orphan.md:605-700`) was **never transplanted** — `grep 'build-timeout|timeoutSeconds|kill-on-teardown' nix/tests/` returns empty.

The related-but-DIFFERENT subtest `cancel-cgroup-kill` at `lifecycle.nix:878` verifies `r[worker.cancel.cgroup-kill]` — that's the CANCEL RPC path (delete Build CR → CancelBuild RPC → `runtime.rs` handler), NOT the timeout-teardown path at `executor/mod.rs:640`. **Two distinct call sites; only one is guarded.**

**Fix:** Add `build-timeout` subtest fragment to `nix/tests/scenarios/lifecycle.nix` per already-written spec at `09-build-timeout-cgroup-orphan.md:605-700` (`sleepSecs=30` drv + `Build.spec.timeoutSeconds=5` → assert `phase=Failed` not `Pending-reassign`, cgroup gone, second build of same drv succeeds no EEXIST). Add `# r[verify worker.cgroup.kill-on-teardown]` at col-0 file header per tracey `.nix` parser constraint.

---

### P0089 — limitation-persists (severity: **perf**)

**The decision:** Worker FUSE-cache bloom filter designed as "never deleted from — evicted paths stay as stale positives. Only restart clears it. 50k expected items at 1% FPR ≈ 60 KB. If actual inventory blows past this, FPR degrades gracefully."

**What aged:** **Zero saturation observability** was ever added — and the documented indirect signals **point the wrong way.**

- `rio-common/src/bloom.rs:43-319` exposes `num_bits()`/`hash_count()`/`byte_len()` but NO `fill_ratio()`/`popcount()`. Grep `rio_worker_bloom` = zero hits. No gauge.
- `rio-worker/src/fuse/cache.rs:208` `BLOOM_EXPECTED_ITEMS = 50_000` still compile-time const
- Workers now deploy as StatefulSet pods (`rio-controller/src/reconcilers/workerpool/builders.rs:331-337`) — stable identity, long-lived. Low-ordinal workers can run indefinitely under autoscaler scale-down.

**The failure is silent and the existing metrics diagnose the OPPOSITE:**

- `rio_worker_prefetch_total{result="already_cached"}` docstring says high = "scheduler bloom filter stale" — that's heartbeat LAG (bloom MISSING paths). **Saturation is the opposite**: FPR rises → bloom claims paths worker DOESN'T have → scheduler SKIPS hint (`dispatch.rs:530 .filter(|p| !bloom.maybe_contains(p))`) → `already_cached` **decreases**, not increases.
- `rio_scheduler_prefetch_paths_sent_total` doc says "High avg = workers cold." Saturation causes LOW avg — **indistinguishable from "locality scoring is working great."**
- `W_LOCALITY=0.7` dominance at `assignment.rs:153`: under saturation, `count_missing()` undercounts → `transfer_cost` converges to 0 → scoring silently becomes pure `W_LOAD=0.3`. No metric reports "locality term contributing 0."

**Fix:** Add `BloomFilter::fill_ratio(&self) -> f64` at `rio-common/src/bloom.rs` (sum `self.bits.iter().map(|b| b.count_ones())` / `self.num_bits`, ~60KB scan = microseconds). Emit `metrics::gauge!("rio_worker_bloom_fill_ratio")` in `rio-worker/src/runtime.rs:96-106` where bloom snapshot is already cloned every 10s. Document alert threshold ~0.5 (at k=7, fill≥0.5 → FPR climbs past 1% nonlinearly) in `docs/src/observability.md`.

---

### P0020 — doc-code-drift (severity: tech-debt)

**The decision:** P0020 Outcome: "No `from_utf8_lossy` in production code paths." P0017 rationale: lossy→parse silently produces U+FFFD → confusing ATerm parse error instead of clear UTF-8 error.

**What aged:** The exact pattern P0017 commit `2f807a4` eliminated is **live at HEAD** in `rio-worker/src/executor/mod.rs:315` — `String::from_utf8_lossy(&assignment.drv_content)` before `Derivation::parse()`. Introduced by `395e826f` ONE DAY after P0020 completed. The parallel branch of the **same if-statement** (line 313 → `fetch_drv_from_store` → `parse_from_nar` at `rio-nix/src/derivation/mod.rs:168`) uses strict `from_utf8`. Two branches, same logical bytes, different UTF-8 handling.

Repo-wide grep confirms `:315` is the ONLY parse-downstream lossy case (others are log display / test assertions / panic messages — acceptable).

**Fix:** At `rio-worker/src/executor/mod.rs:315` replace `String::from_utf8_lossy(&assignment.drv_content)` with `std::str::from_utf8(&assignment.drv_content).map_err(|e| ExecutorError::BuildFailed(format!("drv content is not valid UTF-8: {e}")))?` — matches strict handling the else-branch already gets.

---

### P0116 — deferred-unlanded (severity: tech-debt)

**The decision:** Controller Build CRD reconciler: "TODO(phase4): full DAG reconstruction for inputDrvs not yet built." Module doc: "Single-node DAG (phase3a scope)."

**What aged:** `rio-controller/src/reconcilers/build.rs:238-239` still submits `nodes: vec![node], edges: vec![]`. Phase 4a is DONE. The "Phase 4 deferral" is **prose, not a `TODO(phase4)` tag** — `grep 'TODO(phase4' build.rs` = zero hits, violates CLAUDE.md TODO discipline.

The fix exists but isn't called: `rio-gateway/src/translate.rs:41` `pub async fn reconstruct_dag` is `pub`, controller could call it but doesn't.

`docs/src/components/controller.md:9-43` has `> **Phase 4 deferral:**` blocks for OTHER things (criticalPathRemaining, conditions) but NONE for the single-node limitation — so the `phase4c.md:66` "sweep all Phase 4 deferral blocks" task **will not catch this**. No forward plan in `.claude/work/plan-02*.md` addresses it.

**Fix:** Three-part:
1. Add `> **Phase deferral:**` block to `docs/src/components/controller.md` near `r[ctrl.crd.build]` so `phase4c.md:66` sweep catches it
2. Replace `build.rs:25-27` prose with properly tagged `// TODO(phase5):` comment
3. Write forward plan — either controller calls `rio_gateway::translate::reconstruct_dag`, or decide permanently out-of-scope and reword docs to "CRD path is for pre-built closures; use SSH for DAG builds"

---

### P0191 — untested-marked (severity: tech-debt)

**The decision:** rem-12 fix moved `self.dag.node_mut().db_id` write to AFTER `tx.commit()`. Plan noted prior code was "accidentally correct — `rollback_merge` removed phantom-`db_id` nodes wholesale — but relied on three nonlocal invariants."

**What aged:** `tracey query untested` lists `sched.db.tx-commit-before-mutate`. `r[impl]` at `rio-scheduler/src/actor/merge.rs:429`, NO `r[verify]`. Remediation doc `docs/src/remediations/phase4a/12-pg-transaction-safety.md:1087-1107` **proposed option (b): ~8-line `#[cfg(debug_assertions)]` loop** asserting `state.db_id.is_none()` for all `newly_inserted` nodes just before `tx.commit()`, explicitly endorsed at line 1107 as "sufficient." **Never landed.**

The existing `test_merge_db_failure_rolls_back_memory` (merge.rs:13-51) closes the pool BEFORE sending MergeDag — `insert_build` fails first, `persist_merge_to_db` is never reached. Doesn't exercise the invariant.

**Fix:** Insert the ~8-line `#[cfg(debug_assertions)]` loop from remediation doc lines 1091-1100 immediately before `tx.commit().await?` at `rio-scheduler/src/actor/merge.rs:427`, tagged `// r[verify sched.db.tx-commit-before-mutate]`. Fires in every existing merge test's happy path — closes tracey gap with zero new test scaffolding.

---

### P0194 — untested-marked (severity: tech-debt)

**The decision:** rem-15 replaced hand-rolled SIGTERM-only handler with `shutdown_signal()` (SIGTERM|SIGINT). Before this, Ctrl+C during dev hit default SIGINT → no Drop → FUSE mount leaked + profraw lost. Plan noted: "proposed VM test fragment NOT found in codebase at phase-4a."

**What aged:** Still true. `tracey query untested` lists `worker.shutdown.sigint`. Fully-designed test exists ONLY as prose at `docs/src/remediations/phase4a/15-shutdown-signal-cluster.md:382-470` (`sigint-graceful` fragment: `systemctl kill -s INT` → assert `mountpoint -q` fails → assert profraw count increased). Never ported.

**Concrete risk per CLAUDE.md §Coverage step 3:** `.#coverage-full` depends on worker `main()` returning normally on SIGINT so atexit flushes profraw. A `main.rs` refactor that breaks the cancellation arm at `main.rs:498-500` would **silently zero out worker VM coverage.**

**Fix:** Port `sigint-graceful` fragment from `15-shutdown-signal-cluster.md:382-470` into `nix/tests/scenarios/lifecycle.nix` fragments attrset (uses `wsmall2`, asserts `mountpoint -q` fails after `systemctl kill -s INT` + profraw count increased in coverage mode). Add `# r[verify worker.shutdown.sigint]` to col-0 file-header comment block.

---

### P0200 — deferred-unlanded (severity: tech-debt)

**The decision:** Remediation doc `21-p2-p3-rollup.md:66` explicitly committed: "land the unit-test stub with `#[ignore]` + `r[verify worker.fuse.passthrough]` now so tracey stops flagging it" — ~25 LoC estimate.

**What aged:** A specific LoC-estimated commitment was **dropped on the floor.** `tracey query untested` confirms `worker.fuse.passthrough` is the ONLY untested rule in the entire project (1 of 187). The `passthrough_failures: AtomicU64` field it would assert against DOES exist at `rio-worker/src/fuse/mod.rs:56`. Plan-0200 line 26 says other deferred items got `TODO(phase4b)` comments; this one didn't even get a tracking TODO.

**Fix:** Add `#[ignore = "passthrough requires CAP_SYS_ADMIN + Linux 6.9+; full verify deferred to VM test"]` unit test in `rio-worker/src/fuse/mod.rs` tests module, annotated `// r[verify worker.fuse.passthrough]`, asserting `passthrough_failures.load() == 0` after mount+open cycle. ~25 LoC, spec'd exactly at `21-p2-p3-rollup.md:66`.

---

### P0027 — deferred-unlanded (severity: tech-debt)

**The decision:** `derivation_edges.is_cutoff BOOLEAN DEFAULT FALSE` added for "future CA early cutoff support."

**What aged:** Dead schema weight that will **remain dead after CA cutoff lands** — P0252 (UNIMPL, CA cutoff) uses a completely different mechanism (`DerivationStatus::Skipped` enum variant + `find_cutoff_eligible()` DAG walker). P0252's text never mentions `is_cutoff`. Zero `.rs` references; INSERT at `db.rs:814` omits it (relies on DEFAULT); SELECT at `db.rs:1009` omits it.

**Finding missed:** P0276 (UNIMPL, `GetBuildGraph` RPC) plans to `SELECT e.is_cutoff` and expose as proto `GraphEdge.is_cutoff` with comment "phase5 core wires this (P0252 Skipped cascade)" — but P0252 does NOT wire it. **P0276 would ship an always-FALSE misleading dashboard field.** `GraphNode.status` (P0276:23) already carries "skipped" after P0252, so edge-level `is_cutoff` is redundant.

**Fix:** Drop `is_cutoff` from P0276's `GraphEdge` proto + SELECT (`plan-0276:29,68`). Drop the column via new migration (`ALTER TABLE derivation_edges DROP COLUMN is_cutoff`). Remove from `docs/src/components/scheduler.md:506`.

---

## Doc rot (comment/doc updates, no behavior change)

| Plan | File(s) | What's stale | Fix |
|---|---|---|---|
| **P0128** ⚠ | `rio-scheduler/src/main.rs:416-427`, `docs/src/components/controller.md:237,242` | CRITICAL comment insists "scheduler.yaml MUST have `readinessProbe: grpc: service: ...`" — **false since 187a98a6**; manifest uses `tcpSocket`. Reader who "fixes" manifest per comment will crash-loop standby (per `scheduler.yaml:124-127` warning). | Rewrite comment: "client-side balancer (rio-proto/src/client/balance.rs) probes named service; K8s probes use tcpSocket — DO NOT change manifest to gRPC probe (crash-loops standby)." Drop `scheduler.yaml` from `controller.md:237` claim. **MAINTENANCE TRAP — prioritize.** |
| **P0160** ⚠ | `rio-proto/src/interceptor.rs:42-43,136-139`, `rio-scheduler/src/grpc/mod.rs:355-358,455-459`, `docs/src/observability.md:255` | 5 sites claim `link_parent()` → "same trace_id / contiguous trace." VM-PROVEN FALSE (P0160 round-4, commit 3204e4af). `#[instrument]` span is orphan with own trace_id, LINKED not parented. Last touch to interceptor.rs was 72min AFTER proof; nobody fixed. | Rewrite all 5 to: "creates OTel span LINK (not parent): `#[instrument]` span keeps own trace_id; Jaeger shows two linked traces — see TODO(phase4b) at `observability.nix:268`." **Partly obviated if P0151-fix lands.** |
| **P0068** | `README.md:98-102` | `nix build .#ci-local-fast` / `.#ci-fast` — **neither attribute exists** (retired 89ba88fd). New contributor copying from README gets `attribute not found`. Comment says "30s fuzz" (now 120s). | Collapse to single `nix build .#ci` block; fix "30s" → "2min fuzz". |
| **P0143** | `flake.nix:250`, `nix/coverage.nix` header | `RUSTFLAGS = "-C instrument-coverage"` has NO warning about branch coverage. `-Z coverage-options=branch` was tried (8126dcf), segfaulted `llvm-cov export` at ~15GB RSS with 20+ object files, reverted (4c8365d). Diagnostic info lives ONLY in commit 395c0493 — which was itself reverted. Surviving reverts are bare "This reverts commit X." | Add DO-NOT comment at `flake.nix:250`: "Do NOT add `-Z coverage-options=branch` — llvm-cov export segfaults at ~15GB RSS with 20+ object files (tried 8126dcf, reverted 4c8365d, diagnostic in 395c049)." |
| **P0200** | `docs/src/remediations/phase4a.md:1457-1588` | Index table: 129 OPEN, 1 FIXED — but ≥6 spot-checked entries are fixed (wkr-cancel-flag-stale, common-redact-password-at-sign, common-otel-empty-endpoint, cfg-zero-interval-tokio-panic, wkr-fod-flag-trust, gw-temp-roots-unbounded-insert-only). Index was INPUT to 21-plan sweep; nobody closed the loop. | Sweep table against 21 per-remediation docs + plan-0200 §Remainder: flip OPEN→FIXED(rem-NN). ~5-10 genuinely-open items stay OPEN with `TODO(phase4b)`. |
| **P0051** | `rio-worker/src/synth_db.rs:13,93,158` | "Realisations: currently empty (CA support deferred)" — implies temporary + globally-deferred. Both wrong. Table empty PRE-build is **permanent by design** (nix-daemon INSERTs post-CA-build; rio never populates — scheduler resolves CA inputs before dispatch). CA landed at store/gateway in 2c. | Rewrite to "empty pre-build by design — nix-daemon INSERTs here post-CA-build; rio never populates (scheduler resolves CA inputs before dispatch per phase5.md)." **Also:** add note to P0253 that `drv_content` must never be empty for CA-resolved drvs (executor/mod.rs:312 + derivation.rs:352 fallbacks fetch unresolved .drv). |
| **P0154** | `rio-store/src/cache_server/auth.rs:7,36`, `docs/src/security.md:69` | `TODO(phase4b)` mistagged — phase4b.md deliberately omits this; `security.md:82` + `phase5.md:19` assign to Phase 5. Also `security.md:69` lists "Bearer token authentication" as not-yet-implemented — it shipped (r[impl store.cache.auth-bearer]). | Retag `auth.rs:36` `TODO(phase4b)`→`TODO(phase5)`, `auth.rs:7` "in 4b"→"in Phase 5". Strip "Bearer token authentication" from `security.md:69` not-yet-implemented list. |
| **P0113** | `.claude/work/plan-0113-closure-size-estimator-fallback.md:9,19` | Claims `MEMORY_MULTIPLIER` constant + `HistoryEntry.input_closure_bytes` field — **neither ever existed in code** (`git log -S` returns only the backfill doc commit itself). Fabricated by rio-backfill-clusterer. Actual commit f343eb9 is DURATION proxy only. | Rewrite to describe DURATION proxy (`CLOSURE_BYTES_PER_SEC_PROXY` 10MB/s, `CLOSURE_PROXY_MIN_SECS` 5s floor). Delete fabricated claims. |
| **P0005** | `.claude/work/plan-0005-live-daemon-golden-conformance.md:33,51,63` | "Intentional divergence: wopNarFromPath sends STDERR_WRITE — both work, more robust" — **all false**. Bug #11, fixed 132de90b (140 commits after phase-1b). Causes `error: no sink`. `gateway.md:76` already has Historical note; plan doc has no erratum. | Append `[WRONG — bug #11, fixed 132de90b in phase 2a: Nix client's processStderr() passes no sink → error: no sink. See gateway.md:76.]` after line 51. Strike "intentional" on lines 33, 63. |
| **P0076** | `.claude/work/plan-0076-figment-config.md:25` | Describes `config_defaults_match_phase2a()` test guarding `fuse_mount_point=/nix/store` + gateway `/tmp/*` defaults — 2ab2d22e (P0097) renamed to `config_defaults_are_stable` and FIXED those three defaults as dangerous. `fuse_passthrough` guard alone remains valid. | Append forward-pointer: "(2ab2d22e/P0097 later renamed test → `config_defaults_are_stable`, fixed 3 dangerous defaults. `fuse_passthrough` guard remains.)" |
| **P0025** | `.claude/work/plan-0025-nar-streaming-refactor.md:58` | "HashingReader — phase 2 uses verbatim" — `HashingReader` was `#[cfg(test)]`-gated at 68571efa (wrapping already-accumulated Vec doubled peak memory, ~8GiB for 4GiB NAR). `FramedStreamReader` survived. | Append: "(Later: HashingReader cfg(test)-gated at 68571efa — gRPC chunk accumulation already buffers into Vec; NarDigest::from_bytes on slice is production path. FramedStreamReader survived.)" |

---

## Audited, no action

| Plan | Phase | Verdict | Note |
|---|---|---|---|
| P0007 (seccomp aspect) | 1a | still-fine | Scanner's core claim re: `cgroup.rs:301` is correct but separate from the privileged-void finding above |
| P0066 | 2b | still-fine | DrvHash/WorkerId moved to rio-common with note — file-list drift expected for backfill |
| P0085 | 2c | still-fine | VM coverage for breaker/StoreUnavailable = zero, confirmed — but unit coverage sufficient for circuit breaker |
| P0127 | 3a | still-fine | "any new non-idempotent PG write post-3a would break this" — audit found no new violators |

---

## Aged gracefully (scanner found nothing)

| Phase | Plans | Note |
|---|---|---|
| *(none)* | — | Every phase surfaced ≥1 assessment. Healthiest: **1b** (2 findings, 1 trivial code fix + 1 outcome-footnote), **2b** (3 findings, all doc-only/still-fine — zero code changes needed), **2a** (2 findings, 1 dead-column + 1 comment rewrite). |

---

## Per-phase findings distribution

| Phase | Plans | Assessed | aged-badly | doc-rot | still-fine | Code remediation |
|---|---|---|---|---|---|---|
| 1a | 8 | 2 | 1 | 1 | 1 | 1 (P0007 correctness) |
| 1b | 18 | 2 | 1 | 1 | 0 | 1 (P0020 tech-debt) |
| 2a | 31 | 2 | 1 | 1 | 0 | 1 (P0027 tech-debt) |
| 2b | 25 | 3 | 0 | 2 | 1 | **0** |
| 2c | 15 | 2 | 1 | 0 | 1 | 1 (P0089 perf) |
| 3a | 31 | 3 | 1 | 1 | 1 | 1 (P0116 tech-debt) |
| 3b | 21 | 3 | 2 | 1 | 0 | 1 (P0129 correctness) |
| 4a | 55 | 8 | 5 | 3 | 0 | 5 (P0151, P0188 correctness; P0191, P0194, P0200 tech-debt) |

**4a carries the most weight** — expected (most recent, largest, and explicitly a remediation sweep that left its own tail). All 5 of 4a's code remediations have fully-spec'd fixes already written in `docs/src/remediations/phase4a/*.md` that were never ported.

---

## Followup plan seeds

Candidate `/plan --inline` rows (next available: P0285+). Clustered where work naturally batches; deps assume `204` as the phase-4b fan-out root per existing DAG convention.

### Correctness

```json
{"plan":285,"title":"DrainWorker(force=true) wiring — DisruptionTarget watcher or preStop hook","deps":[204],"deps_raw":null,"tracey_total":2,"tracey_covered":0,"crate":"rio-controller,rio-scheduler","status":"UNIMPL","complexity":"MED","note":"retro P0129 correctness — force=true path has ZERO prod callers; 4 comments lie in present tense. Simpler alt: preStop exec hook curls sched admin. builders.rs:433 grace=7200s means builds burn up to 2h before SIGKILL loses them anyway."}
```

```json
{"plan":286,"title":"privileged-worker hardening: device-plugin + hostUsers:false + values.yaml default flip","deps":[204],"deps_raw":null,"tracey_total":3,"tracey_covered":0,"crate":"rio-controller,infra","status":"UNIMPL","complexity":"HIGH","note":"retro P0007 correctness — ADR-012 track was never scheduled; values.yaml:382 fabricates phase-5 deferral authority. GATES plan-0284 T4 (introduction.md:50 warning removal). phase4c.md:59 seccomp JSON will be silently inert until this lands."}
```

```json
{"plan":287,"title":"gateway→scheduler trace linkage — trace_id via SubmitBuild initial metadata","deps":[204],"deps_raw":null,"tracey_total":1,"tracey_covered":0,"crate":"rio-scheduler,rio-gateway,rio-proto","status":"UNIMPL","complexity":"MED","note":"retro P0151 correctness — P0160 PROVED link_parent+#[instrument] = orphan span (LINKED not parented). Option (b) from observability.nix:279: return sched trace_id in metadata, gw emits in STDERR_NEXT (same pattern as P0199 build_id-in-headers). Un-TODOs observability.nix:268. Obsoletes P0160 doc-rot."}
```

### Perf

```json
{"plan":288,"title":"Bloom saturation gauge: fill_ratio() + rio_worker_bloom_fill_ratio metric","deps":[204],"deps_raw":null,"tracey_total":1,"tracey_covered":0,"crate":"rio-common,rio-worker","status":"UNIMPL","complexity":"LOW","note":"retro P0089 perf — zero saturation observability; existing indirect signals point WRONG way (saturation DECREASES already_cached, indistinguishable from 'locality working great'). popcount over 60KB = microseconds. Emit in runtime.rs:96 10s snapshot loop. Alert threshold ~0.5 in observability.md."}
```

### Tech-debt — tracey-untested batch (P0188+P0194+P0200 share pattern: spec'd-but-not-ported)

```json
{"plan":289,"title":"tracey-untested cleanup: port 3 spec'd-but-unlanded verify fragments","deps":[204],"deps_raw":null,"tracey_total":3,"tracey_covered":0,"crate":"rio-worker,nix-tests","status":"UNIMPL","complexity":"MED","note":"retro P0188+P0194+P0200 tech-debt — worker.cgroup.kill-on-teardown (09-doc:605), worker.shutdown.sigint (15-doc:382), worker.fuse.passthrough (#[ignore] stub 21-doc:66). All 3 have exact specs in remediation docs. sigint guards .#coverage-full profraw flush. Port to lifecycle.nix + fuse/mod.rs tests. col-0 tracey .nix constraint."}
```

### Tech-debt — standalone

```json
{"plan":290,"title":"regression-guard trivia: from_utf8 strict in executor + tx-commit debug_assert","deps":[204],"deps_raw":null,"tracey_total":1,"tracey_covered":0,"crate":"rio-worker,rio-scheduler","status":"UNIMPL","complexity":"LOW","note":"retro P0020+P0191 tech-debt — executor/mod.rs:315 lossy→strict (P0017 pattern, ~5 LoC); merge.rs:427 insert 8-line #[cfg(debug_assertions)] db_id.is_none() loop (12-doc:1091, endorsed line 1107). Closes sched.db.tx-commit-before-mutate tracey gap. Both trivial."}
```

```json
{"plan":291,"title":"drop is_cutoff dead column + strip from P0276 GraphEdge proto","deps":[204,276],"deps_raw":null,"tracey_total":0,"tracey_covered":0,"crate":"rio-scheduler,rio-proto","status":"UNIMPL","complexity":"LOW","note":"retro P0027 tech-debt — P0252 (CA cutoff) uses DerivationStatus::Skipped not edge flag; P0276 would ship always-FALSE misleading dashboard field. Drop from plan-0276:29,68 + migration ALTER DROP + scheduler.md:506. GraphNode.status already carries 'skipped'."}
```

```json
{"plan":292,"title":"controller Build-CRD DAG reconstruction — scope decision + TODO retag","deps":[204],"deps_raw":null,"tracey_total":1,"tracey_covered":0,"crate":"rio-controller","status":"UNIMPL","complexity":"LOW","note":"retro P0116 tech-debt — build.rs:25 prose-deferral not TODO-tagged (phase4c:66 sweep won't catch). reconstruct_dag is pub at translate.rs:41. DECIDE: call it, or declare CRD=pre-built-closures-only. Either way: add controller.md deferral block + proper TODO(phase5) tag."}
```

### Doc-rot sweep (batched)

```json
{"plan":293,"title":"doc-rot sweep: 11 stale comments/docs from retrospective","deps":[204],"deps_raw":null,"tracey_total":0,"tracey_covered":0,"crate":"docs,rio-scheduler,rio-proto,rio-worker,rio-store","status":"UNIMPL","complexity":"LOW","note":"retro doc-only batch — PRIORITY: P0128 (main.rs:416 CRITICAL comment is a maintenance trap, 'fixing' per comment crash-loops standby) + P0160 (5 sites claim link_parent=same-trace-id, VM-proven false). Also: P0068 README.md:98 dead attrs, P0143 flake.nix:250 branch-cov warning, P0200 remediation index 129→~10 OPEN, P0051 synth_db comments, P0154 TODO retag+security.md, P0113/P0005/P0076/P0025 plan-doc errata. No behavior change; skip-CI candidate except clippy (comment-only)."}
```
