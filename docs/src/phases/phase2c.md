# Phase 2c: Intelligent Storage + Scheduling (Months 9-12)

**Goal:** Chunked CAS deduplication and DAG-aware scheduling.

**Implements:** [rio-store](../components/store.md) chunked CAS, [rio-scheduler](../components/scheduler.md) intelligent scheduling

## Tasks

- [ ] `rio-store`: FastCDC chunking, BLAKE3 hashing, S3 chunk backend
- [ ] Inline fast-path for NARs < 256KB
- [ ] Write-ahead manifest pattern for durability
- [ ] FindMissingChunks optimization
- [ ] Binary cache HTTP server (axum, serves narinfo + NAR from chunks)
- [ ] Critical-path scheduling with incremental priority maintenance
- [ ] Transfer-cost-weighted closure-locality scoring (bloom filter from heartbeats)
- [ ] Build duration estimation (historical data with fallback heuristics)
- [ ] Multi-build DAG merging (shared derivations built once)
- [ ] `WorkerPoolSet` CRD and size-class routing: route derivations to right-sized worker pools based on estimated duration. Operator-configured cutoffs. See [ADR-015](../decisions/015-size-class-routing.md).
- [ ] `build_history` schema extension: add `ema_peak_memory_bytes`, `ema_peak_cpu_cores`, `ema_output_size_bytes`, `size_class`, `misclassification_count` columns. Workers report peak resource usage in `CompletionReport`.
- [ ] CA data model in store schema --- add PostgreSQL tables for content-indexed lookups and CA output registrations. Gateway implements `wopRegisterDrvOutput` (42) to **write** the `(drv_hash, output_name) -> output_store_path` mapping to the database (not no-op). `wopQueryRealisation` (43) **reads** from this mapping and returns realisation info if found. These enable CA cache hits even before Phase 5 early cutoff activation.
- [ ] Integration test: CA data model validates correctly (write via wopRegisterDrvOutput, read via wopQueryRealisation, verify ContentLookup returns hits for previously-built CA derivations)
- [ ] Integration test: chunk deduplication measured
- [ ] Integration test: scheduling optimizations measurable against FIFO baseline
- [ ] Circuit-breaker on scheduler cache-check failures: sustained store unreachability should skip the TOCTOU cache check and alert, not treat every submission as 100% miss (avalanche of rebuilds)
- [ ] Inline .drv content in SchedulerMessage for small derivations (avoid worker round-trip to store for common case)
- [ ] `QueryPathFromHashPart` store RPC: gateway currently uses FindMissingPaths workaround that returns empty for non-full-path queries
- [ ] `AddSignatures` store RPC: gateway stub currently accepts and discards
- [ ] Persist `registration_time`/`ultimate` in narinfo table: currently dropped on PutPath, returned as 0/false on QueryPathInfo

## Milestone

Chunk dedup ratio > 30% on nixpkgs rebuild; scheduling latency p99 < 5s.
