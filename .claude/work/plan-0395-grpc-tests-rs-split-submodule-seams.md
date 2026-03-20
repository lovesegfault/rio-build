# Plan 0395: Split grpc/tests.rs along P0356 seams (1682L → 5 files)

[`rio-scheduler/src/grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) is **1682L** — the same trajectory as `admin/tests.rs` (1732L) that motivated [P0386](plan-0386-admin-tests-rs-split-submodule-seams.md). P0386's dag-row note at `:140` already flagged this: "grpc/tests.rs ALSO 1682L — same trajectory, out of scope but flagged."

[P0356](plan-0356-split-scheduler-grpc-service-impls.md) split `grpc/mod.rs` into `scheduler_service.rs` / `worker_service.rs` / `actor_guards.rs`. The tests weren't split — they still live in one monolith that covers both service impls plus helper fn tests. Section boundaries (from `grep '#[tokio::test]|^// ---'`):

| Range | Cluster | Tests | Seam (P0356 prod file) |
|---|---|---|---|
| `:28-296` | BuildExecution bidi-stream end-to-end + log pipeline | 2 | `worker_service.rs` |
| `:297-799` | SubmitBuild validation (~14 tests: empty fields, oversized, priority, tenant resolve, trace-id) | 14 | `scheduler_service.rs` |
| `:800-965` | actor_error_to_status mapping + time-ordered UUID v7 | 4 | `actor_guards.rs` (+ helper) |
| `:966-1175` | since_sequence replay + bridge_build_events (4 tests: lagged, pg-replay, post-subscribe dedup, pg-failure) | 4 | `scheduler_service.rs` (stream bridge) |
| `:1176-1493` | BuildExecution malformed-msg paths (duplicate-register, completion-None, empty-stream) + leader-guard | 4 | `worker_service.rs` + `actor_guards.rs` |
| `:1494-1682` | jti revocation in SubmitBuild (section marker at `:1494`) | 1+ | `scheduler_service.rs` |

Split into: `submit_tests.rs` / `stream_tests.rs` / `bridge_tests.rs` / `guards_tests.rs` + shared `mod.rs` fixtures.

## Entry criteria

- [P0356](plan-0356-split-scheduler-grpc-service-impls.md) merged (`scheduler_service.rs` / `worker_service.rs` / `actor_guards.rs` exist as the seams to mirror)
- [P0386](plan-0386-admin-tests-rs-split-submodule-seams.md) merged (establishes the `tests/{domain}_tests.rs + mod.rs` split pattern this plan copies)

## Tasks

### T1 — `refactor(scheduler):` grpc/tests/mod.rs — shared setup

NEW `rio-scheduler/src/grpc/tests/mod.rs`. Extract the setup shared across all clusters:

- `use` block at `:3-19` (actor helpers, proto clients/servers, TestDb, seed_tenant, Duration, StreamExt, MIGRATOR)
- Any `fn setup_*` or `async fn spawn_*` helpers that appear in ≥2 clusters (grep `fn ` in `tests.rs` — there's likely a `spawn_scheduler_grpc` or similar)
- Module declarations: `mod submit_tests;` etc.

The `r[verify proto.stream.bidi]` annotation at `:1` moves to `stream_tests.rs:1` (it annotates the bidi-stream coverage specifically, not the whole file).

### T2 — `refactor(scheduler):` submit_tests.rs — SubmitBuild validation chain

NEW `rio-scheduler/src/grpc/tests/submit_tests.rs`. Move from `:297-799` + `:1494-1682`:

- `test_submit_build_rejects_*` × 7 (empty drv_hash/drv_path/system, oversized, invalid_priority, unknown_tenant, too_many_edges)
- `test_submit_build_resolves_known_tenant` (`:467`) — carries `r[verify sched.tenant.resolve]` at `:431`
- `test_submit_build_sets_trace_id_header` + `_no_otel_` (`:507,579`) — carry `r[verify obs.trace.scheduler-id-in-metadata]` at `:497`
- `test_submit_build_empty_tenant_is_none` (`:608`)
- `test_resolve_tenant_rpc` + `_works_on_standby` (`:643,705`) — carries `r[verify sched.tenant.resolve]` at `:636`
- jti-revocation test(s) from `:1494+` — carries `r[verify gw.jwt.verify]` at `:1495`

~600L total. The two `r[verify sched.tenant.resolve]` annotations (at `:431` and `:636`) move with their test fns. Check at dispatch with `grep 'r\[verify' rio-scheduler/src/grpc/tests.rs` in the `:297-799` range.

### T3 — `refactor(scheduler):` stream_tests.rs — BuildExecution bidi + malformed-msg

NEW `rio-scheduler/src/grpc/tests/stream_tests.rs`. Move from `:28-296` + `:1176-1420`:

- `test_build_execution_stream_end_to_end` (`:28`) — carries `r[verify proto.stream.bidi]` at `:1`
- `test_log_pipeline_grpc_wire_end_to_end` (`:178`)
- `test_heartbeat_rejects_too_many_running_builds` (`:767`) — worker-side validation, goes here
- `test_build_execution_duplicate_register_ignored` (`:1221`)
- `test_build_execution_completion_none_result_synthesizes_failure` (`:1284`)
- `test_build_execution_empty_stream_rejected` (`:1401`)

~500L. The `r[verify proto.stream.bidi]` at `:1` annotates the end-to-end test specifically — move to `stream_tests.rs:1`.

### T4 — `refactor(scheduler):` bridge_tests.rs — since_sequence replay + BuildEvent bridge

NEW `rio-scheduler/src/grpc/tests/bridge_tests.rs`. Move from `:866-1174`:

- `test_bridge_build_events_lagged_sends_data_loss` (`:867`)
- `test_build_ids_are_time_ordered_v7` (`:906`)
- `test_bridge_replays_from_pg_and_dedups_broadcast` (`:1025`)
- `test_bridge_post_subscribe_events_pass_dedup` (`:1074`)
- `test_bridge_pg_failure_falls_through_no_dedup` (`:1115`)
- `test_read_event_log_half_open_range` (`:1154`)

~320L. No tracey markers in this cluster per the grep; double-check at dispatch.

### T5 — `refactor(scheduler):` guards_tests.rs — actor_guards + leader-gate

NEW `rio-scheduler/src/grpc/tests/guards_tests.rs`. Move from `:800-865` + `:1421-1493`:

- Three `#[test]` (non-async) at `:805,849,856` — likely `actor_error_to_status` mapping unit tests
- `test_not_leader_rejects_all_rpcs` (`:1428`) — carries `r[verify sched.grpc.leader-guard]` at `:1422`

~180L. Small file but the seam is clear: `actor_guards.rs` is the prod file these tests cover.

### T6 — `refactor(scheduler):` delete monolith + wire tests/ mod

MODIFY `rio-scheduler/src/grpc/mod.rs` — if it has `#[cfg(test)] mod tests;` pointing at a single file, change to `#[cfg(test)] mod tests;` pointing at the `tests/` directory (the `mod.rs` inside handles submodule wiring). DELETE `rio-scheduler/src/grpc/tests.rs`.

Post-split: `wc -l rio-scheduler/src/grpc/tests/*.rs` should roughly sum to 1682L minus ~50L of dedup'd imports (the shared `use` block moves to `mod.rs` once).

## Exit criteria

- `wc -l rio-scheduler/src/grpc/tests.rs` → file-not-found (deleted)
- `wc -l rio-scheduler/src/grpc/tests/*.rs` → 5 files, sum ≈1600-1700L
- `cargo nextest run -p rio-scheduler grpc::tests` → all pass (same test count as pre-split; check `--list` diff)
- `nix develop -c tracey query rule proto.stream.bidi` → shows ≥1 verify site at `grpc/tests/stream_tests.rs` (annotation moved)
- `nix develop -c tracey query rule sched.tenant.resolve` → shows ≥2 verify sites in `grpc/tests/submit_tests.rs`
- `nix develop -c tracey query rule sched.grpc.leader-guard` → shows ≥1 verify site at `grpc/tests/guards_tests.rs`
- `nix develop -c tracey query rule gw.jwt.verify` → shows ≥1 verify site at `grpc/tests/submit_tests.rs`
- `grep 'r\[verify' rio-scheduler/src/grpc/tests/` → count matches pre-split `grep -c 'r\[verify' rio-scheduler/src/grpc/tests.rs` (no annotations lost)
- `/nbr .#ci` green

## Tracey

References existing markers (annotations MOVE with tests, not added):
- `r[proto.stream.bidi]` — verify site moves `tests.rs:1` → `tests/stream_tests.rs:1`
- `r[sched.tenant.resolve]` — two verify sites move to `tests/submit_tests.rs`
- `r[obs.trace.scheduler-id-in-metadata]` — verify site moves to `tests/submit_tests.rs`
- `r[sched.grpc.leader-guard]` — verify site moves to `tests/guards_tests.rs`
- `r[gw.jwt.verify]` — verify site moves to `tests/submit_tests.rs`

No new markers. Pure file-reorg; tracey sees the same annotations in new locations (config.styx `include` glob `rio-scheduler/src/**/*.rs` at `:43` covers the new paths).

## Files

```json files
[
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "DELETE", "note": "T6: monolith deleted after split"},
  {"path": "rio-scheduler/src/grpc/tests/mod.rs", "action": "NEW", "note": "T1: shared use block + helpers + mod declarations (~100L)"},
  {"path": "rio-scheduler/src/grpc/tests/submit_tests.rs", "action": "NEW", "note": "T2: SubmitBuild validation ×14 + tenant resolve + jti-revocation (~600L); r[verify sched.tenant.resolve]×2 + r[verify obs.trace.scheduler-id-in-metadata] + r[verify gw.jwt.verify]"},
  {"path": "rio-scheduler/src/grpc/tests/stream_tests.rs", "action": "NEW", "note": "T3: BuildExecution bidi + malformed-msg ×6 (~500L); r[verify proto.stream.bidi]"},
  {"path": "rio-scheduler/src/grpc/tests/bridge_tests.rs", "action": "NEW", "note": "T4: since_sequence replay + BuildEvent bridge ×6 (~320L)"},
  {"path": "rio-scheduler/src/grpc/tests/guards_tests.rs", "action": "NEW", "note": "T5: actor_error_to_status + leader-gate ×4 (~180L); r[verify sched.grpc.leader-guard]"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T6: #[cfg(test)] mod tests; → dir (no-op if already points at dir via mod.rs)"}
]
```

```
rio-scheduler/src/grpc/
├── mod.rs               # T6: tests mod wire
└── tests/
    ├── mod.rs           # T1: shared setup
    ├── submit_tests.rs  # T2: ~600L
    ├── stream_tests.rs  # T3: ~500L
    ├── bridge_tests.rs  # T4: ~320L
    └── guards_tests.rs  # T5: ~180L
```

## Dependencies

```json deps
{"deps": [386, 356], "soft_deps": [311, 304, 295, 383], "note": "discovered_from=386 (rev-p386 trivial — P0386's dag-row note flagged this trajectory). P0386 (DONE) establishes the tests/{domain}_tests.rs split pattern this plan copies verbatim; SEQUENCE P0386 FIRST so the pattern exists as a reference. P0356 (DONE) created the scheduler_service.rs/worker_service.rs/actor_guards.rs seams this split mirrors. Soft-dep P0311-T22 (adds test_progress_arm_ema_counter_fires to grpc/tests.rs — after this split, lands in submit_tests.rs or stream_tests.rs instead; coordinate at dispatch). Soft-dep P0304-T141 (hoists actor_error_to_status to actor_guards.rs — T5 here tests that fn; sequence-independent, test body unchanged). Soft-dep P0295-T43 (edits grpc/tests.rs:1582 assert msg — :1582 is in the jti-revocation block that T2 moves to submit_tests.rs; if P0295-T43 lands first, T2 carries the fix; if this lands first, P0295-T43 retargets to submit_tests.rs). Soft-dep P0383 (DONE — actor_guards.rs exists; mentioned for context of the P0356 seam). grpc/tests.rs count=many (it's the 1682L monolith — by definition high-collision for any plan adding scheduler grpc tests); this split REDUCES future collisions by giving new tests a narrower target file."}
```

**Depends on:** [P0386](plan-0386-admin-tests-rs-split-submodule-seams.md) — pattern precedent. [P0356](plan-0356-split-scheduler-grpc-service-impls.md) — prod seams to mirror.

**Conflicts with:** Any plan adding tests to [`grpc/tests.rs`](../../rio-scheduler/src/grpc/tests.rs) — [P0311](plan-0311-test-gap-batch-cli-recovery-dash.md)-T22, [P0295](plan-0295-doc-rot-batch-sweep.md)-T43. After this split, those retarget to the appropriate `tests/{domain}_tests.rs`. Sequence: this plan BEFORE new-test additions if possible; otherwise, T2-T5 carry the additions forward.
