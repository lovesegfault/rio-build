# Plan 0376: types.proto domain split — 1034L / 34-collision monolith

**consol-mc150 finding.** [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto) at **1034L / 34 plan-collisions** (3rd-highest after `db.rs` and `main.rs`). [P0248](plan-0248-types-proto-is-ca-field.md) exists specifically as "the ONLY phase5 types.proto touch" because every plan that adds a field to any message in this file creates a rebase-and-re-prove cycle for every in-flight plan that ALSO touches types.proto. The collision count grows per phase.

The file conflates **four domains**:
1. **DAG wire types** — `DerivationNode`, `DerivationEdge`, `DerivationEvent` family (`:12-180`-ish)
2. **Admin/status RPCs** — `GetSizeClassStatusRequest/Response`, `SizeClassStatus`, worker/build listing types
3. **Build lifecycle** — `SubmitBuildRequest`, `BuildResult`, `BuildProgress`, assignment tokens
4. **Store/CA** — `PathInfo`, `Realisation`, narinfo types

Each domain's churn is orthogonal (adding `is_ca` to `DerivationNode` has nothing to do with `SizeClassStatus.sample_count`). Splitting into `dag.proto` / `admin.proto` / `build.proto` / `store_types.proto` (or similar — `admin.proto` already exists, so `admin_types.proto` for its data types) makes each file's collision-set track its domain's churn, not the union.

**This is a large refactor** — tonic/prost regen churns every `use rio_proto::types::*` import across the workspace (~50+ sites). Benefit: phase6+ plans get file-level collision detection that actually tracks semantic overlap.

## Entry criteria

- [P0248](plan-0248-types-proto-is-ca-field.md) merged (last explicitly-serialized types.proto touch completes)
- No in-flight plans have `rio-proto/proto/types.proto` in their Files fence (check via `onibus collisions check` at dispatch — this plan is a proto-reorg, must land in a quiet window)

## Tasks

### T1 — `refactor(proto):` survey + partition table

Survey pass (no code yet): produce a comment-block in `types.proto` at `:1` listing the partition:

```proto
// ---- types.proto partition map (P0376) ----
// dag.proto:        DerivationNode DerivationEdge DerivationEvent*
//                   GraphNode GraphEdge BuildGraph
// admin_types.proto: SizeClassStatus GetSizeClassStatusRequest/Response
//                    WorkerStatus ListWorkersRequest/Response
//                    BuildStatus ListBuildsRequest/Response
//                    TenantInfo ListTenantsRequest/Response
// build.proto:      SubmitBuildRequest BuildResult BuildProgress
//                   AssignmentToken BuildLog*
// store_types.proto: PathInfo Realisation NarInfo ChunkInfo
//                    (or: keep in types.proto as the "shared primitives" home)
```

Cross-check against actual message declarations (`grep '^message ' types.proto`). Leave the block in place for T2-T5 as a guide.

### T2 — `refactor(proto):` extract dag.proto

NEW [`rio-proto/proto/dag.proto`](../../rio-proto/proto/dag.proto). Move `DerivationNode`, `DerivationEdge`, all `DerivationEvent*` oneof variants, `GraphNode`/`GraphEdge`/`BuildGraph` from `types.proto`. Add `import "types.proto";` if any messages reference primitives that stay behind.

MODIFY [`rio-proto/build.rs`](../../rio-proto/build.rs) — add `dag.proto` to the tonic/prost compile list.

MODIFY `types.proto` — delete the moved messages, add forward-reference comment ("DAG types → dag.proto, P0376-T2").

Migrate imports: `grep -rn 'rio_proto::types::Derivation\|rio_proto::types::Graph' rio-*/src/` → replace with `rio_proto::dag::*`. Expect ~15-25 sites (scheduler actor, gateway translate, CLI graph-viz).

### T3 — `refactor(proto):` extract build_types.proto

NEW [`rio-proto/proto/build_types.proto`](../../rio-proto/proto/build_types.proto). Move `SubmitBuildRequest`, `SubmitBuildResponse`, `BuildResult`, `BuildProgress`, `AssignmentToken`, `BuildLogLine` (and siblings) from `types.proto`. Same import/build.rs/migrate flow as T2.

`build.proto` may collide with an existing file — use `build_types.proto` or `submit.proto` if `proto/build.proto` is taken by the protobuf service file.

### T4 — `refactor(proto):` extract admin_types.proto (OR fold into admin.proto)

Check at dispatch: `admin.proto` already defines `service AdminService`. **Option A:** put the data types (`SizeClassStatus`, `WorkerStatus`, `TenantInfo`, `ListXRequest/Response`) in `admin.proto` alongside the service definition — cleaner, one file per service + its types. **Option B:** separate `admin_types.proto` — keeps service defs thin. Prefer A (gRPC idiom).

Migrate imports: `grep -rn 'rio_proto::types::(SizeClassStatus|WorkerStatus|TenantInfo|List)' rio-*/src/` → `rio_proto::admin::*`.

### T5 — `refactor(proto):` types.proto residual — shared primitives only

After T2-T4, `types.proto` should shrink to ~200-300L of genuinely-shared types (`Timestamp` wrappers if any, `PathInfo`/`NarInfo`/`Realisation` if not split to `store_types.proto`). Keep these in `types.proto` as the "common imports" layer — every other `.proto` `import "types.proto";` for primitives.

Delete the T1 partition comment-block (served its purpose).

### T6 — `test(proto):` import-completeness smoke test

NEW test (or extend existing proto-regen test) that asserts each new `.proto` file compiles standalone and the tonic-generated modules export the expected types. Simplest form: a `#[test] fn types_moved()` in [`rio-proto/src/lib.rs`](../../rio-proto/src/lib.rs) that `use`s one type from each new module — compile-fail if the move missed a message.

