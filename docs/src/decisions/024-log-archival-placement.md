# ADR-024: Build-Log Archival Stays in Scheduler

## Status

Rejected (proposal: move S3 log archival behind a `rio-store` `PutLog` RPC)

## Context

`rio-scheduler` writes build logs to S3 directly (`rio-scheduler/src/logs/flush.rs`). Wave 2 of the dependency cleanup unified the S3 client construction into `rio_common::s3::default_client`, but architecturally a "DAG scheduler doing S3 PUTs" still smells: a store-ish responsibility living in a scheduling component. The proposal was to add a `StoreService.PutLog(stream LogChunk) → LogRef` RPC, route the flusher through it, and drop `aws-sdk-s3` from `rio-scheduler`'s dependency tree entirely.

This ADR records why that proposal is **not** net-better and the scheduler keeps direct S3.

### Current shape

- **Write path** (`logs/flush.rs`): `LogFlusher` runs on its own task, fed by the actor via bounded `mpsc::try_send` (never blocks the actor). Two triggers:
  - *Completion* — drain ring buffer → gzip in `spawn_blocking` → `PutObject` → `INSERT INTO build_logs`.
  - *Periodic (30s)* — snapshot every active buffer (non-draining) → gzip → `PutObject` to `logs/periodic/`. No PG row.
- **Read path** (`admin/logs.rs`): `AdminService.GetBuildLogs` checks the in-memory ring buffer first; on miss, looks up `build_logs.s3_key` in PG → `GetObject` → gunzip → stream chunks.
- **Metadata**: the `build_logs` table lives in the **scheduler's** PG schema (`migrations/001_scheduler.sql`), with `build_id UUID REFERENCES builds(build_id)`. One row per `(build_id, drv_hash)`, UPSERTed periodic→final.
- **Volume**: ring capacity is 100k lines × ~100 B ≈ 10 MiB raw / ~1 MiB gzipped per derivation. Periodic flush re-uploads an ever-growing prefix of each active buffer every 30s (`logs/mod.rs:165-170` accepts this as the cost of crash-bounded loss). At ~50 concurrent active derivations that's order-of 50 MiB gzipped per 30s tick.

### What "moving it to store" actually requires

`PutLog` alone is insufficient: the scheduler also **reads** logs via `GetObject` for `GetBuildLogs`. To drop `aws-sdk-s3` the proposal needs both `PutLog` and `GetLog`, plus one of:

- **(a)** Store returns the `s3_key` and scheduler keeps the `build_logs` PG write (extra round-trip, but workable), or
- **(b)** `build_logs` moves to store's schema --- but `build_id` is a scheduler concept that store has no business knowing about, and `GetBuildLogs` would then need a cross-service join.

## Decision

**Keep direct S3 in the scheduler.** Do not add `PutLog`/`GetLog` to `StoreService`.

### Rationale

1. **Build logs are scheduler artifacts, not store artifacts.** rio-store's domain is the content-addressed Nix store: NAR chunks, narinfo, signatures, realisations, GC by reachability. Build logs are `build_id`-addressed (not content-addressed), mutable (periodic overwrites the same key), retained on a build-lifetime TTL (not reachability), unsigned, and not deduplicated. They share nothing with `PutPath` except "bytes go to S3." Adding them to `StoreService` dilutes its single responsibility into "also a generic blob bucket"; `store.proto` opens with "Store service for NAR storage and path metadata" and that should stay true.

2. **The metadata table is FK-coupled to scheduler state.** `build_logs.build_id REFERENCES builds(build_id)` lives in the scheduler schema because the scheduler is the only component that knows what a build is. A `PutLog` RPC either leaves the PG write scheduler-side anyway (so the "scheduler does storage I/O" smell just changes flavour from S3 to PG-about-S3), or drags `build_id` into store's vocabulary.

3. **Latency is not the problem; channel mixing is.** The flush is fully async --- the actor `try_send`s and moves on, so an extra RPC hop costs nothing on the build critical path. But the periodic-snapshot bytes would ride the same scheduler→store gRPC channel that carries `FindMissingPaths` / `BatchQueryPathInfo`, the calls that I-110 identified as the scaling bottleneck under builder fan-out. Best-effort bulk log traffic (~50 MiB/30s, bursty, growing-prefix) competing for h2 stream slots and store CPU with latency-sensitive cache lookups is a regression, not a cleanup. Direct scheduler→S3 keeps that traffic on a separate fault domain.

4. **Wave 2 already removed the real smell.** The original concern was config drift: two components hand-rolling AWS SDK config differently. `rio_common::s3::default_client` fixed that --- one config home, one retry policy, one stalled-stream setting. What remains is "scheduler links `aws-sdk-s3`," which is a binary-size observation, not an architectural one. The crate is already in the workspace lockfile for `rio-store`; dropping it from `rio-scheduler` shrinks neither the build graph nor the dependency-audit surface.

5. **Churn vs. payoff.** The scheduler's S3 surface is ~30 LoC of actual SDK calls (one `PutObject`, one `GetObject`). The proposal touches `store.proto`, `rio-store/src/grpc/`, `flush.rs` (766 LoC, heavily mock-tested via `aws-smithy-mocks`), `admin/logs.rs` + tests (588 LoC), helm IRSA wiring, and the VM tests that exercise the log path end-to-end. That's a lot of motion to relocate two SDK calls to the other side of an RPC.

### If the goal is "scheduler off S3" specifically

The narrower motivation --- one fewer IRSA role, one fewer bucket-policy entry --- is legitimate but better served by a **`rio_common::blob` abstraction** (`put(key, bytes)` / `get(key)` with S3 / local-fs / GCS backends) than by a store RPC. That keeps log storage as the scheduler's own durability concern while letting deployment swap the backend without touching `rio-store`. Not pursued now: the IRSA role already exists, the helm chart already wires `RIO_LOG_S3_BUCKET`, and there is exactly one deployment.

## Consequences

- `rio-scheduler` keeps `aws-sdk-s3` as a direct dependency. The `Cargo.toml` comment ("aws-sdk-s3 stays a direct dep for ByteStream/PutObject types") stands; this ADR is the rationale it can point to.
- `rio_common::s3` remains the single S3-config home for both scheduler and store.
- `StoreService` stays scoped to Nix-store semantics. A future contributor proposing `PutLog` should be pointed here.
- Log-loss-on-crash bound stays ≤30s per `r[obs.log.periodic-flush]`; nothing in this decision changes the durability story.
- Revisit if: (a) store gains a generic blob tier for some other reason, (b) multi-region deployment makes per-component IRSA roles materially expensive, or (c) the periodic-snapshot volume grows enough that scheduler→S3 egress itself becomes a cost line item (at which point the fix is delta-upload, not relocation).
