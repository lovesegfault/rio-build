# Plan 0127: Post-tracey docs audit — 40 doc corrections + false-annotation removal

## Design

**Tracey revealed half the spec was stale.** With `r[...]` markers mapping every spec claim to code, discrepancies that had been invisible became `tracey query` results: "rule `r[sched.lease.pg-advisory]` has no impl site" → because PG advisory locks were never implemented; P0114 used K8s Lease. Fifty commits — one audit pass across every spec doc — brought `docs/src/` into agreement with the codebase.

The audit found three classes of stale spec:

**Never-implemented features documented as done:** ADR-007 described PG advisory locks + LISTEN/NOTIFY (`59cab1a` — scheduler uses K8s Lease, tick-based). `failure-modes.md` described FUSE read timeout + worker circuit breaker (phase3b deferrals — reads block until 2h `daemon_timeout`). `configuration.md` listed `w_locality`/`w_load` as configurable (compile-time consts), `orphan_timeout` + `preemption_reserve` (neither exists). `verification.md` claimed arbtest + testcontainers (neither used).

**Wrong implementation details:** `worker.md` FUSE mount point `/nix/store` → actually `/var/rio/fuse-store` (`2bb77dc`). `gateway.md` `wopAddMultipleToStore` wire format wrong (`8655d4f`). `scheduler.md` heartbeat timeout, misclass detection, backpressure hysteresis values all drifted (`b10a1f0`, `a7bbc90`). `store.md` PutPath step order wrong (`7f46a8c`, `11e72b7`). `proto.md` missing `DerivationNode.input_srcs_nar_size`, `HeartbeatRequest.size_class`, `CompletionReport` fields, `PutPathTrailer` (`12597f4`). `observability.md` trace propagation claims wrong (`ed4a915`).

**False tracey annotations** — 41 `r[impl]`/`r[verify]` removed, 8 added. Six `fix(tracey)` commits: `c732044` moved penalty-overwrite + never-running to real impl sites (were on test helpers). `903ec91` moved FUSE lookup-caches annotations to correct files. `2228c9c` removed `ctrl.pdb.workers` + `ctrl.probe.named-service` (kustomize concerns, not code). `46dc37e` removed false `sec.*` annotations (mTLS/HMAC/auth are phase 5 deferrals, not implemented). `f005fa5` removed 19 false `r[verify]` annotations where tests didn't verify the claim — "honest untested reporting" (e.g., `sched.build.keep-going` had no `keep_going=true` test; `worker.ns.order` is privileged-only, untestable in unit tests). `8bb8151` removed false `obs.*` verify annotations.

