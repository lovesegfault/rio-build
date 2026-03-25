# Plan 0430: ChunkServiceClient zero-callers — decide descope or keep

sprint-1 cleanup finding. [`ChunkServiceClient`](../../rio-proto/src/lib.rs) re-export at `:187` still has zero production callers post-[P0263](plan-0263-worker-upload-findmissing-precheck.md). P0263 scoped down to the zero-proto-change path (worker calls `FindMissingPaths` + streams full NAR, store does server-side chunking). Client-side chunking was the original ChunkService motivation but never landed. Promoted from `TODO(P0430)` at `lib.rs:184`.

**Decision plan** — outcome is either (a) remove the re-export + ChunkServiceClient codegen, or (b) keep and file a concrete plan for the first production caller.

## Entry criteria

- [P0263](plan-0263-worker-upload-findmissing-precheck.md) merged (DONE — it's the plan that descoped client-side chunking)

## Tasks

### T1 — `docs(adr):` investigate — is client-side chunking still on the roadmap?

Check `docs/src/` for any spec text mentioning client-side chunking or `ChunkServiceClient` callers. Check [P0434](plan-0434-manifest-mode-upload-bandwidth-opt.md) (manifest-mode upload) — if it plans to use ChunkServiceClient for the store→ChunkCache fetch path, that's a future caller. Write a short ADR fragment in this plan doc recording the finding.

### T2a — `refactor(proto):` (route-a) remove ChunkServiceClient re-export

If T1 concludes descope: MODIFY [`rio-proto/src/lib.rs`](../../rio-proto/src/lib.rs) — delete the `pub use store::chunk_service_client::ChunkServiceClient;` line at `:187` and the `TODO(P0430)` comment. Check if `ChunkServiceServer` is also unused; if only the server side has callers (store implements it), keep the server re-export, drop only the client.

### T2b — `docs(plan):` (route-b) file concrete first-caller plan

If T1 finds a planned caller: replace `TODO(P0430)` with `TODO(P0NNN)` pointing at that plan, and close this plan as investigation-complete.

## Exit criteria

- `/nixbuild .#ci` green (route-a: removal doesn't break any imports)
- Route-a: `grep ChunkServiceClient rio-*/src/` → 0 hits outside rio-proto codegen
- Route-b: `TODO(P0430)` replaced with a concrete plan reference

## Tracey

No markers — pure housekeeping decision, no spec behavior change.

## Files

```json files
[
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "T2a: delete ChunkServiceClient re-export at :187 OR T2b: update TODO tag"}
]
```

## Dependencies

```json deps
{"deps": [263], "soft_deps": [434], "note": "sprint-1 cleanup finding (discovered_from=rio-store cleanup worker). Hard-dep P0263 (DONE — the descope decision). Soft-dep P0434 (manifest-mode upload — potential future ChunkServiceClient caller; check its design before removing)."}
```

**Depends on:** [P0263](plan-0263-worker-upload-findmissing-precheck.md) — DONE.
**Conflicts with:** none — `lib.rs` re-export section is low-traffic.

## ADR — T1 investigation outcome

**Decision: route-a (remove).** Client-side chunking has no planned caller.

Findings:

- **P0434 is not a ChunkServiceClient caller.** Its manifest-mode design ([T1–T3](plan-0434-manifest-mode-upload-bandwidth-opt.md)) adds `PutPathManifest` to `StoreService`. The worker computes a chunk manifest locally and sends it via `StoreService.PutPathManifest`; the store resolves missing chunks from its own `ChunkCache` server-side. The worker never issues `ChunkService` RPCs. The soft-dep note in P0434's deps ("manifest-mode may BE the first production caller") is a stale hypothesis — the design as written does not bear it out.
- **No spec text** in `docs/src/` describes client-side chunking. [ADR-006](../../docs/src/decisions/006-custom-chunked-cas.md) covers the chunked CAS but is server-side-only — no mention of `ChunkService`, `ChunkServiceClient`, or client-driven chunk upload. The `lib.rs` doc-comment's pointer to ADR-006 ("the CAS design this would slot into") is aspirational, not specified.
- **`PutChunk` is stubbed `UNIMPLEMENTED`** per `docs/src/components/proto.md:49` and `docs/src/phases-archive/phase3b.md:30` ("only needed if client-side chunking lands"). It never landed.
- **Only caller** of the `ChunkServiceClient` re-export is `rio-store/tests/grpc/chunk_service.rs` — the server's own integration tests. Tests can use the deep codegen path `rio_proto::store::chunk_service_client::ChunkServiceClient` directly.
- **`ChunkServiceServer` stays** — `rio-store/src/main.rs:436` registers it; `GetChunk`/`FindMissingChunks` are live server-side for cross-tenant dedup.

The original motivation ("upload only missing chunks instead of whole NARs") is now served by P0434's manifest-mode via `StoreService`, which keeps chunk-boundary knowledge server-side and avoids a second gRPC round-trip per chunk. If a true client-side-chunking path is ever specified, the codegen `store::chunk_service_client::ChunkServiceClient` remains available — only the flattened crate-root re-export is removed.