## Exit criteria

- `/nbr .#ci` green (this is the big gate — tonic regen + workspace-wide import churn)
- `wc -l rio-proto/proto/types.proto` → ≤350 lines (from 1034 — ~66% reduction)
- `grep -c '^message ' rio-proto/proto/dag.proto` → ≥8 (DAG types moved)
- `grep -c '^message ' rio-proto/proto/build_types.proto` → ≥6 (build lifecycle types moved)
- `grep 'rio_proto::types::Derivation\|rio_proto::types::Graph' rio-*/src/ -rn` → 0 hits (imports migrated)
- `grep 'rio_proto::types::SubmitBuild\|rio_proto::types::BuildResult' rio-*/src/ -rn` → 0 hits (imports migrated)
- `onibus collisions top 30 | grep types.proto` → count ≤10 (from 34; the plans touching shared primitives remain)
- `cargo nextest run -p rio-proto` → all pass (T6 smoke + existing proto tests)

## Tracey

No new markers. Pure refactor — no spec-visible behavior change. Existing `r[proto.*]` markers (if any in [`proto.md`](../../docs/src/components/proto.md)) may need `tracey bump` if their text references `types.proto` literally — check at dispatch (`tracey query rule proto.<id>` for each, rewrite to "defined in dag.proto" etc.).

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T1: partition comment-block; T2-T5: delete moved messages, shrink to ≤350L primitives-only"},
  {"path": "rio-proto/proto/dag.proto", "action": "NEW", "note": "T2: DerivationNode/Edge/Event*, GraphNode/Edge, BuildGraph"},
  {"path": "rio-proto/proto/build_types.proto", "action": "NEW", "note": "T3: SubmitBuildRequest/Response, BuildResult, BuildProgress, AssignmentToken, BuildLogLine"},
  {"path": "rio-proto/proto/admin.proto", "action": "MODIFY", "note": "T4-A: fold in SizeClassStatus, WorkerStatus, TenantInfo, ListX* (alongside existing AdminService)"},
  {"path": "rio-proto/build.rs", "action": "MODIFY", "note": "T2+T3: add dag.proto + build_types.proto to tonic compile list"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "T2-T4: pub mod declarations for new modules; T6: types_moved smoke test"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "T2: rio_proto::types::Derivation* → rio_proto::dag::*"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "T2+T3: import migration"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "T3+T4: SubmitBuild*/SizeClassStatus import migration"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "T4: admin types import migration"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "T2: Derivation* import migration"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "T3: BuildResult/BuildProgress import migration"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "T4: SizeClassStatus import migration"},
  {"path": "rio-cli/src/builds.rs", "action": "MODIFY", "note": "T4: ListBuilds* import migration"},
  {"path": "rio-cli/src/workers.rs", "action": "MODIFY", "note": "T4: ListWorkers* import migration"},
  {"path": "rio-cli/src/cutoffs.rs", "action": "MODIFY", "note": "T4: SizeClassStatus import migration (post-P0236)"},
  {"path": "rio-dashboard/src/api/types.ts", "action": "MODIFY", "note": "T2-T4: TS-side regen if protoc-gen-es splits by .proto file (check at dispatch — connect-es v2 may generate one _pb.ts per .proto)"}
]
```

**NOTE:** the `rio-*/src/*.rs` MODIFY entries above are a SAMPLE — actual migration touches every `use rio_proto::types::X` where X moved. At dispatch: `grep -rn 'rio_proto::types::' rio-*/src/ | cut -d: -f1 | sort -u` gives the full set (~20-30 files).

```
rio-proto/proto/
├── types.proto         # T1-T5: shrink to primitives
├── dag.proto           # T2: NEW
├── build_types.proto   # T3: NEW
└── admin.proto         # T4-A: +data types
rio-proto/
├── build.rs            # T2+T3: compile list
└── src/lib.rs          # pub mod + T6 smoke
rio-{scheduler,gateway,controller,cli,dashboard}/src/**
                        # T2-T4: import migrations (~20-30 files)
```

## Dependencies

```json deps
{"deps": [248], "soft_deps": [231, 234, 236, 276, 356], "note": "HARD-dep P0248 (last explicitly-serialized types.proto touch — after it merges, the quiet-window starts). soft_deps are every UNIMPL plan with types.proto in its Files fence — this plan MUST land AFTER them or re-serialize. Check at dispatch: `onibus collisions check 0376` lists the live conflicts. Soft-dep P356 (grpc/mod.rs split — both are large-refactor class; sequence them). discovered_from=consol-mc150 (types.proto 1034L / 34-collision, 3rd-highest). This is a MODE-5-PREDICTOR plan: touches rio-proto → full workspace rebuild → nextest too slow to beat VM fast-fail. Pre-seed the merger with the nextest-standalone clause-4(c) recipe. Also class-3 proto (new .proto files, tonic regen → every downstream crate rebuilds). Implementer: run `.#checks.x86_64-linux.nextest` standalone BEFORE `.#ci` as the load-bearing proof; expect .#ci VM-tier to TCG-fail on fleet roulette but nextest-fresh proves the refactor correct."}
```

**Depends on:** [P0248](plan-0248-types-proto-is-ca-field.md) — last explicit types.proto touch; this split lands after to avoid re-doing the partition.
**Conflicts with:** **HIGH** — [`types.proto`](../../rio-proto/proto/types.proto) count=34. This plan deliberately lands in a quiet window; the Entry criteria says "check `onibus collisions check` at dispatch." Workspace-wide import churn touches ~30 `.rs` files — most are 1-line `use` edits, low semantic-conflict risk but high textual-collision count.
