# Phase 2c: Intelligent Storage + Scheduling (Months 9-12)

**Goal:** Chunked CAS deduplication and DAG-aware scheduling.

**Implements:** [rio-store](../components/store.md) chunked CAS, [rio-scheduler](../components/scheduler.md) intelligent scheduling

## Tasks

- [x] `rio-store`: FastCDC chunking, BLAKE3 hashing, S3 chunk backend — `chunker.rs` (C1), `backend/chunk.rs` (C2). 16K/64K/256K min/avg/max.
- [x] Inline fast-path for NARs < 256KB — `metadata.rs` `complete_manifest_inline` (E1), size gate in `grpc.rs` PutPath (C3).
- [x] Write-ahead manifest pattern for durability — `insert_manifest_uploading` → `complete_manifest_*` two-step (E1, C3). Refcount UPSERT before upload.
- [x] FindMissingChunks optimization — `ChunkServiceImpl` (C6). PG-first check, piggybacks `rio_store_chunks_total` gauge.
- [x] Binary cache HTTP server (axum, serves narinfo + NAR from chunks) — `cache_server.rs` (B3). `/nix-cache-info`, `/{hash}.narinfo`, `/nar/{narhash}.nar.zst`.
- [x] Critical-path scheduling with incremental priority maintenance — `critical_path.rs` (D4). Bottom-up compute at merge, ancestor-walk on completion, full sweep every 60s.
- [x] Transfer-cost-weighted closure-locality scoring (bloom filter from heartbeats) — `assignment.rs` `best_worker` (D6). `score = transfer_cost*0.7 + load*0.3`. Bloom from FUSE cache (D1, D2).
- [x] Build duration estimation (historical data with fallback heuristics) — `estimator.rs` (D3). exact (pname,system) → pname cross-system mean → 30s default. Refreshed every 60s from `build_history`.
- [x] Multi-build DAG merging (shared derivations built once) — `dag/mod.rs` `merge()` (existing since Phase 2a). `interest_added` tracking for priority-boost (D5).
- [x] Size-class routing: route derivations to right-sized worker pools based on estimated duration. Operator-configured cutoffs. — `assignment.rs` `classify()` + dispatch overflow chain (D7). See [ADR-015](../decisions/015-size-class-routing.md). **`WorkerPoolSet` CRD deferred to Phase 3a** — cutoffs are static TOML config (`[[size_classes]]` in `/etc/rio/scheduler.toml`).
- [x] `build_history` schema extension: add `ema_peak_memory_bytes`, `ema_peak_cpu_cores`, `ema_output_size_bytes`, `size_class`, `misclassification_count` columns. Workers report peak resource usage in `CompletionReport`. — Migration `006_phase2c.sql` (E1). VmHWM sampling + scheduler EMA write (F1, F2). `ema_peak_cpu_cores` wired in phase3a via cgroup `cpu.stat` 1Hz polling (`rio-worker/src/executor/mod.rs:455`); VmHWM sampling also replaced by cgroup `memory.peak` (fixes the daemon-PID-not-builder bug).
- [x] CA data model in store schema: `realisations` + `content_index` tables (E3, G1). Gateway `wopRegisterDrvOutput` **writes** via `RegisterRealisation` RPC (E4). `wopQueryRealisation` **reads** via `QueryRealisation` RPC. `ContentLookup` finds paths by nar_hash. Enables CA cache hits before Phase 5 early cutoff.
- [x] Integration test: CA data model roundtrip — `rio-gateway/tests/ca_roundtrip.rs` (G2). wopRegisterDrvOutput → wopQueryRealisation → ContentLookup.
- [x] Integration test: chunk deduplication — unit tests in `rio-store/src/cas.rs` + `chunker.rs` (C1-C5). Dedup ratio metric in PutPath. **VM-level dedup validation deferred to Phase 3a with main.rs ChunkBackend wiring.**
- [x] Integration test: scheduling optimizations measurable — `vm-phase2c` (H1): critical-path dispatch order, size-class routing to correct worker pool. Unit tests in `queue.rs` (D5), `assignment.rs` (D6/D7).
- [x] Circuit-breaker on scheduler cache-check failures — `actor/mod.rs` `CacheCheckBreaker` (E5). 5 consecutive failures → open 30s → reject SubmitBuild with UNAVAILABLE. Half-open probe for fast recovery.
- [x] Inline .drv content in DerivationNode — `translate.rs` `filter_and_inline_drv` (D8). FindMissingPaths-gated (only will-dispatch nodes), 64KB/node, 16MB total budget, safe degrade on store error.
- [x] `QueryPathFromHashPart` store RPC — `metadata.rs` `query_by_hash_part` (E2). `WHERE store_path LIKE '/nix/store/' || $1 || '-%'`.
- [x] `AddSignatures` store RPC — `metadata.rs` `append_signatures` (E2). No manifests-join (sigs are narinfo metadata, not upload state).
- [x] Persist `registration_time`/`ultimate` in narinfo table — `metadata.rs` rewrite (E1). Written on PutPath, returned on QueryPathInfo.

## Milestone

Chunk dedup ratio > 30% on nixpkgs rebuild; scheduling latency p99 < 5s.

> **Phase 3a deferrals (library-complete, main.rs wiring pending):**
> - `rio-store/src/main.rs` ChunkBackend construction from config → enables chunk dedup + binary cache HTTP in the running binary. Library (C1-C6, B1-B3) is complete and unit-tested.
> - `WorkerPoolSet` CRD + `CutoffRebalancer` — adaptive size-class cutoffs from `misclassification_count`. Phase 2c uses operator-configured static cutoffs.
> - Closure-size-as-proxy fallback in `Estimator` — needs scheduler-side nar_size tracking.
> - `ema_peak_cpu_cores` EMA write — CPU% needs mid-build polling (VmHWM is one-shot), wired via ProgressUpdate.
