# Plan 0094: Inline .drv in DerivationNode (FindMissingPaths-gated)

## Design

Gateway inlines `.drv` ATerm content into `DerivationNode` for nodes that will actually dispatch — saves one worker→store `GetPath` round-trip per dispatched derivation. The worker already handled both paths (`executor/mod.rs:241` branches on `is_empty()`); this plan filled in the gateway side that had been sending empty `drv_content` and forcing the worker fetch every time.

**Gated by `FindMissingPaths`:** only nodes with missing outputs get inlined. Cache-hit nodes never dispatch, so inlining their `.drv` would just inflate `SubmitBuild` for no benefit. Difference between "inline everything" (100 MB for a cold 10k DAG) and "inline what's needed" (usually a handful of nodes on the frontier).

**Two caps:** per-node 64 KB (huge flake-env `.drv`s not worth it — worker fetches those), total 16 MB budget (half the gRPC 32 MB limit). Over-budget nodes fall back to worker-fetch. First-come-first-serve order, not critical-path-ordered — simple and correct. The optimization is worth more when it's cheap to compute.

**Safe degrade:** `FindMissingPaths` error/timeout → skip inlining entirely, worker fetches. This is an optimization, not correctness. A store hiccup shouldn't fail `SubmitBuild`; it should just forgo the round-trip saving.

Scheduler stores `drv_content` on `DerivationState`, forwards verbatim in `WorkAssignment` (`dispatch.rs:211` — this resolved the last phase2c-tagged TODO in `.rs` files). gRPC validates ≤256 KB per-node defensively (gateway already capped at 64 KB, but don't trust the wire).

Proto: `DerivationNode.drv_content bytes = 9`.

## Files

```json files
[
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "filter_and_inline_drv: FindMissingPaths gate, 64KB/node + 16MB total caps, safe degrade on store error"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "call filter_and_inline_drv before SubmitBuild"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "DerivationNode.drv_content bytes = 9"},
  {"path": "rio-scheduler/src/state/mod.rs", "action": "MODIFY", "note": "DerivationState.drv_content"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "forward drv_content verbatim in WorkAssignment"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "<=256KB per-node defensive validation"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "oversized reject test"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "drv_content field in dispatch path"},
  {"path": "rio-scheduler/src/critical_path.rs", "action": "MODIFY", "note": "test fixture update for new DerivationState field"},
  {"path": "rio-scheduler/src/dag/tests.rs", "action": "MODIFY", "note": "test fixture update"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "test fixture update"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0090**: `DerivationState` struct shape (this plan adds a field alongside `priority`/`est_duration`).
- Depends on **P0062** (2b actor-split, `288479d`): `actor/dispatch.rs` submodule exists; `WorkAssignment` forwarding.

## Exit

Merged as `9366528` (1 commit). Tests: +4 (inline gates on missing+roundtrip-parses, store-error safe degrade, empty-outputs early return, scheduler oversized reject).

**ZERO `TODO(phase2c)` remaining in `.rs` files after this commit.**
