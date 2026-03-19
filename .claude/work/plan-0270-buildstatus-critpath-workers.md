# Plan 0270: BuildStatus criticalPathRemaining+workers fields (absorbs 4c A3/P0238 deferral)

Per 4c A3: "if `BuildEvent` carries worker IDs, trivial." Verify at dispatch: `grep -A3 'worker_id\|worker_name' rio-proto/proto/types.proto`.

Dashboard consumes these fields — [P0278](plan-0278-dashboard-build-list-drawer.md) shows `criticalPathRemaining` in the progress column.

**Audit B2 #16 + retro P0116/P0294 scope change:** Plan targeted admin.proto (no messages there). `critpath_remaining_secs` needs DAG + ema — only scheduler computes. **Proto + scheduler ONLY** (P0294 rips the Build CRD; the "+ CRD" row below is obsolete):

| Crate | Change |
|---|---|
| `rio-proto` | `BuildEvent::Progress` gets `optional uint64 critpath_remaining_secs` + `repeated string assigned_workers` (types.proto, NOT admin.proto) |
| `rio-scheduler` | compute critpath on each Progress emit; populate assigned_workers from in-mem assignments |
| `rio-controller` (crds/build.rs) | CRD status gets `Option<i64> critical_path_remaining_secs` + `Vec<String> assigned_workers`, both `skip_serializing_if`. `apply_event` mirrors from Progress. (k8s JSON can't do uint64 — i64 per crds/build.rs:140-149) |

Dashboard + rio-cli read from gRPC stream (live). kubectl reads CRD (polling). Scheduler is single source of truth.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged
- [P0238](plan-0238-buildstatus-conditions-fields-deferred.md) merged (4c conditions impl exists)

## Tasks

### T1 — `feat(proto):` add 2 fields to BuildStatus (admin.proto)

MODIFY [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto) — EOF-append to `BuildStatus`:

```protobuf
// Estimated seconds remaining on the critical path (longest chain of
// incomplete derivations weighted by ema_duration_secs).
optional uint64 critical_path_remaining_secs = <next>;
// Worker IDs currently assigned to this build's running derivations.
repeated string assigned_workers = <next>;
```

`admin.proto` count=2 — LOW collision. Parallel with [P0271](plan-0271-cursor-pagination-admin-builds.md) (both EOF-append, merge clean).

### T2 — `feat(controller):` populate from BuildEvent

MODIFY [`rio-controller/src/reconcilers/build.rs`](../../rio-controller/src/reconcilers/build.rs) — compute `critical_path_remaining_secs` from the DAG + ema, collect `assigned_workers` from running derivations.

### T3 — `test(controller):` field population

Unit: mock `BuildEvent` with 2 running + 1 queued → assert `assigned_workers.len() == 2`.

## Exit criteria

- `/nbr .#ci` green

## Tracey

none — proto field + plumbing.

## Files

```json files
[
  {"path": "rio-proto/proto/admin.proto", "action": "MODIFY", "note": "T1: 2 fields EOF-append (low collision)"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "T2: populate (serial after P0238)"}
]
```

```
rio-proto/proto/
└── admin.proto                   # T1: 2 fields
rio-controller/src/reconcilers/
└── build.rs                      # T2: populate (after P0238)
```

## Dependencies

```json deps
{"deps": [245, 238], "soft_deps": [271], "note": "Absorbs 4c A3/P0238 deferral. admin.proto low — parallel with P0271 (both EOF-append). reconcilers/build.rs serial after P0238."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md). [P0238](plan-0238-buildstatus-conditions-fields-deferred.md) — conditions impl exists.
**Conflicts with:** `reconcilers/build.rs` serial after P0238. `admin.proto` parallel-safe with P0271.
