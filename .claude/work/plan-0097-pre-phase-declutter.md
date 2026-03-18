# Plan 0097: Pre-phase declutter — dead code/deps/migrations + hardening fixes

## Design

Phase 3a opened not with a feat but with a two-day decluttering burst: 19 commits that cleared technical debt accumulated across phases 2a–2c before the kubernetes-operator work began. The deployment wipe at the 2c→3a boundary unblocked several removals that had been held hostage by migration-checksum compatibility.

The anchor is `46028a0`, which deleted the `NarBackend` trait and its three implementations (`s3.rs`, `filesystem.rs`, `memory.rs`) — 616 LOC of whole-NAR storage code rendered dead by phase 2c's chunking work. The trait had been kept only to make the 2c diff additive; with no deployments to preserve, it was removed outright. The adjacent migration squash (`f41133e`) collapsed six migration files into two phase3a baselines: `001_scheduler.sql` and `002_store.sql`. The `nar_blobs` table (created in 002, dropped in 006) disappeared entirely from history — net zero. Verified by byte-identical `pg_dump --schema-only` against the old six-migration chain.

Alongside the deletions, the sweep fixed a cluster of small correctness bugs that had been noted but deferred: `2e6aa60` stopped `inputs Err(_)` from collapsing real gRPC transport errors into fabricated NotFound (a debugging footgun); `2ab2d22` removed the unsafe `/tmp` defaults for the fuse mount point and gateway SSH keys; `0778543` made `cutoff_secs` validation reject NaN/infinity and switched sort comparators to `total_cmp`; `c1d3564` changed the scheduler to reject unclassified workers when size_classes is configured (previously silently accepted); `2fbc9a7` fixed FUSE `getattr` to reply with a fresh error kind after retry instead of echoing the pre-retry error.

The sweep also included a docs-truth pass (`4578d5d`..`7027a0f`): stale in-code comments rewritten, `crate-structure.md` rewritten to match the actual module tree, `//!` crate-level docs added to four `lib.rs` files, and 15 rustdoc warnings fixed with `deny rustdoc warnings` added to the CI clippy check. The workspace dep cleanup (`dfb661c`) dropped `tower-http`, `async-stream`, `hex`, and the redundant sqlx dev-dep; `futures-util` was promoted to a workspace dep to deduplicate version resolution.

## Files

```json files
[
  {"path": "rio-store/src/backend/mod.rs", "action": "MODIFY", "note": "NarBackend trait deleted; chunk.rs comments de-reference dead types"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "backend/base_dir/s3_* config fields removed; discard tuple deleted"},
  {"path": "migrations/001_scheduler.sql", "action": "NEW", "note": "squash of 001+003+004+005+006[ALTER build_history]"},
  {"path": "migrations/002_store.sql", "action": "NEW", "note": "squash of 002+006; nar_blobs never created (net zero)"},
  {"path": "nix/modules/store.nix", "action": "MODIFY", "note": "backend/baseDir options removed"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "MODIFY", "note": "Err(_) → typed match; don't collapse Transport into NotFound"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "fuse_mount_point default removed (was unsafe /tmp)"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "SSH key /tmp defaults removed; required config now"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "cutoff_secs is_finite validation; total_cmp for sort"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "reject unclassified workers when size_classes configured"},
  {"path": "rio-worker/src/fuse/ops.rs", "action": "MODIFY", "note": "getattr replies with fresh error after retry"},
  {"path": "rio-common/src/limits.rs", "action": "MODIFY", "note": "HEARTBEAT shared const added; figment max_leaked_mounts"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "dead WorkAssignment.input_paths field removed; stale phase-2a comments"},
  {"path": "docs/src/crate-structure.md", "action": "MODIFY", "note": "rewritten to match reality"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "wopNarFromPath fix"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "fingerprint/schema corrections"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "tower-http/async-stream/hex removed; futures-util → workspace dep"},
  {"path": "flake.nix", "action": "MODIFY", "note": "deny rustdoc warnings in CI clippy"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). No `r[impl]` or `r[verify]` annotations existed at these commits — tracey was adopted at commit 144 of 197 in this phase. The schema baselines in `migrations/001_scheduler.sql` and `002_store.sql` carry `r[sched.schema.*]` and `r[store.schema.*]` markers, added in the retroactive sweep.

## Entry

- Depends on P0096: phase 2c complete. The `nar_blobs` table and `NarBackend` trait were made dead by 2c's chunking work; the 2c→3a deployment wipe unblocked their removal.

## Exit

Merged as `2e6aa60..2bc7397` (19 commits, non-contiguous — interleaved with P0098-P0105). `.#ci` green at merge. Test count: 812 → 796 (−16 from NarBackend tests deleted). Net −736 LOC from the dead-code removal; `rg 'NarBackend|S3Backend|FilesystemBackend|MemoryBackend'` → 0 hits. `pg_dump --schema-only` byte-identical between old 6-migration chain and new 2-migration chain.