Embedded feats discovered during audit: `f6e61ae` added `describe!()` for controller reconcile metrics (P0123 emitted but didn't describe them). `9513cf2` added custom histogram buckets for build duration (default buckets were milliseconds; builds take minutes-to-hours). `152db45` fixed prod NetworkPolicy (removed no-op `except` clause, added `192.168/16` to allow — VM tests use that range). Small chores: `0aaec2e` (migration ref comments: `006_phase2c.sql` → `002_store.sql`), `06c1356` (cgroup `own_cgroup` comments → `delegated_root`), `53ae282` (proto stale line refs).

## Files

```json files
[
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "dontCheckSigs discarded not enforced; wopAddMultipleToStore wire format; STDERR_ERROR exception for wopQueryRealisation"},
  {"path": "docs/src/components/scheduler.md", "action": "MODIFY", "note": "K8s Lease rewrite (was PG advisory); heartbeat timeout/misclass/backpressure values; state machine poisoned triggers"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "FUSE mount /var/rio/fuse-store not /nix/store; output upload no atomicity; Key Files section"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "PutPath flow/step order; inline blob claims; ChunkBackend deferral removed (it's done)"},
  {"path": "docs/src/components/controller.md", "action": "MODIFY", "note": "sentinel, field names, condition types; NetPol/RBAC/GC as kustomize-or-deferred"},
  {"path": "docs/src/components/proto.md", "action": "MODIFY", "note": "systems plural; BloomHash BLAKE3; DerivationNode fields; HeartbeatRequest; CompletionReport; PutPathTrailer"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "trace propagation; required log fields; undocumented metrics; custom histogram buckets"},
  {"path": "docs/src/failure-modes.md", "action": "MODIFY", "note": "K8s Lease not PG; split-brain 15s bounded window; NO PG write fencing; FUSE timeout phase3b deferral"},
  {"path": "docs/src/configuration.md", "action": "MODIFY", "note": "w_locality/w_load are consts not config; remove orphan_timeout+preemption_reserve; OTel env-only not figment"},
  {"path": "docs/src/decisions.md", "action": "MODIFY", "note": "ADR-007 PG advisory locks never implemented; ADR-005 FUSE+overlay details; ADR-012 privileged escape"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "Phase deferrals for mTLS/HMAC/multi-tenancy auth"},
  {"path": "docs/src/multi-tenancy.md", "action": "MODIFY", "note": "Phase 5 deferrals JWT/auth/quotas"},
  {"path": "docs/src/verification.md", "action": "MODIFY", "note": "fuzz target names/counts; remove arbtest/testcontainers claims"},
  {"path": "docs/src/dependencies.md", "action": "MODIFY", "note": "remove unused deps; fix feature flags; add missing crates"},
  {"path": "docs/src/data-flows.md", "action": "MODIFY", "note": "wopNarFromPath wire format; wrong RPC names"},
  {"path": "docs/src/errors.md", "action": "MODIFY", "note": "BuildStatus mappings; retry/timeout reality"},
  {"path": "docs/src/deployment.md", "action": "MODIFY", "note": "controller/store replica claims; config field names"},
  {"path": "docs/src/architecture.md", "action": "MODIFY", "note": "rio-fuse crate name fix; list rio-common/rio-nix"},
  {"path": "docs/src/crate-structure.md", "action": "MODIFY", "note": "add rio-controller; fix file splits; module trees"},
  {"path": "docs/src/challenges.md", "action": "MODIFY", "note": "Challenge 11 FUSE mitigations; Challenge 12/14 values"},
  {"path": "docs/src/phases/phase3a.md", "action": "MODIFY", "note": "stale deferrals; missing bugs + TODOs"},
  {"path": "rio-scheduler/src/actor/tests/dispatch.rs", "action": "MODIFY", "note": "10 false r[verify] removed (keep-going, hysteresis, critical-path, ema-alpha, penalty, mem-bump, preempt, merge dedup/priority)"},
  {"path": "rio-store/tests/grpc/main.rs", "action": "MODIFY", "note": "4 false r[verify] removed (tests are error classification not flow verification)"},
  {"path": "rio-controller/src/reconcilers/workerpool/tests.rs", "action": "MODIFY", "note": "2 false r[verify] removed (no deletionTimestamp test, no double-reconcile test)"},
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "3 false r[verify] removed (privileged-only, untestable in unit tests)"},
  {"path": "rio-worker/src/fuse/ops.rs", "action": "MODIFY", "note": "lookup-caches + passthrough annotations moved from wrong file"},
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "custom histogram buckets for build_duration (minutes-to-hours not milliseconds)"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "describe! for reconcile_duration + reconcile_errors"},
  {"path": "infra/overlays/prod/networkpolicy.yaml", "action": "MODIFY", "note": "remove no-op except clause; add 192.168/16 allow"}
]
```

## Tracey

**Net −33 annotations** (8 added, 41 removed). 1 new spec marker. The removals were the point: `tracey query untested` had been showing falsely-low counts because of `r[verify]` annotations on tests that didn't actually verify the claim. Post-audit `tracey query untested` is honest — 19 rules show as untested that were previously marked verified. This is the correct state: they ARE untested (keep-going, hysteresis, namespace ordering — all VM-only or need-test-written).

## Entry

- Depends on P0126: the audit was **driven by** tracey — finding where spec text didn't match annotated code. Without P0126 there would have been no systematic way to find the stale claims.

## Exit

Merged as `3d26a83..11e72b7` (50 commits). `.#ci` green at merge. `11e72b7` is the `phase-3a` tag. `tracey query validate` → `"0 total error(s)"`. Every spec doc in `docs/src/` reviewed and corrected. The CLAUDE.md "keep docs/code in sync" discipline is now mechanically enforced.
