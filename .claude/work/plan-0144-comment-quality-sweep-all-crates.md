# Plan 0144: Comment quality sweep across all crates

## Design

After four validation rounds, the code was correct but the comments were stale. A doc comment written at P0131 said "HMAC bypass when peer_certs present" — X1 changed it to `CN=rio-gateway` but the comment still said "any mTLS client." A deliberation block in `cas.rs` walked through three approaches to chunk dedup — the decision was made two rounds ago, the block was dead weight. This plan was a deliberate per-crate comment-quality pass: one commit per crate, comments only.

Each commit followed the same pattern: read every `//` and `///` in the crate, delete/fix/collapse anything stale, misleading, or verbose. Specific fixes called out in commit subjects:

- **rio-proto (`102e26e`):** "fix stale HMAC/PutChunk notes" — PutChunk was retagged phase4 but a comment still said "phase3b."
- **rio-test-support (`b1f6f50`):** "fix misleading prefix-scan claim" — comment claimed O(n) scan, code did O(log n) btree range.
- **rio-store (`05676f3`):** "collapse cas.rs deliberation block" — 40-line "should we use X, Y, or Z" comment → 2-line "chose Y because Z."
- **rio-controller (`f2e8162`):** "fix orphaned doc + delete misplaced tracey marker" — a `///` doc comment had drifted away from its function after a refactor. A tracey marker was on a helper, not the semantic implementation site.
- **rio-scheduler (`0d96659`):** "relocate tracey marker + fix stale docs" — `r[impl ctrl.probe.named-service]` moved to the actual probe-construction code.
- **nix (`b7ee76a`):** "delete stale corpus TODO" — fuzz corpus S3 landed in P0136, TODO was resolved.
- **phase3b.nix (`78752e5`):** "rename task-ID identifiers + scrub task-ID comments" — variable names like `c1_setup`, `x9_pins` were meaningless without the validation-round context. Renamed to descriptive: `gc_sweep_setup`, `live_pin_seed`.

No `.rs` logic changed in any of these commits. The tracey marker relocations are net-zero (delete+add same marker elsewhere).

## Files

```json files
[
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-nix/src/protocol/stderr.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-nix/fuzz/fuzz_targets/wire_primitives.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-common/src/bloom.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-common/src/config.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-common/src/grpc.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-common/src/hmac.rs", "action": "MODIFY", "note": "fix stale HMAC bypass note"},
  {"path": "rio-common/src/newtype.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-common/src/task.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-common/src/tls.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-common/tests/tls_integration.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "fix stale PutChunk note"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-proto/src/interceptor.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-proto/tests/contract.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-test-support/src/fixtures.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "fix misleading prefix-scan claim"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "collapse deliberation block"},
  {"path": "rio-store/src/backend/chunk.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/cache_server.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/content_index.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/gc/mod.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/grpc/chunk.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/metadata/chunked.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/metadata/inline.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/metadata/queries.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-store/src/validate.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/config.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/executor/daemon/stderr_loop.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/fuse/ops.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-gateway/src/handler/opcodes_write.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-controller/src/crds/build.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-controller/src/crds/mod.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-controller/src/fixtures.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-controller/src/main.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-controller/src/reconcilers/build.rs", "action": "MODIFY", "note": "fix orphaned doc comment"},
  {"path": "rio-controller/src/reconcilers/mod.rs", "action": "MODIFY", "note": "delete misplaced tracey marker"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-controller/src/reconcilers/workerpool/mod.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-controller/src/scaling.rs", "action": "MODIFY", "note": "comment sweep"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "relocate r[impl ctrl.probe.named-service]"},
  {"path": "nix/modules/worker.nix", "action": "MODIFY", "note": "comment sweep"},
  {"path": "nix/tests/common.nix", "action": "MODIFY", "note": "comment sweep"},
  {"path": "nix/tests/phase3b.nix", "action": "MODIFY", "note": "rename task-ID identifiers (c1_setup → gc_sweep_setup etc)"},
  {"path": "nix/tracey.nix", "action": "MODIFY", "note": "delete stale corpus TODO"},
  {"path": "flake.nix", "action": "MODIFY", "note": "comment sweep"}
]
```

## Tracey

Zero net change. `f2e8162` deletes a misplaced tracey marker from `rio-controller/src/reconcilers/mod.rs`; `0d96659` relocates `r[impl ctrl.probe.named-service]` to the actual probe-construction code. These are RELOCATIONS — the marker count is unchanged, the marker-to-code mapping is more accurate.

## Entry

- Depends on P0141: R4 complete (documenting post-Z state — comments can't lead the code).

## Exit

Merged as `fbac5fc..78752e5` (11 commits). `.#ci` green at merge. Pure doc changes — no `.rs` logic modified.
