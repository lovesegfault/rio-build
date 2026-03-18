# Plan Decision Audit — 2026-03-18

## Summary

- **372** decisions found by scanners
- **62** already user-validated (skipped)
- **139** trivial reversibility (skipped — batch-reviewable)
- **171** assessed → **168** unique groups after dedup
- **29** flagged "revisit" → **29** unique after dedup
- **0** split-verdict groups (no `is_sound` conflicts across plans)

| Recommendation | Count | | Blast radius | Count |
|---|---|---|---|---|
| revisit | 29 | | proto-field | 12 |
| confirm | 118 | | migration | 14 |
| already-decided | 19 | | spec-marker | 28 |
| non-decision | 5 | | multi-plan | 35 |
|  |  | | single-plan | 82 |

---

## Revisit queue (ranked)

**29 groups.** Ranked by `max(blast_weight × rec_weight)` across group members. Ties broken by blast weight then alphabetically.

### 1. `client-chunked-upload-finalize-api` — api-shape (proto-field, score=15)

**Plans:** P0263
**The question:** What shape should `client-chunked-upload-finalize-api` take on the wire/API?
**Current default:**
> // 4. PutPath with manifest-only mode (chunk list, not raw NAR). store_client.put_path_manifest(PutPathManifestRequest { store_path, chunk_hashes, nar_hash, ... }).await?;

**Assessor verdict:** **NOT sound** — `PutPathManifest` / `PutPathManifestRequest` do not exist in rio-proto/proto/store.proto or types.proto — this is a brand-new RPC + message type, yet the plan's `files` block lists only rio-worker/src/upload.rs and lib.rs (no proto, no store handler). Additionally: (1) `manifest_data.chunk_list` stores `(blake3_hash, chunk_size)` pairs (migrations/002_store.sql:68-72, rio-store/src/manifest.rs:50-55) but the proposed request sends only `chunk_hashes` — store must either query `chunks` table for sizes or the wire shape is incomplete; (2) `r[store.integrity.verify-on-put]` (docs/src/components/store.md:46-47) requires the store to *independently* compute SHA-256 over the NAR — with manifest-only the store would have to fetch+concat all chunks to verify, or trust the client's nar_hash (spec change), neither addressed; (3) the P0245 spec marker `r[store.chunk.grace-ttl]` already names this the "PutPath manifest" flow, suggesting extension of existing `PutPath` was the envisioned shape, not a new RPC.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Extend PutPathRequest oneof with a 4th variant `ChunkManifest manifest = 4` (repeated {bytes digest, uint32 size}). Stream: metadata → manifest → trailer. Server branches on manifest-vs-nar_chunk after the placeholder insert. | Reuses ALL 688 lines of put_path.rs plumbing (HMAC verify, idempotency fast-path, WAL placeholder, signing, GC advisory lock) with one branch point. Backward-compatible proto (new oneof field = additive). Store still needs to resolve integrity-verify-on-put (fetch missing chunks? trust client?). Slightly branchy handler. |
| No proto change: worker calls FindMissingPaths first (already exists, store.proto:19); if path already complete, skip. Else stream full NAR via existing PutPath — server-side refcount==1 dedup (docs/src/components/store.md:63) skips S3 upload for existing chunks. | Zero proto work, zero store handler work. Idempotency fast-path (r[store.put.idempotent]) already handles the 100%-identical-NAR case with zero bytes transferred. Only loses bandwidth for *partial* overlap (e.g. 70%-shared NAR still streams 100% to store, 30% to S3). Matches the plan's stated exit criterion ('second identical-NAR upload → zero chunks') without any API change. |
| New dedicated unary `rpc PutPathManifest(PutPathManifestRequest)` on StoreService (as planned), duplicating HMAC/placeholder/sign flow. | Clean separation, simple unary call, no stream state machine. But duplicates ~300 lines of put_path.rs auth/WAL/sign flow; adds permanent proto surface; must resolve integrity-verify independently; files list becomes wrong (collision detection breaks for rio-proto and rio-store). |

### 2. `jti-forward-mechanism` — api-shape (proto-field, score=15)

**Plans:** P0245
**The question:** What shape should `jti-forward-mechanism` take on the wire/API?
**Current default:**
> The gateway forwards `jti` in `SubmitBuildRequest.jwt_jti` so the scheduler can check revocation.

**Assessor verdict:** **NOT sound** — Redundant. Per r[gw.jwt.verify] in this same plan, the scheduler's tonic interceptor extracts x-rio-tenant-token, verifies it, and attaches Claims to request extensions. jti is one of those claims. The SubmitBuild handler can read req.extensions().get::<Claims>().jti — no proto body field needed. Adding field 10 to SubmitBuildRequest (types.proto:621-635) duplicates header data and opens a desync vector (body jti ≠ header jti, which one wins?).

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Scheduler reads jti from Claims extension (no proto field) | Zero wire change, single source of truth. The interceptor already does the JWT parse. Strictly better unless there's an unstated persistence need. |
| Keep the proto field but rename intent: jti persisted with build row for audit trail | If the real goal is audit (which jti issued this build?), a body field that gets INSERTed to PG makes sense — but the stated rationale 'so scheduler can check revocation' doesn't require it. |
| Forward full JWT in a proto bytes field | Even more redundant; the header already carries it. |

### 3. `pagination-cursor-encoding` — api-shape (proto-field, score=15)

**Plans:** P0271
**The question:** What shape should `pagination-cursor-encoding` take on the wire/API?
**Current default:**
> optional string cursor = <next>; // opaque — submitted_at timestamp as RFC3339

**Assessor verdict:** **NOT sound** — Three problems: (1) 'opaque' is a lie — RFC3339 leaks the single-column impl, so fixing decision 3's tiebreaker later means a cursor-format break on a shipped proto field. (2) chrono is NOT a dep of rio-scheduler (grep confirms); db.rs:146 deliberately uses EXTRACT(EPOCH)::bigint to avoid it. (3) ListBuildsRequest lives in types.proto:693, not admin.proto — plan's Files manifest is wrong, which matters for P0270 collision coordination.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| base64(version_byte \|\| submitted_at_micros_i64_be \|\| build_id_bytes) | Actually opaque; version prefix lets you change encoding later without breaking old cursors; room for compound tiebreaker from day 1; no chrono needed (PG gives micros via EXTRACT(EPOCH)*1e6). Slightly more code to encode/decode. |
| Epoch-microseconds as stringified i64 | Matches existing TenantRow.created_at pattern at db.rs:146/154 — no new dep, trivial parse. But still single-column (no tiebreaker room) and not truly opaque. |
| RFC3339 as planned + add chrono dep | Human-readable in curl output (nice for admin API debugging). But locks proto to single-column cursor before the tiebreaker question (decision 3) is resolved — fixing it later is a wire break. |

### 4. `chunk-upsert-inserted-fallback` — fallback (migration, score=12)

**Plans:** P0208
**The question:** What's the fallback behavior for `chunk-upsert-inserted-fallback`?
**Current default:**
> **Fallback if xmax semantics are surprising:** add `updated_at TIMESTAMPTZ DEFAULT now()` column via migration 013, upsert bumps it with `ON CONFLICT DO UPDATE SET updated_at = now()`, drain checks `updated_at < $marked_at`. This fallback is a bigger change (migration + drain logic) — only take it if xmax doesn't work.

**Assessor verdict:** **NOT sound** — The fallback as stated does NOT fix the same bug. It changes DRAIN to skip recently-touched chunks — but the bug is at UPLOAD time (cas.rs:215-224): both concurrent PutPaths see refcount≥2, both skip upload, chunk never reaches S3. Drain refusing to delete doesn't help when there's nothing in S3 to preserve. A correct updated_at-based fallback would need a separate `created_at` column and `RETURNING (updated_at = created_at)` to distinguish insert from update — the plan doesn't describe this. Also: migration 013 is wrong; migrations/ contains 001-011, next is 012. Given the simpler `RETURNING (refcount = 1)` alternative (see Decision 1), this fallback should probably be dropped entirely.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| RETURNING (refcount = 1) AS inserted — as the fallback (or primary) | Zero migrations, zero drain changes. Same single-statement shape as xmax. Only 'risk' is that it conflates fresh-insert with resurrection-from-refcount-0, but that conflation is actually the safer answer (upload in both cases). |
| Corrected updated_at fallback: add created_at + updated_at, RETURNING (updated_at = created_at) AS inserted | Actually fixes the upload race (atomic with INSERT like xmax). Needs migration 012 + two columns. Heavier than refcount=1 for no additional benefit. |
| Keep plan's drain-side updated_at check as belt-and-suspenders ONLY, paired with a real upload-side fix | Defense in depth. But drain.rs:111-131 already has still_dead — another layer is diminishing returns. |

### 5. `chunks-tenant-ownership-model` — data-model (migration, score=12)

**Plans:** P0264
**The question:** How should we model `chunks-tenant-ownership-model`?
**Current default:**
> ALTER TABLE chunks ADD COLUMN tenant_id UUID NULL REFERENCES tenants(tenant_id); CREATE INDEX chunks_tenant_idx ON chunks(tenant_id, chunk_hash); ``` NULL allowed for backward-compat (existing chunks have no tenant).

**Assessor verdict:** **NOT sound** — Three concrete defects. (1) BUG: index column is `chunk_hash` but the table PK is `blake3_hash` (migrations/002_store.sql:88) — migration will fail. (2) DATA MODEL: chunks are content-addressed and inserted via `ON CONFLICT (blake3_hash) DO UPDATE` (chunked.rs:117-120). When tenant B uploads bytes identical to tenant A's chunk, the row already exists with tenant_id=A. T3 doesn't specify whether ON CONFLICT overwrites tenant_id (breaks A's visibility) or preserves it (B uploaded but can't see). A single FK column cannot model many-to-many ownership of content-addressed data — `path_tenants` junction (migration 009 Part C) exists as precedent for exactly this. (3) MINOR: missing `ON DELETE SET NULL` (precedent: 009_phase4.sql:33,37) and uses full compound index where precedent uses partial `WHERE tenant_id IS NOT NULL` (009_phase4.sql:41-42).

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Junction table: chunk_tenants(blake3_hash, tenant_id) with PK on both | Correctly models many-to-many; PutChunk does INSERT ON CONFLICT DO NOTHING into junction; FindMissingChunks joins on it. Extra table + join cost, but matches path_tenants precedent and has unambiguous semantics when two tenants upload identical bytes |
| Keep single column but define 'first uploader wins' + ON CONFLICT DO NOTHING on tenant_id | Simple schema. But tenant B uploads bytes, gets told 'missing' next time (chunk is tagged A), re-uploads forever. Defeats dedup for the second tenant entirely. Only viable if cross-tenant byte-collisions are negligible (they're not — glibc, bash, etc. are in every closure) |
| Array column: tenant_ids UUID[] with GIN index | No join, single-row update on conflict. But array-append under concurrent ON CONFLICT is racy without explicit locking; junction table's PK constraint is cleaner |

### 6. `poison-nondistinct-counter` — data-model (migration, score=12)

**Plans:** P0219
**The question:** How should we model `poison-nondistinct-counter`?
**Current default:**
> **`require_distinct_workers = false` semantics:** `failed_workers` stays a `HashSet` (for `best_worker` exclusion) but the poison check becomes a separate counter that increments regardless of contain-check. Simplest: add `failure_count: u32` to `DerivationState`, increment unconditionally in `record_failure_and_check_poison` ... Persist `failure_count` alongside `failed_workers` in PG (minor migration? check if `derivations` table already has a count column — `grep failed_ migrations/` at dispatch).

**Assessor verdict:** **NOT sound** — The plan's own grep-at-dispatch hint has an answer the plan didn't anticipate: `retry_count: u32` already exists (state/derivation.rs:198), already persisted (migrations/001_scheduler.sql:51 `retry_count INT NOT NULL DEFAULT 0`), and already increments unconditionally per retry regardless of worker identity (completion.rs:552). For same-worker repeat failure: HashSet insert is idempotent but retry_count still increments. The proposed `failure_count` field + migration is likely redundant — `retry_count` at check time equals (failures - 1), so `retry_count + 1 >= threshold` works. Only real distinction: retry_count is also compared against `max_retries` (completion.rs:531), so reuse couples two policy knobs.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Reuse existing `retry_count` — no new field, no migration. Check: `(s.retry_count + 1) >= threshold` in non-distinct branch. | Zero schema change. Couples poison-threshold to max_retries semantics (both count the same thing). For single-worker dev (the stated use case, plan:88) this coupling is arguably correct — they ARE the same concern. The off-by-one (+1 because retry_count hasn't incremented yet at check time in record_failure_and_check_poison) needs a comment. |
| In-memory-only `failure_count` (don't persist) | No migration. Scheduler restart loses the count — derivation gets N more retries post-restart. For single-worker DEV deployments (the only stated consumer of require_distinct_workers=false), restart-loses-count is acceptable. Breaks recovery invariant symmetry with failed_workers though. |
| Change `failed_workers: HashSet` → `Vec<WorkerId>` and derive both distinct-count (via dedup) and raw-count (via len) | Single source of truth, no separate counter. But changes the PG type contract — migrations/004_recovery.sql:71 defines `failed_workers TEXT[]` with array_append+ANY-guard idempotence (db.rs:494-495). Dropping the ANY-guard to allow duplicates is a behavior change on a hot path. Most invasive option. |

### 7. `realisation-deps-fk-target` — data-model (migration, score=12)

**Plans:** P0249
**The question:** How should we model `realisation-deps-fk-target`?
**Current default:**
> realisation_id BIGINT NOT NULL REFERENCES realisations(id) ON DELETE CASCADE, dep_realisation_id BIGINT NOT NULL REFERENCES realisations(id) ON DELETE CASCADE, PRIMARY KEY (realisation_id, dep_realisation_id)

**Assessor verdict:** **NOT sound** — realisations(id) DOES NOT EXIST. migrations/002_store.sql:134-143 defines PRIMARY KEY (drv_hash, output_name) with no surrogate id column. All existing code (rio-store/src/realisations.rs:55,57,89) uses the composite natural key. Plan line 54 flags this ('Verify realisations.id column name at dispatch') — verification answer: it's not there. Migration 015 as written will fail sqlx migrate run. CASCADE and composite-PK choices are fine; FK target is the bug.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Prepend ALTER TABLE realisations ADD COLUMN id BIGSERIAL UNIQUE to migration 015 | Junction rows stay compact (16 bytes), cheap joins; but adds a second key to realisations alongside the natural PK, and P0253's wopRegisterDrvOutput handler must now fetch the id after INSERT ... RETURNING (existing insert at realisations.rs:52-70 doesn't return it) |
| Use composite FK: (drv_hash BYTEA, output_name TEXT) × 2 sides, PRIMARY KEY on all 4 columns | No ALTER to realisations, matches exactly how opcodes_read.rs and realisations.rs already identify rows; but junction rows are ~100+ bytes vs 16 bytes, and the 4-column PK index is wider. For low CA-on-CA frequency (P0247 samples this) bloat is likely irrelevant |
| ON DELETE RESTRICT instead of CASCADE | Orthogonal to the FK-target bug; protects against accidental realisation deletes orphaning deps, but GC would need explicit dep-cleanup first. CASCADE matches existing narinfo/manifests pattern at 002_store.sql:56,67 |

### 8. `chunk-upsert-inserted-detect` — data-model (spec-marker, score=9)

**Plans:** P0208
**The question:** How should we model `chunk-upsert-inserted-detect`?
**Current default:**
> The fix: `RETURNING blake3_hash, (xmax = 0) AS inserted` — atomic with the INSERT. Changes the return type `Result<()>` → `Result<HashSet<Vec<u8>>>`, deletes the re-query in cas.rs.

**Assessor verdict:** sound — The xmax=0 idiom is a well-known PostgreSQL trick (stable since 9.5 when ON CONFLICT landed) and the race it fixes is real — cas.rs:215-224 explicitly documents it. However, there is a simpler alternative the plan does not consider: `RETURNING (refcount = 1) AS inserted`. Since chunked.rs:121 does `SET refcount = chunks.refcount + 1` on conflict and inserts with refcount=1, the post-statement refcount equals 1 iff either (a) we freshly inserted, or (b) the chunk was at refcount=0 (deleted=true, awaiting GC drain) and got resurrected. In BOTH cases you want to upload — the S3 object may be gone. xmax reports `inserted=false` for the resurrection case (ON CONFLICT fired), which is LESS safe. The refcount approach avoids reliance on an undocumented implementation detail AND handles resurrection better. Note: store.md:222-231 already commits to xmax in normative text, so this is now a spec-marker change if revisited.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| RETURNING blake3_hash, (refcount = 1) AS inserted | Simpler — relies only on documented column semantics, not xmax implementation detail. Correctly reports 'upload needed' for the refcount=0 resurrection case (chunked.rs:109-114) where xmax would say 'skip'. Downside: requires the T0 spike to verify RETURNING sees post-UPDATE refcount (it does — RETURNING is defined to see the final row state), and the spec marker at store.md:222 would need rewording. |
| INSERT ... ON CONFLICT DO NOTHING RETURNING blake3_hash, then separate UPDATE for refcount bumps | DO NOTHING + RETURNING only returns inserted rows — exactly the set you want, no filtering. But requires a second statement in the same tx to bump refcount on pre-existing chunks. Two PG roundtrips instead of one; loses the single-statement atomicity the comment at chunked.rs:105-107 praises. |
| xmax = 0 (plan's primary) | Well-trodden idiom, one statement. Relies on undocumented-but-stable PG internals. Reports inserted=false for refcount=0 resurrection, which could skip upload of a chunk GC already deleted from S3 — the belt-and-suspenders still_dead check at drain.rs:111-131 narrows but doesn't close that window. |

### 9. `jwt-interceptor-service-scope` — scope-boundary (spec-marker, score=9)

**Plans:** P0259
**The question:** Where's the scope boundary for `jwt-interceptor-service-scope`?
**Current default:**
> tonic interceptor: extract `x-rio-tenant-token`, verify signature+expiry, attach `Claims` to request extensions. **Wired in THREE `main.rs` files** — scheduler, store, controller. Per R11: easy to miss one, creating an unauth'd backdoor.

**Assessor verdict:** **NOT sound** — rio-controller/src/main.rs has NO tonic::transport::Server — grep shows 0 matches (vs 5 in scheduler, 3 in store). It's a kube-runtime reconcile loop (main.rs:3-9, 16) with only a raw-TCP /healthz server (main.rs:381) and no gRPC ingress to protect. Tonic is a dep only as a gRPC client (scheduler_addr/store_addr config). The exit criterion `wc -l → 3` is unachievable. Error originates upstream in P0245:91 which seeds r[gw.jwt.verify] spec text naming 'controller' — that spec text is wrong and P0259 inherited it.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Scope to 2 services (scheduler + store); amend P0245's r[gw.jwt.verify] spec text to drop 'controller'; exit criterion wc -l → 2 | Requires tracey bump on r[gw.jwt.verify] since spec text changes. If P0245 not yet merged, fix there instead. Cleanest: controller has no tenant-facing ingress, only k8s-probed /healthz. |
| Add client-side token-attach interceptor for controller's outgoing gRPC calls (controller→scheduler, controller→store) | Different direction (attach not verify) and different concern — controller runs in-cluster with ServiceAccount creds, typically trusts mTLS or network-policy, not tenant JWT. Scope creep beyond 'verify middleware'. |
| Keep '3' aspirationally, add a gRPC admin endpoint to controller that needs protection | Inventing work to satisfy a miscounted exit criterion. Controller's admin surface is the k8s API (CRD status), not gRPC — no organic need. |

### 10. `k8s-finalizer-naming-convention` — api-shape (spec-marker, score=9)

**Plans:** P0233
**The question:** What shape should `k8s-finalizer-naming-convention` take on the wire/API?
**Current default:**
> pub const MANAGER: &str = "rio-controller-wps"; pub const FINALIZER: &str = "workerpoolset.rio.build/finalizer";

**Assessor verdict:** **NOT sound** — MANAGER is sound — follows `rio-controller[-suffix]` pattern (build.rs:54 + workerpool/mod.rs:54 use `rio-controller`; scaling.rs:338 uses `rio-controller-autoscaler`; P0234 uses `rio-controller-wps-{status,autoscaler}`). Distinct manager from bare `rio-controller` correctly disambiguates managedFields when WPS-created WorkerPools appear alongside user-created ones. FINALIZER is functionally valid but **forks the naming convention**: both existing finalizers are `rio.build/workerpool-drain` (workerpool/mod.rs:48) and `rio.build/build-cleanup` (build.rs:53) — pattern `rio.build/<resource>-<purpose>`. Proposed `workerpoolset.rio.build/finalizer` uses Kubebuilder's `<kind>.<group>/finalizer` style. WPS CRD group is `rio.build` (plan-0232:45), so `rio.build/workerpoolset-cleanup` would match existing convention. The spec marker `r[ctrl.wps.reconcile]` hardcodes MANAGER verbatim (plan:160,185), so changing MANAGER post-merge = tracey bump.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| FINALIZER = "rio.build/workerpoolset-cleanup" | consistent with the 2 existing finalizers (grep `rio.build/` finds all 3); slightly less Kubebuilder-idiomatic, but this codebase isn't using Kubebuilder codegen anyway |
| FINALIZER = "rio.build/wps-cleanup" | shortest, matches the MANAGER's `-wps` abbreviation; risk that `wps` is opaque to an operator reading `kubectl get workerpoolset -o yaml \| grep finalizers` |
| keep as proposed + retrofit existing 2 finalizers to the new style | live-object migration needed (finalizers are stored in etcd on every object with that CRD; renaming requires add-new+remove-old on each); not worth it for cosmetics |

### 11. `rate-key-spec-marker-update` — assumption (spec-marker, score=9)

**Plans:** P0261
**The question:** Is the `rate-key-spec-marker-update` assumption valid?
**Current default:**
> none — this is internal resilience. `r[gw.rate.per-tenant]` exists at gateway.md:657 but this plan doesn't change its contract (rate-limiting still happens); the key change is an implementation detail.

**Assessor verdict:** **NOT sound** — Verified r[gw.rate.per-tenant] at gateway.md:657, but the normative text under it (lines 659-666) says 'keyed on tenant_name (from authorized_keys comment)' and 'No key-eviction while key = tenant_name'. P0261 changes BOTH facts. The spec already anticipates jti at line 667-668 but describes it as future-tense; after P0261 merges, the present-tense text is wrong. CLAUDE.md: update design doc in same commit + tracey bump when spec text changes meaningfully.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Update gateway.md:659-668 text to describe jti keying + LRU as current behavior, run `tracey bump` → r[gw.rate.per-tenant+2] | Marker ID 'per-tenant' becomes a misnomer but IDs are opaque to tracey. P0213's r[impl] annotation (if it lands with v1) goes stale and must be re-bumped — correct behavior, forces review. |
| Rename marker to r[gw.rate.per-session] (or r[gw.rate.keyed]), update text, re-annotate impl | Semantically cleanest. But if P0213 already landed an r[impl gw.rate.per-tenant] annotation, that becomes a broken ref (tracey-validate fails) until re-annotated — forces coordination with P0213's merged code. |
| Leave spec as-is (plan's current choice) | Spec text says 'keyed on tenant_name' while code keys on jti — exactly the drift CLAUDE.md's 'Keeping docs and code in sync' section prohibits. Future readers of the spec get wrong info. |

### 12. `seccomp-default-action-layering` — config-default (spec-marker, score=9)

**Plans:** P0223
**The question:** What's the right default for `seccomp-default-action-layering`?
**Current default:**
> "defaultAction": "SCMP_ACT_ALLOW" ... This is a **denylist on top of RuntimeDefault** — `defaultAction: ALLOW` because we layer on what RuntimeDefault already blocks.

**Assessor verdict:** **NOT sound** — Architecturally wrong: Kubernetes `type: Localhost` REPLACES RuntimeDefault (kubelet passes one profile to OCI runtime, no stacking). With defaultAction:ALLOW + 5 denials, the profile re-enables ~40 syscalls RuntimeDefault blocks (kexec_load, open_by_handle_at, userfaultfd) — net security regression. Also contradicts ADR-012:14 which explicitly specifies allowlist ('allows the specific syscalls needed... while blocking everything else'). The false 'on top of' claim is already encoded in spec marker security.md:56.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Clone containerd's RuntimeDefault JSON (~350 syscalls allowlist, defaultAction:ERRNO) and append the 5 extra denials as ERRNO entries | Correct 'on top of' semantics; ~350 lines but zero maintenance since it's copy-paste from upstream; version-pin risk if containerd changes its default |
| defaultAction:SCMP_ACT_ERRNO + hand-curated allowlist per ADR-012:14 (mount, unshare, pivot_root, clone, +base set) | Tightest security posture and matches the ADR; high risk of EPERM crashes on missed syscalls (glibc internals, futex variants); needs extensive build-corpus testing before prod |
| Keep defaultAction:ALLOW but rewrite spec to say 'trades ~40 RuntimeDefault blocks for 5 targeted blocks' | Honest but indefensible — kexec_load/open_by_handle_at are higher-value attacker targets than ptrace; only viable if someone proves the 40 are all CAP-gated anyway under CAP_SYS_ADMIN (some are) |

### 13. `store-gc-sweep-metric-names` — api-shape (spec-marker, score=9)

**Plans:** P0212
**The question:** What shape should `store-gc-sweep-metric-names` take on the wire/API?
**Current default:**
> metrics::counter!("rio_store_gc_paths_swept_total").increment(stats.paths_deleted as u64); metrics::counter!("rio_store_gc_chunks_enqueued_total").increment(stats.chunks_enqueued as u64);

**Assessor verdict:** **NOT sound** — Two bugs. (1) Field doesn't exist: rio-store/src/gc/mod.rs:79-94 GcStats has s3_keys_enqueued (line 85) and chunks_deleted (line 83), NOT chunks_enqueued — won't compile. (2) Naming inconsistency: existing GC metrics at observability.md:134-135 use SINGULAR nouns (rio_store_gc_path_resurrected_total, rio_store_gc_chunk_resurrected_total); plan uses PLURAL (paths_swept, chunks_enqueued). Also neither metric is added to observability.md (plan only registers in lib.rs) — CLAUDE.md says metric names must match observability.md.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| rio_store_gc_path_swept_total ← stats.paths_deleted; rio_store_gc_s3_key_enqueued_total ← stats.s3_keys_enqueued (singular, correct fields) | Consistent with existing path_resurrected/chunk_resurrected naming; uses actual GcStats field names; 's3_key' is precise about what's being counted |
| rio_store_gc_path_deleted_total (mirror the field name paths_deleted exactly instead of 'swept') | 'swept' is GC jargon, 'deleted' is what actually happened to narinfo rows; easier to correlate with log lines that say 'paths deleted' |
| Emit all four non-resurrected GcStats fields (paths_deleted, chunks_deleted, s3_keys_enqueued, bytes_freed) as counters | Complete observability of GC output; bytes_freed especially useful for capacity dashboards. Four describe_counter! calls instead of two. |

### 14. `wps-autoscale-target-source` — threshold (spec-marker, score=9)

**Plans:** P0234
**The question:** What's the right threshold/limit for `wps-autoscale-target-source`?
**Current default:**
> compute `desired = clamp(queued / target_queue_per_replica, min_replicas, max_replicas)` — autoscale formula is queue-depth-proportional (target_queue_per_replica is referenced but not defined in this plan).

**Assessor verdict:** **NOT sound** — The formula matches compute_desired at scaling.rs:574 (`queued.div_ceil(target)` clamped), which is proven. But `target_queue_per_replica` has NO source: P0232's SizeClassSpec defines only {name, cutoff_secs, min_replicas, max_replicas} — no target field. The existing WorkerPool autoscaler reads `pool.spec.autoscaling.target_value` (scaling.rs:295). Plan's T2 code at line 82-86 calls compute_desired with 3 args; real signature takes 4 — won't compile. The r[ctrl.wps.autoscale] spec text (line 148) bakes in a field that doesn't exist.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Add `target_queue_per_replica: Option<i32>` (default 5) to SizeClassSpec in P0232 | Clean per-class tuning (small builds may want 10/replica, large builds 2/replica). Requires P0232 CRD change before this plan — but P0232 is already upstream of P0233 upstream of P0234, so it's in the dep chain. CRD regen happens in P0235 anyway. |
| Read target from the child WorkerPool's `spec.autoscaling.target_value` (set by P0233's build_child_workerpool from template.autoscaling) | Keeps SizeClassSpec lean; indirects through the child. Requires an extra GET per class (or list). P0233's child builder would need to populate autoscaling from PoolTemplate. |
| Single WPS-level `spec.autoscaling.target_value` shared across all classes | Simplest CRD shape. Loses per-class tuning — the whole point of size classes is that small vs large builds have different characteristics. Probably wrong for SITA-E. |

### 15. `build-condition-event-mapping` — data-model (multi-plan, score=6)

**Plans:** P0238
**The question:** How should we model `build-condition-event-mapping`?
**Current default:**
> BuildEvent::Scheduled { .. } => vec![build_condition("Scheduled", "True", "Scheduled", "build assigned to scheduler")], BuildEvent::InputsResolved { .. } => vec![build_condition("InputsResolved", "True", "InputsAvailable", "all input paths substituted")], BuildEvent::Building { .. } => vec![build_condition("Building", "True", "WorkerAssigned", "derivation dispatched to worker")]

**Assessor verdict:** **NOT sound** — The proto BuildEvent oneof (rio-proto/proto/types.proto:70-78) has Started/Progress/Log/Derivation/Completed/Failed/Cancelled — NO Scheduled/InputsResolved/Building variants exist. InputsResolved appears nowhere in proto or scheduler code (only in controller.md spec + crds/build.rs doc comment). Scheduled maps cleanly to BuildEv::Started; Building can be derived from Progress.running>0 or first DerivationEvent::Started; but InputsResolved has NO source signal. Either proto needs a new event (wire compat blast radius) or InputsResolved must be derived heuristically or dropped.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Map BuildEv::Started→Scheduled, first Progress with running>0→Building, DROP InputsResolved (spec update needed at controller.md:41) | Loses one of three spec'd conditions but avoids proto changes. InputsResolved is the least actionable of the three for operators anyway. |
| Add BuildEvent::InputsResolved to types.proto — scheduler fires after closure substitution completes | Proto-field blast radius: scheduler must be updated to emit it, wire compat concern, scope bleed into rio-scheduler. Proper fix but this plan becomes multi-crate. |
| Derive InputsResolved from Progress.queued==0 && completed==0 heuristic | Fragile — queued may drop to 0 for reasons other than input resolution; a single-derivation build never has queued>0. Condition would be misleading. |

### 16. `buildstatus-critpath-workers-shape` — api-shape (multi-plan, score=6)

**Plans:** P0270
**The question:** What shape should `buildstatus-critpath-workers-shape` take on the wire/API?
**Current default:**
> // Estimated seconds remaining on the critical path (longest chain of incomplete derivations weighted by ema_duration_secs). optional uint64 critical_path_remaining_secs = <next>; // Worker IDs currently assigned to this build's running derivations. repeated string assigned_workers = <next>;

**Assessor verdict:** **NOT sound** — File-path error: admin.proto (37 lines) contains ONLY a service definition — zero messages. Proto `BuildStatus` lives in types.proto:416. But P0238 (this plan's predecessor) targets the CRD struct in crds/build.rs, and build.rs:977 apply_event writes to that CRD struct — so the intended target is almost certainly the Rust CRD, not proto. If so, `uint64` is wrong: crds/build.rs:140-149 documents k8s JSON schema can't do uint64 natively, existing `last_sequence` uses i64. Also needs `skip_serializing_if` for SSA-stomp protection (crds/build.rs:92-106 documents why). The Files block's admin.proto collision analysis with P0271 is therefore based on a phantom file intersection. Field shapes themselves are reasonable: seconds-as-int matches CRD siblings; `string` for worker_id matches types.proto:121,160,350,680,807 uniformly. `ema_duration_secs` reference is real (scheduler/src/estimator.rs:37, db.rs:77).

**Alternatives:**

| Option | Tradeoff |
|---|---|
| CRD struct fields: `Option<i64> critical_path_remaining_secs` + `Vec<String> assigned_workers` in crds/build.rs, both with skip_serializing_if | Most consistent with P0238 context and controller's apply_event flow; loses the P0271 parallel-safe story but crds/build.rs isn't in P0271's file set anyway |
| types.proto BuildStatus (QueryBuildStatus RPC) + CRD mirror | Dashboard could also read via gRPC-Web (admin.proto:9 mentions tonic-web), but doubles the field surface and controller would need to populate both |
| Structured assignments: `Vec<WorkerAssignment { worker_id, drv_path }>` instead of bare string list, with dedup-by-worker semantics specified | Richer for debugging 'which worker is building what', but heavier CRD status churn on every DerivationStarted/Completed event → more SSA patch traffic |

### 17. `ca-cutoff-multi-output-granularity` — data-model (multi-plan, score=6)

**Plans:** P0251
**The question:** How should we model `ca-cutoff-multi-output-granularity`?
**Current default:**
> // MVP: single bool. Multi-output CA is rare; per-output map is a followup if the VM test (P0254) shows it matters. state.ca_output_unchanged = matched;

**Assessor verdict:** **NOT sound** — Single bool as a data model is defensible (FODs dominate real CA usage; per-output granularity requires P0252's find_cutoff_eligible to track per-edge output-name deps — premature). But the sketch's loop semantics are wrong: `for output { state.ca_output_unchanged = matched; }` OVERWRITES each iteration — only the LAST output's result survives. For outputs=[miss, match] → final bool=true → P0252 skips downstream → wrong (output[0] is new content). The correct single-bool MVP is AND-fold: `state.ca_output_unchanged = all_outputs_matched` — same one-bool model, same LOC, strictly correct for multi-output. 'Rare' doesn't excuse 'silently wrong when it happens'; rio-nix/derivation/mod.rs:222-226 uses .any() over outputs precisely because multi-output CA is structurally supported.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Single bool with AND-fold: compute all queries, then `state.ca_output_unchanged = results.iter().all(\|r\| *r)` | Same data model (one bool), same field, zero extra complexity, correct for all output counts. Cutoff fires only when EVERY output is unchanged — conservative and correct. This should be the default; it's what 'MVP single bool' should mean. |
| HashMap<OutputName, bool> on DerivationState | Correct and maximally granular, but P0252's find_cutoff_eligible would need to know which output each downstream edge depends on (A→B.dev vs A→B.out). The dag currently tracks drv-level edges, not output-level. Real work; defer until P0254 shows single-bool AND-fold leaves measurable cutoff on the table. |
| Short-circuit on first miss: break out of loop on !matched, set false | Correct and saves RPCs on first miss. Loses per-output metric granularity (can't distinguish 'all miss' from 'first miss') but that's minor. Equivalent safety to AND-fold. |

### 18. `crd-k8s-type-passthrough` — data-model (multi-plan, score=6)

**Plans:** P0232
**The question:** How should we model `crd-k8s-type-passthrough`?
**Current default:**
> #[schemars(schema_with = "any_object")] // passthrough: k8s ResourceRequirements pub resources: serde_json::Value, — avoids duplicating k8s type schemas.

**Assessor verdict:** **NOT sound** — Diverges from the established pattern and creates a downstream type mismatch. workerpool.rs:70 uses `Option<ResourceRequirements>` + `schema_with = any_object` — the schema_with passthrough works regardless of Rust type, so strongly-typed + passthrough gives you both type safety AND schema correctness. The plan's rationale conflates 'k8s-openapi lacks JsonSchema impl' (true, solved by schema_with) with 'must use serde_json::Value' (false). Critically: P0233:38 does `resources: Some(class.resources.clone())` to populate `WorkerPoolSpec.resources: Option<ResourceRequirements>` — serde_json::Value won't unify, forcing a runtime `from_value()` that can fail.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| pub resources: ResourceRequirements + schema_with=any_object (non-Option) | Matches workerpool.rs pattern for schema, keeps Rust type safety, P0233 builder works with `Some(class.resources.clone())`. Making it required (non-Option) is reasonable — the whole point of size classes is distinct resource profiles. |
| pub resources: serde_json::Value (plan's choice) | Schema output is identical (any_object controls that), but loses Rust-side type safety and forces P0233 to do `serde_json::from_value::<ResourceRequirements>(class.resources)` with runtime failure mode. Zero upside over the typed approach. |
| pub resources: Option<ResourceRequirements> + schema_with=any_object | Exact copy of workerpool.rs:70. Allows omitting resources per-class (cluster default); but makes 'size class with no resource distinction' representable, which may be a footgun. |

### 19. `decision-gate-outcome-channel` — scope-boundary (multi-plan, score=6)

**Plans:** P0241
**The question:** Where's the scope boundary for `decision-gate-outcome-channel`?
**Current default:**
> **Hidden check at dispatch:** `jq 'select(.origin=="P0220")' .claude/state/followups-pending.jsonl` — read the recorded outcome. If `needs-calico`: stop, re-estimate, possibly promote a Calico-preload plan first.

**Assessor verdict:** **NOT sound** — POLICY is sound (stop+re-estimate on balloon scope is correct risk-gating). MECHANISM is broken: state.py:145-147 defines origin as closed Literal[reviewer|consolidator|bughunter|coverage|inline|coordinator] — 'P0220' is not a member; state.py:514 sets origin='reviewer' when positional is P<N>. The jq never matches. Additionally Followup has no `payload` field (state.py:153-187) and pydantic v2 silently drops extras, so `payload.netpol` is lost on write. Failure is SILENT: write succeeds, read returns empty, implementer falls through to kube-router-ok default. P0220 (line 64-66, 77) shares the same stale schema assumption — both plans need fixing.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Fix jq to `select(.source_plan=="P0220")` and encode outcome in `description` string (e.g., 'NetPol: needs-calico') | Zero schema change, works today. Stringly-typed: fragile to description rewording, no structured field to assert on. Quick fix, not durable if more decision-gate plans emerge. |
| Add `payload: dict[str, Any] \| None = None` to Followup model in state.py | One-line schema change makes Followup a proper decision-carrier for ALL future gate plans. Touches state.py (high-traffic file per collisions matrix). Cleanest long-term; requires fixing both P0220's write and P0241's read. |
| Skip followups sink — P0220 already annotates docs/src/phases/phase4c.md:41 with outcome; P0241 dispatch-check greps that line | No JSONL, no schema, no state.py edit. But doc-grep is fragile to rewording and loses the structured record. Coordinator already has to read phase4c.md anyway for the annotation; dual-channel (doc + jsonl) was redundant. |

### 20. `grpc-web-transport-flavor` — config-default (multi-plan, score=6)

**Plans:** P0277
**The question:** What's the right default for `grpc-web-transport-flavor`?
**Current default:**
> export const transport = createConnectTransport({ baseUrl: "/", useBinaryFormat: true });

**Assessor verdict:** **NOT sound** — Protocol mismatch. P0273:52 configures Envoy with envoy.filters.http.grpc_web, validated at P0273:98 with content-type: application/grpc-web+proto. createConnectTransport speaks the Connect protocol (application/proto), which Envoy's grpc_web filter does NOT translate — requests pass through un-transformed and hit the scheduler as malformed HTTP/1.1. Correct: createGrpcWebTransport from the same package. With that fix, useBinaryFormat:true is right (matches P0273's curl test, supports server-streaming via fetch ReadableStream for P0279).

**Alternatives:**

| Option | Tradeoff |
|---|---|
| createGrpcWebTransport({ baseUrl: "/", useBinaryFormat: true }) | Correct — matches Envoy grpc_web filter and P0273's application/grpc-web+proto curl validation. Binary framing works for both unary and server-streaming in modern browsers. |
| createGrpcWebTransport({ baseUrl: "/", useBinaryFormat: false }) | grpc-web-text (base64). ~33% payload bloat. Only needed for browsers lacking fetch body streaming — not a constraint here. |
| Keep createConnectTransport, add Connect-protocol termination in Envoy | Not possible — Envoy has no Connect-protocol filter. Would require replacing Envoy with a Connect-native proxy (buf's connect-go gateway), undoing all of P0273. |

### 21. `jwt-pubkey-hotswap-shape` — api-shape (multi-plan, score=6)

**Plans:** P0260
**The question:** What shape should `jwt-pubkey-hotswap-shape` take on the wire/API?
**Current default:**
> extend the existing `shutdown_signal` pattern with a SIGHUP handler that re-reads the pubkey ConfigMap mount. The interceptor from P0259 reads from an `Arc<RwLock<VerifyingKey>>` so reload is a write-lock swap.

**Assessor verdict:** **NOT sound** — The SIGHUP mechanism is spec-mandated (docs/src/multi-tenancy.md:31, docs/src/security.md:126) and extending the shutdown_signal pattern is consistent (rio-common/src/signal.rs:35-58 uses the same tokio::signal::unix infra). Arc<RwLock<VerifyingKey>> is defensible: zero new deps, ed25519 verify dominates read-lock cost, annual rotation means zero write contention. BUT the premise is factually wrong: P0259's plan (plan-0259-jwt-verify-middleware.md:23) specifies a bare `pubkey: ed25519_dalek::VerifyingKey` moved by-value into the closure — no Arc, no RwLock, no hot-swap affordance. P0260's Files block doesn't list rio-common/src/jwt_interceptor.rs as MODIFY, so the implementer hits a type mismatch at the P0259→P0260 boundary.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Amend P0259 pre-merge to take Arc<RwLock<VerifyingKey>> from the start | Cleanest — avoids churning just-landed code. P0259 is still UNIMPL (hard dep of P0260), so the signature is still malleable. Cost: touches 3× main.rs wiring in P0259 to wrap the key. |
| tokio::sync::watch::Receiver<VerifyingKey> instead of Arc<RwLock> | Zero new deps (tokio already present). receiver.borrow() in interceptor, SIGHUP handler calls tx.send(). Slightly more idiomatic with tokio's async model than a sync RwLock. VerifyingKey is 32 bytes so clone-on-borrow is cheap. Still requires the same P0259 signature coordination. |
| arc-swap::ArcSwap<VerifyingKey> | Lock-free reads, the canonical crate for read-heavy/write-rare config reload. New dep (tiny, well-audited). Overkill for annual rotation — RwLock read contention is a non-issue at this write frequency. |

### 22. `sita-degenerate-cutoffs` — fallback (multi-plan, score=6)

**Plans:** P0229
**The question:** What's the fallback behavior for `sita-degenerate-cutoffs`?
**Current default:**
> All durations identical → cutoffs degenerate (all boundaries at the same value). Handle: dedupe adjacent equal cutoffs OR clamp to `[prev-ε, prev+ε]` range.

**Assessor verdict:** **NOT sound** — False dichotomy — both stated options are wrong. Dedupe is a correctness bug: P0230:49 applies via `guard.iter_mut().zip(&result.new_cutoffs)`, and zip silently truncates to the shorter iterator, so a deduped vec leaves some classes with stale cutoffs. Clamp has no `prev` on first run (plan:76 skips smoothing when prev_cutoffs.len() != raw_cutoffs.len()). The test at plan:184-187 already enshrines the correct third option — no-op pass-through — and assignment.rs:102-130 already tolerates equal cutoffs via stable sort + first-match.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| No-op: return equal cutoffs unchanged (what the test already asserts) | Preserves Vec length invariant so P0230's zip works. classify() routes everything to first-matching class, others idle — semantically correct for uniform workload. Zero added code. Only downside: gauge shows 1.0/0.0/0.0 load split, which is accurate but may alarm operators who don't realize their workload is degenerate. |
| Dedupe adjacent equal cutoffs | BREAKS P0230. zip truncates silently — if N-1 cutoffs dedupe to 1, only class[0] gets updated, classes[1..N-2] keep stale values. Would also need classify() to handle variable class count mid-run. Don't do this. |
| Clamp to [prev-ε, prev+ε] | Preserves length, but meaningless on first run (no prev — plan:76 returns raw unsmoothed). On subsequent runs, clamps against already-clamped values, creating artificial ε-separation that has no basis in the workload. Adds code for a cosmetic fix. |

### 23. `wps-classstatus-fields` — data-model (multi-plan, score=6)

**Plans:** P0232
**The question:** How should we model `wps-classstatus-fields`?
**Current default:**
> pub struct ClassStatus { pub name: String, pub effective_cutoff_secs: f64, pub queued: u64, pub child_pool: String, pub replicas: i32 } — status subresource shape (no running, no sample_count — subset of GetSizeClassStatus).

**Assessor verdict:** **NOT sound** — Missing `ready_replicas` vs phase4c.md:25 spec: `ClassStatus { name, effective_cutoff_secs, replicas, ready_replicas, queued_derivations }`. The plan has `child_pool` (good addition — P0237 CLI uses it for discoverability) but dropped `ready_replicas`. WorkerPool's own printcolumns (workerpool.rs:39-40) show Ready/Desired as the primary operator signal — operators expect the same at the WPS level. Omitting `running`/`sample_count` from the RPC is a fine call (status ≠ full RPC echo), but `ready_replicas` is spec-mandated and operationally useful. P0234:43 has `replicas: 0 // filled below from child WorkerPool status` — fetching ready_replicas from the same source is trivial.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Add ready_replicas: i32 (6 fields total) | Matches spec + keeps child_pool. P0234 fetches it from child WorkerPool.status alongside replicas — ~2 extra lines. Best of both. |
| Plan's 5 fields as-is | Spec-divergent. `kubectl describe wps` can't show Ready-vs-Desired without a separate `kubectl get wp`. Status subresource fields are additive in v1alpha1 so adding later is non-breaking, but cheaper to include now than to re-touch P0234. |
| Spec's 5 fields verbatim (drop child_pool) | Loses the WPS→WorkerPool name link. P0237 CLI would have to recompute `format!("{}-{}", ...)` client-side. child_pool is a good addition, keep it. |

### 24. `ca-skipped-source-states` — data-model (single-plan, score=3)

**Plans:** P0252
**The question:** How should we model `ca-skipped-source-states`?
**Current default:**
> (Queued, Skipped) => Ok(()), // CA cutoff — only Queued can skip

**Assessor verdict:** sound — Narrowest safe transition, but narrower than precedent: the existing DependencyFailed cascade at rio-scheduler/src/actor/completion.rs:678-680 handles Queued|Ready|Created under the same never-running invariant. find_newly_ready() fires at completion.rs:408 — if cascade runs after, targets may already be Ready and silently missed. Plan's stated 'Pending' alternative is bogus (no such variant in derivation.rs:17-37).

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Also allow (Ready, Skipped) | Matches DependencyFailed cascade precedent; order-independent vs find_newly_ready(); Ready means not-yet-dispatched so no worker signaling needed. Recommended. |
| Also allow (Created, Skipped) | Covers pre-queue nodes; matches full DependencyFailed precedent; but Created→Completed cache-hit path may already handle this. |
| Queued-only + enforce cascade runs BEFORE find_newly_ready() | Keeps state machine minimal but couples correctness to call ordering in completion.rs — fragile across refactors. |

### 25. `envoy-image-pinning` — dep-choice (single-plan, score=3)

**Plans:** P0282
**The question:** Which dependency for `envoy-image-pinning`?
**Current default:**
> envoyImage: envoyproxy/envoy:v1.29-latest

**Assessor verdict:** **NOT sound** — Floating tag conflicts with project convention — nix/docker-pulled.nix:4 states "pulls upstream images pinned by digest"; P0273 T3 will add a digest-pinned envoy there for VM tests, so test and prod versions silently diverge as -latest rolls. Concrete footgun: global.image.pullPolicy defaults to IfNotPresent (values.yaml:15, applied via all templates e.g. fod-proxy.yaml:77) — with a floating tag this means different nodes cache different patches and security updates never reach existing nodes. This is the first third-party container image directly referenced in the chart (all others go through `rio.image` helper on nix-built images); fod-proxy.yaml:75 comment already notes "Third-party tags can vanish; ours can't".

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Pinned patch version (e.g., envoyproxy/envoy:v1.29.5) matching whatever digest P0273 T3 pins | Requires manual bump for CVEs, but reproducible, consistent across nodes, and matches the VM-test image so what's tested is what runs. Single source of truth if the patch number is kept in lockstep with docker-pulled.nix (one comment cross-referencing the other). |
| Pinned digest (envoyproxy/envoy@sha256:...) | Strongest reproducibility and exactly matches docker-pulled.nix convention. Ugly in values.yaml, two places to bump (nix + helm), and --set overrides become unwieldy. Overkill given the patch-tag option gets 95% of the benefit. |
| Keep v1.X-latest but set imagePullPolicy: Always on the envoy container only | Fixes node-skew (all nodes converge on restart) and actually delivers the 'auto security patches' benefit. Still non-reproducible across helm rollbacks, still diverges from VM-test digest. Introduces per-container pullPolicy deviation from the global knob. |

### 26. `keyset-cursor-tiebreaker` — data-model (single-plan, score=3)

**Plans:** P0271
**The question:** How should we model `keyset-cursor-tiebreaker`?
**Current default:**
> "SELECT * FROM builds WHERE submitted_at < $1 ORDER BY submitted_at DESC LIMIT $2" ... let next_cursor = builds.last().map(|b| b.submitted_at.to_rfc3339());

**Assessor verdict:** **NOT sound** — Regression from existing code: db.rs:283 already orders by (b.submitted_at DESC, b.build_id DESC) — the tiebreaker is ALREADY there and the plan drops it. submitted_at is TIMESTAMPTZ DEFAULT now() (001_scheduler.sql:23) — microsecond precision, but CI batch-submit can hit collisions. The T3 test (250 serial inserts) won't catch this: admin/tests.rs:911 shows the existing test suite sleeps between inserts 'so submitted_at ordering is deterministic'. Also: BuildListRow (db.rs:132-141) has no submitted_at field, so `b.submitted_at.to_rfc3339()` doesn't compile without adding it to the struct + SELECT — unstated scope.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Compound keyset: WHERE (b.submitted_at, b.build_id) < ($1, $2) with row-value comparison | Postgres supports row-value < natively. Matches existing ORDER BY at db.rs:283. Cursor must encode both values (ties into decision 2's encoding choice). This is the standard textbook fix and cheap to do now vs. a proto break later. |
| Single-column + document the limitation | T3's 'no duplicates, no gaps' assertion becomes false advertising. Acceptable for a low-stakes admin dashboard IF explicitly noted, but the plan claims it tests exactly this property. |
| Force unique submitted_at via CREATE UNIQUE INDEX ON builds (submitted_at) | Turns a pagination glitch into INSERT failures under concurrency. Strictly worse. |

### 27. `run-gc-extracted-signature` — api-shape (single-plan, score=3)

**Plans:** P0212
**The question:** What shape should `run-gc-extracted-signature` take on the wire/API?
**Current default:**
> pub async fn run_gc(pool: &PgPool, backend: Arc<dyn ChunkBackend>, dry_run: bool, progress_tx: mpsc::Sender<GcProgress>) -> Result<GcStats>

**Assessor verdict:** **NOT sound** — Signature is missing two required params. rio-store/src/grpc/admin.rs:552 calls gc::mark::compute_unreachable(&pool, grace_hours, &req.extra_roots) and admin.rs:489 calls check_empty_refs_gate(&pool, grace_hours, ...) — both are INSIDE the block being extracted. Without grace_hours + extra_roots params, run_gc either hardcodes defaults (breaking the gRPC API that accepts user-provided extra_roots, tested at admin.rs:1062-1095) or won't compile. The mpsc::Sender<GcProgress> (dropping Result<_, Status>) is GOOD decoupling — keeps tonic out of gc/. backend: Arc<dyn ChunkBackend> (dropping Option) is fine — wrapper unwraps once.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Add grace_hours: u32, extra_roots: &[String] as positional params | Minimal change, preserves gRPC API fidelity; cron caller passes grace_hours=2, extra_roots=&[]. Six params is at the edge of readability but acceptable. |
| Bundle into struct GcParams { dry_run, grace_hours, extra_roots } passed by ref | More extensible if future GC knobs appear; slightly more ceremony (new type) for only 3 fields today |
| Keep signature as-is, hardcode grace_hours=2 + empty extra_roots inside run_gc | Breaks existing gRPC TriggerGC semantics — user-provided extra_roots silently ignored, grace_period_hours request field becomes dead. Regresses tests at admin.rs:1062+. |

### 28. `ssa-field-manager-conditions` — api-shape (single-plan, score=3)

**Plans:** P0238
**The question:** What shape should `ssa-field-manager-conditions` take on the wire/API?
**Current default:**
> &PatchParams::apply("rio-controller-build-conditions").force()

**Assessor verdict:** **NOT sound** — Existing patch_status at build.rs:1114-1132 uses MANAGER="rio-controller" and serializes the FULL BuildStatus struct including conditions. Introducing a second SSA field manager writing status.conditions creates ownership conflict: with .force() both managers claim the field, last-write-wins, so each patch DELETES conditions written by the other. The Failed/Cancelled conditions (set via set_condition → patch_status → MANAGER) would be clobbered by the new manager's Scheduled/Building patch and vice versa. This is the exact SSA foot-gun noted at crds/build.rs:94 comment.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Reuse MANAGER="rio-controller" + existing patch_status() — route through apply_event() match arms like Failed/Cancelled already do | Single field manager owns status.conditions; no clobbering. Minor con: one manager for all status writes makes managedFields less granular for debugging, but that's the existing pattern. |
| Separate manager but ONLY write via set_condition into a full BuildStatus and have the existing manager relinquish conditions | Requires refactoring existing Failed/Cancelled arms to use the new manager too. Pointless churn — no functional win over single manager. |

### 29. `tenant-scope-fail-open-vs-closed` — fallback (single-plan, score=3)

**Plans:** P0264
**The question:** What's the fallback behavior for `tenant-scope-fail-open-vs-closed`?
**Current default:**
> let where_clause = match tenant_id { Some(tid) => "WHERE tenant_id = $1 OR tenant_id IS NULL", // shared chunks visible None => "", // anonymous: see all (backward-compat) };

**Assessor verdict:** **NOT sound** — After P0259, the jwt_interceptor is wired globally in rio-store/src/main.rs and returns `Status::unauthenticated` if `x-rio-tenant-token` is missing (plan-0259 line 28). So `request.extensions().get::<Claims>()` returning None means either (a) interceptor not wired — a bug — or (b) some internal unauth'd path. The `None => ""` branch is fail-open: a wiring bug silently grants full cross-tenant chunk visibility instead of erroring. Also: `Claims.sub` is `Uuid` not `Option<Uuid>` (plan-0257:27) so the extraction shape is correct, but the `OR tenant_id IS NULL` clause means all pre-migration chunks remain visible to every tenant indefinitely — reasonable for backfill compat but undercuts the isolation guarantee the plan purports to add.

**Alternatives:**

| Option | Tradeoff |
|---|---|
| Fail-closed: None => return Err(Status::unauthenticated("tenant scoping requires auth")) | Matches security posture. If P0259 interceptor is truly global, this branch is unreachable anyway — so it's a free assertion. Breaks only if there's a deliberate unauth'd internal caller, which should be explicit not implicit |
| Feature-flag the scoping (config bool tenant_scoped_chunks) | Single-tenant deployments set it false and get the old behavior; multi-tenant sets true and requires auth. Clearer intent than inferring mode from Claims presence. Extra config surface |
| Keep as written — fail-open for backward-compat | Only defensible if store has legitimate unauth'd callers post-P0259. Plan doesn't identify any. Silent-wrong is worse than loud-wrong for a security boundary |

---

## Confirm queue (quick ack — sound defaults)

**117 groups.** All members `recommendation=confirm`. Sorted by score (blast × 1).

| Key | Plans | Category | Blast | Current default | Why sound |
|---|---|---|---|---|---|
| `ca-detect-field-shape` | P0248 | api-shape | proto-field | // Phase 5 CA cutoff: set by gateway translate.rs from has_ca_floating_outputs() \|\| is_fi… | All downstream consumers (P0251 completion.rs `if state.is_ca`, P0253 dispatch `IF state.is_ca AND...`) need only a binary gate; none disti… |
| `graph-node-cap` | P0245 | threshold | proto-field | Graph rendering MUST degrade to a sortable table when the node count exceeds 2000. dagre … | GetBuildGraphResponse does not exist in rio-proto/proto/*.proto (zero matches for 'truncated') — new proto message. The three tiers (500/20… |
| `graph-rpc-thin-vs-fat-node` | P0276 | data-model | proto-field | Does NOT reuse `DerivationNode` — that carries `bytes drv_content` (types.proto:32, ≤64KB… | Verified: types.proto:32 has `bytes drv_content = 9` with ≤64KB gateway-enforced comment; rio-proto/src/lib.rs:12 sets DEFAULT_MAX_MESSAGE_… |
| `heartbeat-store-degraded-field-type` | P0205 | api-shape | proto-field | bool store_degraded = 9; | Field 9 verified as next available in HeartbeatRequest (types.proto:367 ends at size_class=8). Type `bool` matches the `draining` analogy (… |
| `proto-status-string-vs-enum` | P0276 | data-model | proto-field | string status = 4; // derivations.status CHECK constraint values | Strong precedent: types.proto:686 `string status` for worker status, types.proto:694 `string status_filter` for builds — this codebase cons… |
| `sizeclass-status-proto-shape` | P0231 | api-shape | proto-field | message SizeClassStatus { string name = 1; double effective_cutoff_secs = 2; double confi… | Types align with source of truth: `cutoff_secs: f64` at assignment.rs:59 → `double` correct (matches `double peak_cpu_cores` convention at … |
| `build-samples-no-fk` | P0227 | data-model | migration | No FK to `build_history` — samples outlive individual build rows (GC may delete build_his… | Conclusion correct, reasoning WRONG. Verified: build_history (001_scheduler.sql:129-141) has PRIMARY KEY (pname, system) and is UPSERTed in… |
| `build-samples-schema-index` | P0227 | data-model | migration | CREATE TABLE build_samples ( id BIGSERIAL PRIMARY KEY, pname TEXT NOT NULL, system TEXT N… | Verified against P0229's actual query (plan-0229:104): `WHERE completed_at > now() - interval` with no pname/system filter — the SITA-E alg… |
| `derivation-is-ca-column-type` | P0249 | data-model | migration | ALTER TABLE derivations ADD COLUMN IF NOT EXISTS is_ca BOOLEAN NOT NULL DEFAULT FALSE; | Exact precedent at migrations/004_recovery.sql:62 — `is_fixed_output BOOLEAN NOT NULL DEFAULT false`. P0250's detection is binary: `is_ca =… |
| `jti-pk-type` | P0249 | data-model | migration | jti TEXT PRIMARY KEY | P0257's Claims struct defines `pub jti: String` (plan-0257 line 34) — string type confirmed. RFC 7519 §4.1.7 defines jti as StringOrURI. Co… |
| `path-tenants-no-narinfo-fk` | P0206 | data-model | migration | WHY NO FK→narinfo: follows migrations/007_live_pins.sql precedent — path_tenants referenc… | Precedent verified exactly at migrations/007_live_pins.sql:15-19 — same rationale (scheduler writes before narinfo exists), same mitigation… |
| `path-tenants-tenant-fk-cascade` | P0206 | data-model | migration | tenant_id UUID NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE | CASCADE is correct for a NOT NULL FK in a pure join table — row is meaningless without tenant. Differs from migrations/009_phase4.sql:33,37… |
| `skipped-terminal-recovery-exclusion` | P0252 | data-model | migration | -- Before: WHERE status NOT IN ('completed', 'failed', 'poisoned', 'dependency_failed') -… | Decision (add 'skipped' to terminal set) is correct — Skipped is terminal, failover must not re-queue. BUT plan's 'Before' text is WRONG: a… |
| `tenant-key-active-selection` | P0256 | data-model | migration | "SELECT ed25519_seed FROM tenant_keys WHERE tenant_id = $1 AND revoked_at IS NULL ORDER B… | P0249 migration 014 schema (plan-0249:25-34) defines exactly these columns: ed25519_seed BYTEA, created_at TIMESTAMPTZ, revoked_at TIMESTAM… |
| `tenant-key-storage-format` | P0249 | data-model | migration | ed25519_seed BYTEA NOT NULL, created_at TIMESTAMPTZ NOT NULL DEFAULT now(), revoked_at TI… | Matches existing Signer construction at rio-store/src/signing.rs:104-114 — 32-byte seed fed to SigningKey::from_bytes. Consumer P0256 expec… |
| `ca-cutoff-cascade-bound` | P0245 | threshold | spec-marker | The transition cascades recursively (depth-capped at 1000). Running derivations are NEVER… | The 'never kill Running' half is a restatement of r[sched.preempt.never-running] at scheduler.md:246-247, which already explicitly covers C… |
| `fuse-circuit-metric-type` | P0209 | api-shape | spec-marker | metrics::describe_gauge!("rio_worker_fuse_circuit_open", "1.0 when FUSE circuit breaker i… | Name follows the `rio_worker_fuse_*` convention — 7 existing metrics in rio-worker/src/lib.rs:70-113 and docs/src/observability.md:150-159 … |
| `gateway-auth-dual-mode` | P0245 | config-default | spec-marker | Gateway auth is two-branched PERMANENTLY: `x-rio-tenant-token` header present → JWT verif… | auth_mode config does not exist yet (zero matches). SSH-comment auth at rio-gateway/src/server.rs (r[gw.auth.tenant-from-key-comment]) is t… |
| `gc-retention-first-vs-last-reference` | P0206 | data-model | spec-marker | first_referenced_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY (store_path_hash, ten… | Composite PK mirrors migrations/007_live_pins.sql:29 exactly. `first_referenced_at` semantics is FORCED by spec marker r[sched.gc.path-tena… |
| `gc-runs-result-label-cardinality` | P0212 | api-shape | spec-marker | metrics::counter!("rio_controller_gc_runs_total", "result" => "success").increment(1); ..… | observability.md:185 currently specs 'result: success/failure' (two values); plan uses three (success/rpc_failure/connect_failure). This is… |
| `hmac-bypass-check-order` | P0224 | api-shape | spec-marker | The check is CN-first, SAN-second; either match grants bypass. | Semantically vacuous given OR semantics — check order only affects which branch short-circuits, not the result. CN-first is a fine micro-op… |
| `hmac-bypass-identity-matching` | P0224 | api-shape | spec-marker | `hmac_bypass_cns: Vec<String>` config (default `["rio-gateway"]`) + SAN DNSName check via… | Verified against put_path.rs:108 (hardcoded Some("rio-gateway") match) and existing test cert_cn_no_cn_returns_none at :608 which documents… |
| `log-stream-decode-policy` | P0245 | fallback | spec-marker | `GetBuildLogs` server-stream consumption MUST use `TextDecoder('utf-8', {fatal: false})` … | GetBuildLogs streaming already exists (admin.proto:21, rio-scheduler/src/admin/mod.rs:340). Build logs from real compilers absolutely conta… |
| `multi-output-atomicity-blob-orphan` | P0245 | fallback | spec-marker | Multi-output derivation registration MUST be atomic at the DB level: all output rows comm… | Standard pattern for heterogeneous-store commits: blob backends (S3-style) can't participate in PG transactions, so orphan + GC-sweep is co… |
| `ssa-force-vs-conflict` | P0233 | api-shape | spec-marker | &PatchParams::apply(MANAGER).force(), — SSA-apply with force (field manager `rio-controll… | Every SSA call in rio-controller uses `.force()` — 6 call sites (workerpool/mod.rs:142,157,204,251; build.rs:1127; scaling.rs:338,477). Doc… |
| `tenant-quota-cache-ttl` | P0245 | threshold | spec-marker | Enforcement is eventually-consistent — `tenant_store_bytes` may be cached with ≤30s TTL. … | tenant_store_bytes does not exist yet (zero matches) — greenfield. 30s TTL for a submit-time gate is appropriate: the check fires once at S… |
| `admin-list-builds-pagination-strategy` | P0271 | api-shape | multi-plan | Keyset pagination: `WHERE submitted_at < $cursor ORDER BY submitted_at DESC LIMIT $n`. | Keyset over offset is correct — builds.rs:19-21 already documents offset instability under concurrent inserts. But there is NO index on sub… |
| `authenticated-tenant-struct-shape` | P0207 | api-shape | multi-plan | Change `AuthenticatedTenant(Option<String>)` (write-only today) to carry both: pub struct… | Verified write-only: grep shows 5 hits, all in auth.rs itself (definition + 2 .insert() calls at :75, :86). Zero downstream readers today =… |
| `ca-cutoff-hash-source` | P0251 | assumption | multi-plan | cutoff does NOT read `realisations.output_hash` (that's zeros per opcodes_read.rs:493 and… | Verified against code: opcodes_read.rs:496 literally sets `output_hash = [0u8; 32]` with TODO(phase5) at :493 scoping it as a signing conce… |
| `ca-detect-includes-fod` | P0250 | assumption | multi-plan | `has_ca_floating_outputs()` covers floating-CA but not fixed-output-CA. `is_ca = is_fixed… | Both functions exist at rio-nix/src/derivation/mod.rs:211-226 and are mutually exclusive (hash.rs:291-292 asserts floating-CA drv has has_c… |
| `circuit-halfopen-heartbeat-semantics` | P0209 | api-shape | multi-plan | /// For heartbeat (P0210). Includes half-open as "not open". pub fn is_open(&self) -> boo… | Verified against P0210/P0211: `is_open()` → `HeartbeatRequest.store_degraded` → P0211 gates `has_capacity()` like `draining`. Probes are PA… |
| `dashboard-core-toolchain-deps` | P0274 | dep-choice | multi-plan | Dev: `typescript@5` (NOT native-preview), `vite@6`, `@sveltejs/vite-plugin-svelte`, `svel… | Stock typescript@5 has no postinstall (the trap at tracey.nix:53-56 is specific to @typescript/native-preview). These are the canonical Sve… |
| `dashboard-tech-stack` | P0245 | library-choice | multi-plan | Web dashboard for operational visibility. Svelte 5 SPA, Envoy-sidecar gRPC-Web translatio… | phase5.md:34 lists 'Envoy sidecar or tonic-web' — this picks Envoy. No rio-dashboard/ directory or helm templates exist yet; pure greenfiel… |
| `drift-classify-cutoff-snapshot` | P0228 | assumption | multi-plan | `classify()` takes `&[SizeClassConfig]` — at this point still the static `&self.size_clas… | Verified: size_classes is Vec<SizeClassConfig> at actor/mod.rs:135 (static). completion.rs:325 ALREADY uses &self.size_classes for cutoff_f… |
| `drv-filter-url-shape` | P0280 | api-shape | multi-plan | Click → navigate `/builds/:id?drv=<path>` — [P0279]'s LogViewer filters by it. | Query param is correct: `drv` is a FILTER on the build view, not a distinct resource. P0278:44 establishes `/builds/:id` as the deep-link b… |
| `intro-safety-warning-removal-gate` | P0284 | scope-boundary | multi-plan | MODIFY [`docs/src/introduction.md`] — **remove the `:50` warning** ("Multi-tenant deploym… | Warning at introduction.md:50 enumerates 3 gaps mapping exactly to the 3 gating plans (quotas→P0255, signing-keys→P0256, query-filtering→P0… |
| `jwt-crate-choice` | P0257 | dep-choice | multi-plan | Only new Cargo dep for the entire JWT lane: `jsonwebtoken`. | Verified: jsonwebtoken absent from Cargo.toml; ed25519-dalek v2 with pkcs8 feature already at Cargo.toml:136 (so to_pkcs8_der() works); spe… |
| `jwt-revocation-check-location` | P0245 | api-shape | multi-plan | The scheduler ADDITIONALLY checks `jti NOT IN jwt_revoked` (PG lookup — gateway stays PG-… | Verified: rio-gateway/src/ has zero PgPool/sqlx references — gateway is currently PG-free and this preserves that invariant. Scheduler alre… |
| `k3s-netpol-enforcer-choice` | P0220 | assumption | multi-plan | The phase4c.md assumption (A2) is that stock k3s ships kube-router with NetworkPolicy enf… | The underlying assumption is well-supported: nix/tests/fixtures/k3s-full.nix:216-235 confirms no `--disable-network-policy` in extraFlags, … |
| `narinfo-anon-access-policy` | P0245 | fallback | multi-plan | Authenticated narinfo requests MUST filter results by `path_tenants.tenant_id = auth.tena… | path_tenants is only a comment at migrations/009_phase4.sql:7 — not created yet. Anonymous-unfiltered preserves standard Nix substituter be… |
| `rebalancer-pure-vs-apply` | P0229 | scope-boundary | multi-plan | This is a **pure module** — it computes new cutoffs but does NOT apply them (P0230 wires … | P0230:13 already depends on this contract verbatim. The convergence test (plan:164-167) iterates compute_cutoffs 3× with no RwLock, no asyn… |
| `seccomp-profile-enum-shape` | P0223 | data-model | multi-plan | pub enum SeccompProfileKind { RuntimeDefault, Localhost { localhost_profile: String }, Un… | Exact mirror of k8s core/v1 SeccompProfile.type's three values — zero impedance mismatch. Including Unconfined follows the existing `privil… |
| `spa-api-baseurl-strategy` | P0277 | config-default | multi-plan | // baseUrl '/' — prod: nginx proxies /rio.admin.AdminService/* → envoy:8080. // dev: vite… | Verified coherent across three plans. P0274:20 sets vite server.proxy['/rio.admin.AdminService'] → localhost:8080. P0282:30-36 sets nginx t… |
| `ssa-autoscaler-distinct-manager` | P0234 | api-shape | multi-plan | SSA replica patch with DISTINCT field manager so it doesn't fight the reconciler's apply.… | Necessary, not just stylistic. Direct parallel of scaling.rs:338 (`rio-controller-autoscaler`). The WPS autoscaler patches child WorkerPool… |
| `tenant-name-normalization-scope` | P0217 | api-shape | multi-plan | This plan adds a `NormalizedName` newtype in `rio-common` that centralizes normalization … | Trim-only matches all current call sites exactly (grpc/mod.rs:445, admin/mod.rs:717, server.rs:354 — none lowercase, none validate chars). … |
| `tenant-retention-first-vs-last-referenced` | P0207 | data-model | multi-plan | WHERE pt.first_referenced_at > now() - make_interval(hours => t.gc_retention_hours) | Matches spec verbatim (store.md:201-203), matches P0206 schema (first_referenced_at TIMESTAMPTZ DEFAULT now() + ON CONFLICT DO NOTHING at P… |
| `tenant-signing-key-fallback` | P0256 | fallback | multi-plan | This adds a `tenant_keys` lookup (migration 014 via [P0249](plan-0249-migration-batch-014… | Current signing.rs:34-40 has a single `key: SigningKey` field — the 'single cluster SigningKey' claim is accurate. multi-tenancy.md:66 spec… |
| `wps-child-pool-naming` | P0232 | api-shape | multi-plan | Size classes. Each becomes a child WorkerPool named `{wps}-{class.name}`. | Spec-mandated at phase4c.md:29 (`name = {wps-name}-{class-name}`). P0233:33,118 / P0234:42,88 / P0237:72 all hardcode `format!("{}-{}", wps… |
| `wps-crd-kube-attrs` | P0232 | api-shape | multi-plan | #[kube(group = "rio.build", version = "v1alpha1", kind = "WorkerPoolSet", namespaced, sta… | Matches existing CRD conventions exactly: workerpool.rs:33-34 and build.rs:18-19 both use group="rio.build" version="v1alpha1"; shortname="… |
| `admin-rpc-pg-vs-actor-snapshot` | P0276 | data-model | single-plan | PG-backed via 3-table JOIN on migrations/001_scheduler.sql:39-100 schema — not actor-snap… | All 3 tables verified at migrations/001_scheduler.sql:39-82. Actor-snapshot is a REAL alternative (ActorCommand::QueryBuildStatus{build_id}… |
| `bench-framework-choice` | P0221 | dep-choice | single-plan | criterion = { version = "0.5", features = ["async_tokio", "html_reports"] } | Not currently in workspace (new dep, clean slate). `async_tokio` is load-bearing — the bench code at plan:76 uses `b.to_async(&rt).iter(\|\| … |
| `buildevent-carries-worker-id` | P0270 | assumption | single-plan | Per 4c A3: "if `BuildEvent` carries worker IDs, trivial." Verify at dispatch: `grep -A3 '… | Verified: types.proto:121 has `DerivationStarted { string worker_id = 1 }`, reachable via `BuildEvent.derivation.status.started`. Caveat: b… |
| `ca-cascade-depth-cap-failopen` | P0252 | fallback | single-plan | if depth >= MAX_CASCADE_DEPTH { tracing::warn!(depth, "CA cutoff cascade hit depth cap");… | Fail-open is correct: un-cascaded nodes just build normally (wasted CPU, correct output). Aborting completion would be strictly worse (fail… |
| `ca-cascade-transitivity` | P0252 | api-shape | single-plan | // Recurse: if eligible was also CA + unchanged, keep walking. // (Transitivity: A unchan… | Transitivity is correct IFF Skipped implies 'cached output exists for reuse' (B's previous output, produced when A last had this identical … |
| `ca-cutoff-store-error-fallback` | P0251 | fallback | single-plan | .await .map(\|r\| r.into_inner().found) .unwrap_or(false); | Fail-safe direction: error → false → ca_output_unchanged=false → P0252 doesn't skip → build runs anyway. Worst case is wasted CPU, never wr… |
| `ca-resolve-dispatch-gate` | P0253 | api-shape | single-plan | let drv_to_send = if state.has_ca_inputs() { ca::resolve_ca_inputs(&state, &realisations)… | has_ca_inputs() is the semantically correct predicate: resolution rewrites INPUT placeholder paths (ADR-004:15), so it's gated on INPUTS be… |
| `chaos-link-coverage` | P0268 | scope-boundary | single-plan | NixOS module wrapping toxiproxy as a systemd service between scheduler↔store and worker↔s… | These two links cover the data plane where retry loops live: rio-worker/src/upload.rs:136-154 has a real store-upload retry loop (subtests … |
| `chaos-proxy-tool` | P0268 | dep-choice | single-plan | NEW [`nix/tests/fixtures/toxiproxy.nix`](../../nix/tests/fixtures/toxiproxy.nix) — NixOS … | toxiproxy is in nixpkgs (v2.12.0, `nix eval nixpkgs#toxiproxy.version`), ships `toxiproxy-cli`, and supports all 4 toxics the plan uses (la… |
| `child-cleanup-explicit-vs-ownerref-gc` | P0233 | fallback | single-plan | Finalizer-wrapped cleanup explicitly deletes children (belt-and-suspenders — ownerRef han… | Confirmed against rio-controller/src/reconcilers/workerpool/mod.rs:309-436 — the existing WP cleanup does NOT explicitly delete children (i… |
| `chunk-tenant-scope-cuttable` | P0264 | scope-boundary | single-plan | **CUTTABLE — zero downstream deps.** Coordinator cuts on schedule slip. | Verified: P0264 appears only as a soft_dep in P0284 (phase closeout), which explicitly handles the cut case ('if cut, add Phase 6 deferral … |
| `chunk-upsert-return-type` | P0208 | api-shape | single-plan | Return type changes: `Result<(), ...>` → `Result<HashSet<Vec<u8>>, ...>`. | Confirmed at chunked.rs:60 (current `Result<()>`). HashSet<Vec<u8>> exactly matches what cas.rs:225 already constructs (`std::collections::… |
| `class-drift-metric-labels` | P0228 | api-shape | single-plan | &["assigned_class", "actual_class"] | Standard from/to transition-metric pattern. Cardinality trivially bounded: assignment.rs:92 says 'typically 2-4 classes' → max 16 series, n… |
| `classify-bump-step-semantics` | P0230 | api-shape | single-plan | if let Some(cpu_limit) = classes[chosen].cpu_limit_cores { if ema_peak_cpu_cores > cpu_li… | Verified mem-bump at assignment.rs:122-127 is single-step (`sorted.get(i + 1)`) — this correctly mirrors it. Impl caveat: snippet indexes `… |
| `cli-prost-json-serialize` | P0216 | library-choice | single-plan | Verify prost-generated types have `serde::Serialize` — if not, either add thin wrapper st… | Premise verified: rio-proto/build.rs:2-14 has no type_attribute/pbjson — prost types definitively lack Serialize. rio-proto has 7 reverse d… |
| `cluster-status-poll-vs-stream` | P0277 | library-choice | single-plan | $effect(() => { const id = setInterval(async () => { try { status = await admin.clusterSt… | admin.proto:12 ClusterStatus returns ClusterStatusResponse (unary, no stream keyword) — polling is the only option without a proto change. … |
| `completion-tenant-lookup-borrow` | P0206 | assumption | single-plan | If the tenant-lookup at `completion.rs` turns out to need a different borrow pattern than… | Verified: assumption holds, no borrow conflict. `get_interested_builds` returns owned `Vec<Uuid>` (rio-scheduler/src/actor/mod.rs:907), `Bu… |
| `cron-connect-per-tick-timeout` | P0212 | fallback | single-plan | The cron loop **connects fresh per iteration INSIDE the loop with `tokio::time::timeout(3… | Validated against lang-gotchas.md:44/88/89 — tonic has no default connect timeout, stale IP = SYN hang, eager connect in main = process exi… |
| `cut-feature-deferral-block` | P0284 | scope-boundary | single-plan | IF [P0264](plan-0264-findmissingchunks-tenant-scope.md) was cut: `store.md` gets `> Phase… | P0264 is currently UNIMPL in dag.jsonl and self-describes as 'CUTTABLE — zero downstream deps', so the cut-path is live. The '> Phase X def… |
| `dag-edge-render-direction` | P0280 | api-shape | single-plan | for (const e of ge) g.setEdge(e.childDrvPath, e.parentDrvPath); // child→parent = build o… | Proto semantics (P0276:27-28, matching existing DerivationEdge at rio-proto/proto/types.proto:56-57): parent=depends-on, child=dependency. … |
| `dag-layout-engine` | P0280 | dep-choice | single-plan | `package.json` — add `@xyflow/svelte`, `@dagrejs/dagre`. Hash bump. | Two sub-choices bundled. `@xyflow/svelte` is USER-VALIDATED via A7 (.claude/notes/phase5-dashboard-partition.md:13 — 'React 18 → Svelte 5 +… |
| `dashboard-deeplink-build-fetch` | P0278 | fallback | single-plan | Deep-link fallback: `/builds/:id` with no list context → `listBuilds({limit:1000})` clien… | Verified: admin.proto:18 has no GetBuild RPC, types.proto:693-699 ListBuildsRequest has no build_id filter, scheduler builds.rs:33 confirms… |
| `dashboard-helm-enabled-default` | P0282 | config-default | single-plan | dashboard: enabled: false # default off — existing deploys unchanged | Matches established pattern in values.yaml: optional/non-critical-path features default off (coverage:53, externalSecrets:65, bootstrap:80,… |
| `dashboard-lint-test-bundled-buildphase` | P0274 | config-default | single-plan | # Unlike tracey.nix:57-61 — svelte-check + lint + test in sandbox. buildPhase = '' runHoo… | Repo convention (flake.nix:320/336/372/464) is separate check derivations per concern, but that's for Rust where crane shares cargoArtifact… |
| `dashboard-optimistic-write-ux` | P0281 | fallback | single-plan | **USER A7: Svelte.** Optimistic updates via `$state` mutation + revert-on-error. | DrainWorker (types.proto:806-814) is a fast unary RPC returning {accepted, running_builds} — sub-second state flip, ideal for optimistic UI… |
| `dashboard-route-paths` | P0277 | api-shape | single-plan | <Route path="/" component={Cluster} /> <Route path="/builds" component={Builds} /> <Route… | SPA-internal paths, no spec markers reference them (grep docs/src/ for r[dash.* route patterns found nothing). /builds and /builds/:id both… |
| `dashboard-typecheck-in-sandbox` | P0274 | config-default | single-plan | Clones nix/tracey.nix:32-68 with one deviation: we run `svelte-check` in buildPhase (trac… | Verified at nix/tracey.nix:53-56 — tracey's skip was a workaround for @typescript/native-preview's binary-fetching postinstall being blocke… |
| `dashboard-vm-browser-vs-curl` | P0283 | scope-boundary | single-plan | **De-risks R9 by not taking the risk.** Playwright = ~500MB chromium FOD + font-config sa… | All four factual claims verified: zero chromium/playwright matches in nix/tests/; vitest rendering tests exist in P0274:21-25 (jsdom env + … |
| `dashboard-vs-grafana-scope` | P0284 | scope-boundary | single-plan | MODIFY [`docs/src/phases/phase5.md`] `:32` — **strike** "Worker utilization graphs, cache… | Verified: phase5.md:32 says exactly this, and plan-0222-grafana-dashboards.md T2 ships worker-utilization.json + T3 store-health.json with … |
| `db-tx-blob-no-rollback` | P0267 | fallback | single-plan | **Per R10:** tx covers DB rows only. Blob-store writes are NOT rolled back — orphaned blo… | Pattern is already established and working: cas.rs:172-176 explicitly says 'We DON'T delete from S3 — GC sweep's job... Deleting now races … |
| `ema-midbuild-update-gate` | P0266 | threshold | single-plan | if resources.peak_memory_bytes > current_ema { // Same update logic as completion.rs:336 … | One-sided `>` gate is correct: mid-build peak_memory_bytes is monotonically non-decreasing (cgroup memory.peak), so the observed value is a… |
| `envoy-mtls-cert-source` | P0273 | scope-boundary | single-plan | Check how rio-cli/rio-controller present mTLS certs to scheduler — likely a shared CA + c… | Ground truth resolves the stated OR: cert-manager.yaml:77 ranges over scheduler/store/gateway/controller, values.yaml defaults provisioner=… |
| `fuse-sync-primitives` | P0209 | library-choice | single-plan | `ensure_cached` at `:54` is `fn` not `async fn`. FUSE callbacks run on fuser's thread poo… | Verified: fetch.rs:54 is `pub(super) fn ensure_cached(...)` (sync), and fetch.rs:268-269 has the exact R-SYNC comment quoted. The no-tokio … |
| `gateway-ratelimit-crate` | P0213 | dep-choice | single-plan | per-tenant build-submit rate limiting via [`governor`](https://docs.rs/governor) keyed on… | Gateway transport is russh SSH (server.rs), not tower HTTP — so tower::limit::RateLimitLayer doesn't apply naturally. No existing rate-limi… |
| `gateway-ttl-cache-impl` | P0255 | dep-choice | single-plan | Simple `DashMap<Uuid, (u64, Instant)>` with TTL check. Or `moka::sync::Cache` if already … | The conditional resolves: moka IS in workspace deps (Cargo.toml:125) — but with features=["future"], not "sync" as the plan states. moka::f… |
| `grafana-dashboard-configmap-packaging` | P0222 | api-shape | single-plan | Or, since this is NOT a helm template (no `.Files.Get` context), ship as a plain kubectl-… | The plan's own Files block (line 105-111) places everything in infra/helm/grafana/ as a SIBLING to the rio-build chart, not inside it — .Fi… |
| `graph-edge-subgraph-scoping` | P0276 | data-model | single-plan | WHERE e.parent_id IN (SELECT derivation_id FROM build_derivations WHERE build_id = $1) AN… | Both-IN is the correct subgraph projection: guarantees every drv_path in edges[] also appears in nodes[] — no dangling arrows in the dashbo… |
| `gt13-verify-gates-scope` | P0245 | scope-boundary | single-plan | If `false-alarm`: P0267 becomes tracey-annotate-only (4c P0226 pattern). If `real`: P0267… | This is a process gate, not a design choice: T4 runs an actual unit test against rio-store/src/cas.rs with fault injection between PutPath … |
| `inflight-scopeguard-test-gate` | P0225 | fallback | single-plan | **If this test passes cleanly** → self-heal is proven → proceed to T2 (doc-only). **If th… | Mechanistically verified at cas.rs:497-566: fetch is tokio::spawn'd (survives abort), Shared at :536 returns instantly once complete, entry… |
| `infra-fail-retry-cap` | P0219 | assumption | single-plan | **Design note:** the original plan's `per_worker_failures` HashMap would have let us cap … | P0211 plan (line 30) confirms `has_capacity()` will gate on `!store_degraded`, and `best_worker()` hard-filters on it (assignment.rs:175). … |
| `jti-revocation-check-placement` | P0259 | api-shape | single-plan | In scheduler's `SubmitBuild` handler (NOT in the interceptor — interceptor is shared acro… | Conclusion sound, stated rationale partially wrong: store DOES have PG (rio-store/Cargo.toml has sqlx; content_index.rs:30 uses PgPool agai… |
| `log-viewer-array-append-strategy` | P0279 | scope-boundary | single-plan | **Perf note:** `[...lines, ...decoded]` is O(n) per chunk. `types.proto:178` says 64 line… | YAGNI stance is defensible. Proto claim verified (rio-worker/src/log_stream.rs:4 confirms 64/100ms), though actual chunks hitting the brows… |
| `log-viewer-virtual-scroll-lib` | P0279 | library-choice | single-plan | Virtual scrolling (if needed for >10k lines): `@tanstack/svelte-virtual` or hand-rolled `… | Correctly framed as conditional ('if needed'). 10k-line threshold is reasonable — browsers render 10k <pre> elements fine but the follow-ta… |
| `multi-output-atomicity-scope` | P0267 | scope-boundary | single-plan | **Scope depends on [P0245](plan-0245-prologue-phase5-markers-gt-verify.md)'s GT13 verify … | Gating scope on empirical verification is correct methodology. GT13-OUTCOME is NOT yet recorded (only task description at phase5-partition.… |
| `nix-cache-info-public-route` | P0225 | config-default | single-plan | /nix-cache-info must be public: nix clients hit it FIRST to discover the cache's `StoreDi… | Verified at cache_server/mod.rs:77-80 — the existing TODO already states 'Real Nix clients probe this endpoint first (before authenticating… |
| `orphan-chunk-grace-ttl` | P0262 | fallback | single-plan | Grace-TTL: a chunk with no manifest reference is held for `grace_seconds` before GC eligi… | This is exactly what the existing TODO at rio-store/src/grpc/chunk.rs:62-65 already recommends ("probably: chunk without manifest gets a sh… |
| `output-hash-todo-owner` | P0256 | assumption | single-plan | Per GT5 analysis: cutoff does NOT read this field (it compares `nar_hash` via content_ind… | Directly corroborated by existing code. opcodes_read.rs:485-495 comment says 'Zeros are fine for cache-hit purposes — QueryRealisation only… |
| `parking-lot-rwlock-await-guard` | P0230 | fallback | single-plan | **R10 CHECK — no `.await` across the guard.** `parking_lot::RwLock` is sync; holding a gu… | Verified dispatch.rs:76-81: `classify()` call is fully sync — `self.estimator.peak_memory()` is not awaited, no `.await` between field acce… |
| `pg-unnest-duplicate-spike` | P0208 | assumption | single-plan | **Expected:** per-row output with correct `inserted` bools. Likely `(1,true),(1,false),(2… | The spike-then-document discipline is sound process. Two notes: (1) PostgreSQL actually ERRORS on duplicate target keys in INSERT...ON CONF… |
| `quota-reject-predictive-vs-current` | P0255 | scope-boundary | single-plan | MVP: `estimated_new_bytes = 0` — reject on CURRENT overflow only, not predictive. | Consistent with the plan's own 'eventually-enforcing' semantics (line 7: 30s TTL already tolerates race-window overflow). Output NAR size i… |
| `rate-limiter-eviction-backend` | P0261 | dep-choice | single-plan | **Option A (preferred — moka):** ... use moka::sync::Cache; ... .max_capacity(max_keys) /… | moka is already in workspace deps at Cargo.toml:125 (features=['future'], used by rio-store) — Option B's 'dep weight' escape hatch is moot… |
| `ratelimit-key-eviction` | P0213 | assumption | single-plan | **R-GOVN:** `DefaultKeyedRateLimiter` uses `dashmap`, no auto-eviction. Acceptable for 4b… | Verified at rio-gateway/src/server.rs:344-354: tenant_name is assigned from matched.comment() where matched is the SERVER-SIDE authorized_k… |
| `realisation-deps-reverse-index` | P0249 | data-model | single-plan | -- Reverse lookup: "who depends on this realisation?" (for cutoff cascade) CREATE INDEX r… | Index is correct but comment is misleading. P0252's cutoff cascade (find_cutoff_eligible at dag/mod.rs) walks the in-memory DAG — it never … |
| `sched-build-timeout-tick-poll` | P0214 | api-shape | single-plan | At [`rio-scheduler/src/actor/worker.rs:440`](../../rio-scheduler/src/actor/worker.rs), AF… | Verified: handle_tick is at worker.rs:440, backstop loop at :464 with r[impl sched.backstop.timeout]. Tick interval is 10s (main.rs:76). Th… |
| `seccomp-crd-default-value` | P0223 | config-default | single-plan | // Default: RuntimeDefault (None case + explicit RuntimeDefault) _ => json!({"type": "Run… | Preserves exact current behavior (builders.rs:260 already sets RuntimeDefault unconditionally). Existing WorkerPool CRs without the new fie… |
| `seccomp-deny-action-errno` | P0223 | fallback | single-plan | "action": "SCMP_ACT_ERRNO", "errnoRet": 1 | ERRNO+EPERM(1) is the industry-standard choice (Docker default, containerd default, gVisor). KILL would crash builds that opportunistically… |
| `seccomp-denylist-syscall-set` | P0223 | scope-boundary | single-plan | "names": ["ptrace", "bpf", "setns", "process_vm_readv", "process_vm_writev"] ... The five… | All five are legitimate attacker tools under CAP_SYS_ADMIN and none are used by rio-worker. Critically, `setns` (join namespace) is blocked… |
| `silence-timeout-authority` | P0215 | scope-boundary | single-plan | The local nix-daemon MAY also enforce `maxSilentTime` itself (forwarded via `client_set_o… | Verified: daemon-side enforcement alone produces WRONG status — daemon's STDERR_ERROR hits stderr_loop.rs:286 → misc_fail() → BuildStatus::… |
| `sizeclass-snapshot-scan-vs-counter` | P0231 | assumption | single-plan | O(n) ready-queue scan is fine — this RPC is operator/autoscaler-facing, not hot path. | The principle holds: actor/mod.rs:798-800 documents ready_queue is bounded by ACTOR_CHANNEL_CAPACITY × derivations_per_submit; autoscaler p… |
| `ssa-status-distinct-manager` | P0234 | api-shape | single-plan | &PatchParams::apply("rio-controller-wps-status").force(), — status refresh uses DISTINCT … | Follows established precedent: scaling.rs:496 uses `rio-controller-autoscaler-status` distinct from the reconciler's `rio-controller` (work… |
| `svelte-router-dep` | P0277 | dep-choice | single-plan | `package.json` — add `svelte-routing` or a lightweight SPA router. `pnpmDeps` hash bump. | Hedged appropriately. P0274:16 scaffolds svelte@5 with runes; svelte-routing v2+ supports Svelte 5 and is history-API based, which pairs co… |
| `sync-clock-injection` | P0209 | library-choice | single-plan | Testability: inject time so tests don't need real sleeps. tokio::time::pause does NOT wor… | Claim verified: tokio::time::pause only affects tokio::time::Instant, not std::time::Instant — and this code is sync-only per D1. No existi… |
| `synth-dag-no-store-validity` | P0221 | assumption | single-plan | They do NOT need to correspond to real derivations — the scheduler's SubmitBuild path val… | Verified against rio-scheduler/src/grpc/mod.rs:382-402 — validation is `StorePath::parse(&node.drv_path)` + `.is_derivation()` + non-empty … |
| `tenant-key-active-index` | P0249 | data-model | single-plan | -- Active-key lookup is the hot path (signing every narinfo) CREATE INDEX tenant_keys_act… | Partial index exactly matches consumer query in P0256: `WHERE tenant_id = $1 AND revoked_at IS NULL`. Established pattern — migrations/001_… |
| `vm-fixture-standalone-vs-k3s` | P0283 | scope-boundary | single-plan | MODIFY [`nix/tests/default.nix`] — add `vm-dashboard-standalone` (standalone fixture, not… | Decisive reliability signal: known-flakes.jsonl has 8/8 entries k3s-related, 0 standalone. k3s-full.nix is 521 lines vs standalone's 239, n… |
| `wps-controller-owns-child-wp` | P0235 | api-shape | single-plan | .owns(wp_api, watcher::Config::default()) // watch child WorkerPools — WPS controller wat… | Mirrors established pattern at rio-controller/src/main.rs:316 where wp_controller uses .owns(stses) — comment at :304-308 documents exactly… |
| `wps-cutoff-learning-optionality` | P0232 | config-default | single-plan | #[serde(default, skip_serializing_if = "Option::is_none")] pub cutoff_learning: Option<Cu… | Belt-and-suspenders: Option<> wrapper + `enabled: bool` inside gives three states (None = unconfigured, Some({enabled:false}) = paused with… |
| `wps-rbac-least-privilege` | P0235 | api-shape | single-plan | RBAC ClusterRole: workerpoolsets [get, list, watch, patch, update]; workerpoolsets/status… | Core claim verified: P0233's reconciler only SSA-applies child WorkerPools and explicitly deletes child WPs in cleanup (lines 113-119) — it… |

---

## Non-decisions + already-decided

### Already-decided (17 groups)

These were flagged by scanners but turn out to restate prior locked decisions (spec already mandates, or prior plan already fixed).

| Key | Plans | Category | Decision | Reasoning |
|---|---|---|---|---|
| `anon-narinfo-visibility` | P0272 | fallback | When auth present, narinfo endpoint JOINs `path_tenants WHERE tenant_id = auth.… | Anonymous-unfiltered is safe because AuthenticatedTenant(None) only reaches the handler when operator explicitly set al… |
| `ca-cutoff-never-kill-running` | P0252 | fallback | **Running builds are NEVER killed.** Per `r[sched.preempt.never-running]`, cuto… | Not a new decision — r[sched.preempt.never-running] at docs/src/components/scheduler.md:246-247 already explicitly stat… |
| `chunks-created-at-exists` | P0262 | assumption | **Verify at dispatch:** `grep 'created_at' migrations/*.sql \| grep chunks` — if… | The column DOES exist (migrations/002_store.sql:91) — but the grep as written returns empty because `created_at` and `c… |
| `condition-last-transition-time-semantics` | P0238 | api-shape | **lastTransitionTime subtlety:** the above sets it every time. If that's too no… | This question is already answered in the codebase: rio-controller/src/reconcilers/build.rs:1082-1109 has `set_condition… |
| `controller-ctx-has-admin` | P0234 | assumption | let admin = ctx.admin_client(); // check: does controller have this? §8 hidden-… | Verified: reconcilers/mod.rs:42 defines `pub admin: rio_proto::AdminServiceClient<tonic::transport::Channel>` on Ctx. C… |
| `cpu-bump-tracey-marker-family` | P0230 | data-model | **GT12 — MARKER RENAME:** phase doc says `sched.rebalancer.cpu-bump`. The exist… | Verified: scheduler.md:185,192,202 has `sched.classify.{smallest-covering,mem-bump,penalty-overwrite}`; `sched.rebalanc… |
| `crd-types-crate-home` | P0237 | dep-choice | `rio-cli` needs `rio-controller` and `kube` as deps. If `rio-controller` isn't … | Resolved 2026-03-18 in .claude/notes/plan-adjustments-2026-03-18.md §rio-crds: shared-crate alternative chosen as `rio-… |
| `graph-degrade-fallback-shape` | P0280 | fallback | {#if layout?.degraded} <GraphTable nodes={...} /> <!-- sortable table, failed/p… | The fallback SHAPE is normatively specified: P0245:126 spec text reads 'MUST degrade to a sortable table'. P0280 is imp… |
| `graph-degrade-threshold` | P0280 | threshold | `>2000` nodes → degrade to sortable table (`r[dash.graph.degrade-threshold]` is… | This is no longer P0280's decision to make — it's normative spec. P0245:124-126 seeds `r[dash.graph.degrade-threshold]`… |
| `jti-transport-proto-vs-header` | P0258 | api-shape | `SubmitBuildRequest` gets a `jwt_jti: Option<String>` field populated from `ses… | USER-ratified: phase5-partition.md:18 explicitly resolves Q4 as 'Gateway just forwards jti claim in SubmitBuildRequest.… |
| `jwt-claim-shape` | P0245 | api-shape | JWT claims: `sub` = tenant_id UUID (server-resolved at mint time), `iat`, `exp`… | multi-tenancy.md:27 already specifies jti for revocation; :28 already specifies ed25519; :31 already specifies ConfigMa… |
| `nochroot-reject-layer` | P0226 | api-shape | The gateway MUST reject any derivation (at SubmitBuild time) whose env contains… | All four factual claims verified: validate_dag iterates nodes→drv_cache→env check (translate.rs:317-337, skips uncached… |
| `size-class-cpu-limit-field-shape` | P0230 | data-model | pub cpu_limit_cores: Option<f64>, — CPU limit for this class (cores). If ema_pe… | Type-asymmetric with `mem_limit_bytes: u64` (assignment.rs:62, uses `u64::MAX` sentinel per :61 doc comment) but justif… |
| `tenant-retention-floor-not-replace` | P0207 | fallback | Global grace (narinfo.created_at) is a floor; this extends, never shortens. | Structurally a non-decision: seed (c) at mark.rs:61-62 stays in place; the new seed is UNIONed alongside it. UNION is a… |
| `tenant-retention-union-semantics` | P0207 | data-model | Union-of-retention semantics — the most generous tenant wins. | Already normative spec: docs/src/components/store.md:203-204 under r[store.gc.tenant-retention] says this verbatim (P02… |
| `tenant-sign-key-fallback` | P0245 | fallback | narinfo signing MUST use the tenant's active signing key from `tenant_keys` whe… | multi-tenancy.md:64-66 already states 'per-tenant signing keys are unimplemented. A single cluster-wide ed25519 key sig… |
| `wps-seccomp-dep-sequencing` | P0232 | scope-boundary | **`PoolTemplate` consciously inherits `seccomp_profile`** from [P0223]. The dep… | Verified P0223 status=UNIMPL in dag.jsonl and `grep SeccompProfileKind rio-controller/src/` returns nothing. However th… |

### Non-decisions (5 groups)

Scanner false-positives — descriptive text, not normative choices.

| Key | Plans | Category | Text | Why non-decision |
|---|---|---|---|---|
| `buildstatus-conditions-field-exists` | P0238 | assumption | if `BuildStatus` doesn't already have a `conditions: Vec<Condition>` field with… | Verified: rio-controller/src/crds/build.rs:122-128 already has `conditions: Vec<Condition>` with k8s_openapi's standard… |
| `graph-edge-is-cutoff-field` | P0276 | data-model | bool is_cutoff = 3; // phase5 core wires this (P0252 Skipped cascade) | Column already exists at migrations/001_scheduler.sql:68 `is_cutoff BOOLEAN NOT NULL DEFAULT FALSE`. T3's SELECT alread… |
| `heartbeat-store-degraded-wire-default` | P0205 | config-default | Wire-compatible: default `false`, old workers don't send it, old schedulers rea… | Not a decision — a proto3 consequence. types.proto:1 confirms proto3; scalar bool has implicit default false, omitted f… |
| `sqlx-migration-immutability` | P0227 | scope-boundary | **NEVER append to 009.** The stale comment at `migrations/009_phase4.sql:8` ("P… | Verified: 009_phase4.sql:8 has the stale comment, sqlx::migrate! at main.rs:217 enforces SHA-384 checksums, and 010/011… |
| `x509-parse-crate` | P0224 | dep-choice | SAN DNSName check via [`x509-parser`](https://docs.rs/x509-parser) | Not a real decision: x509-parser is already the crate parsing CN at put_path.rs:44-46 (fn cert_cn uses X509Certificate:… |

---

## Skipped (not assessed)

### Already user-validated (62)

Category histogram:

| Category | Count |
|---|---|
| scope-boundary | 23 |
| data-model | 8 |
| api-shape | 7 |
| library-choice | 6 |
| config-default | 6 |
| threshold | 5 |
| fallback | 4 |
| dep-choice | 3 |

### Trivial reversibility (139)

Category histogram — these are one-line config/threshold changes, batch-reviewable if needed:

| Category | Count |
|---|---|
| threshold | 40 |
| fallback | 25 |
| scope-boundary | 18 |
| config-default | 16 |
| assumption | 15 |
| api-shape | 9 |
| dep-choice | 8 |
| library-choice | 5 |
| data-model | 3 |

---

## Coordinator prompt batches

Grouped for `AskUserQuestion` — 3-4 decisions per batch.

### Batch A — proto/migration (hardest to undo)

**A1** (4 questions):

- **`client-chunked-upload-finalize-api`** (proto-field, P0263)
  - Q: What shape should `client-chunked-upload-finalize-api` take on the wire/API?
  - Default: // 4. PutPath with manifest-only mode (chunk list, not raw NAR). store_client.put_path_manifest(PutPathManifestRequest { store_path, chunk_…
  - Alt: Extend PutPathRequest oneof with a 4th variant `ChunkManifest manifest = 4` (re… — Reuses ALL 688 lines of put_path.rs plumbing (HMAC verify, idempotency fast-path, WAL placeholder, …
  - Alt: No proto change: worker calls FindMissingPaths first (already exists, store.pro… — Zero proto work, zero store handler work. Idempotency fast-path (r[store.put.idempotent]) already h…
  - Alt: New dedicated unary `rpc PutPathManifest(PutPathManifestRequest)` on StoreServi… — Clean separation, simple unary call, no stream state machine. But duplicates ~300 lines of put_path…

- **`jti-forward-mechanism`** (proto-field, P0245)
  - Q: What shape should `jti-forward-mechanism` take on the wire/API?
  - Default: The gateway forwards `jti` in `SubmitBuildRequest.jwt_jti` so the scheduler can check revocation.
  - Alt: Scheduler reads jti from Claims extension (no proto field) — Zero wire change, single source of truth. The interceptor already does the JWT parse. Strictly bett…
  - Alt: Keep the proto field but rename intent: jti persisted with build row for audit … — If the real goal is audit (which jti issued this build?), a body field that gets INSERTed to PG mak…
  - Alt: Forward full JWT in a proto bytes field — Even more redundant; the header already carries it.

- **`pagination-cursor-encoding`** (proto-field, P0271)
  - Q: What shape should `pagination-cursor-encoding` take on the wire/API?
  - Default: optional string cursor = <next>; // opaque — submitted_at timestamp as RFC3339
  - Alt: base64(version_byte || submitted_at_micros_i64_be || build_id_bytes) — Actually opaque; version prefix lets you change encoding later without breaking old cursors; room f…
  - Alt: Epoch-microseconds as stringified i64 — Matches existing TenantRow.created_at pattern at db.rs:146/154 — no new dep, trivial parse. But sti…
  - Alt: RFC3339 as planned + add chrono dep — Human-readable in curl output (nice for admin API debugging). But locks proto to single-column curs…

- **`chunk-upsert-inserted-fallback`** (migration, P0208)
  - Q: What's the fallback behavior for `chunk-upsert-inserted-fallback`?
  - Default: **Fallback if xmax semantics are surprising:** add `updated_at TIMESTAMPTZ DEFAULT now()` column via migration 013, upsert bumps it with `O…
  - Alt: RETURNING (refcount = 1) AS inserted — as the fallback (or primary) — Zero migrations, zero drain changes. Same single-statement shape as xmax. Only 'risk' is that it co…
  - Alt: Corrected updated_at fallback: add created_at + updated_at, RETURNING (updated_… — Actually fixes the upload race (atomic with INSERT like xmax). Needs migration 012 + two columns. H…
  - Alt: Keep plan's drain-side updated_at check as belt-and-suspenders ONLY, paired wit… — Defense in depth. But drain.rs:111-131 already has still_dead — another layer is diminishing return…

**A2** (3 questions):

- **`chunks-tenant-ownership-model`** (migration, P0264)
  - Q: How should we model `chunks-tenant-ownership-model`?
  - Default: ALTER TABLE chunks ADD COLUMN tenant_id UUID NULL REFERENCES tenants(tenant_id); CREATE INDEX chunks_tenant_idx ON chunks(tenant_id, chunk_…
  - Alt: Junction table: chunk_tenants(blake3_hash, tenant_id) with PK on both — Correctly models many-to-many; PutChunk does INSERT ON CONFLICT DO NOTHING into junction; FindMissi…
  - Alt: Keep single column but define 'first uploader wins' + ON CONFLICT DO NOTHING on… — Simple schema. But tenant B uploads bytes, gets told 'missing' next time (chunk is tagged A), re-up…
  - Alt: Array column: tenant_ids UUID[] with GIN index — No join, single-row update on conflict. But array-append under concurrent ON CONFLICT is racy witho…

- **`poison-nondistinct-counter`** (migration, P0219)
  - Q: How should we model `poison-nondistinct-counter`?
  - Default: **`require_distinct_workers = false` semantics:** `failed_workers` stays a `HashSet` (for `best_worker` exclusion) but the poison check bec…
  - Alt: Reuse existing `retry_count` — no new field, no migration. Check: `(s.retry_cou… — Zero schema change. Couples poison-threshold to max_retries semantics (both count the same thing). …
  - Alt: In-memory-only `failure_count` (don't persist) — No migration. Scheduler restart loses the count — derivation gets N more retries post-restart. For …
  - Alt: Change `failed_workers: HashSet` → `Vec<WorkerId>` and derive both distinct-cou… — Single source of truth, no separate counter. But changes the PG type contract — migrations/004_reco…

- **`realisation-deps-fk-target`** (migration, P0249)
  - Q: How should we model `realisation-deps-fk-target`?
  - Default: realisation_id BIGINT NOT NULL REFERENCES realisations(id) ON DELETE CASCADE, dep_realisation_id BIGINT NOT NULL REFERENCES realisations(id…
  - Alt: Prepend ALTER TABLE realisations ADD COLUMN id BIGSERIAL UNIQUE to migration 015 — Junction rows stay compact (16 bytes), cheap joins; but adds a second key to realisations alongside…
  - Alt: Use composite FK: (drv_hash BYTEA, output_name TEXT) × 2 sides, PRIMARY KEY on … — No ALTER to realisations, matches exactly how opcodes_read.rs and realisations.rs already identify …
  - Alt: ON DELETE RESTRICT instead of CASCADE — Orthogonal to the FK-target bug; protects against accidental realisation deletes orphaning deps, bu…

### Batch B — spec-markers + multi-plan cascades

**B1** (4 questions):

- **`chunk-upsert-inserted-detect`** (spec-marker, P0208)
  - Q: How should we model `chunk-upsert-inserted-detect`?
  - Default: The fix: `RETURNING blake3_hash, (xmax = 0) AS inserted` — atomic with the INSERT. Changes the return type `Result<()>` → `Result<HashSet<V…
  - Alt: RETURNING blake3_hash, (refcount = 1) AS inserted — Simpler — relies only on documented column semantics, not xmax implementation detail. Correctly rep…
  - Alt: INSERT ... ON CONFLICT DO NOTHING RETURNING blake3_hash, then separate UPDATE f… — DO NOTHING + RETURNING only returns inserted rows — exactly the set you want, no filtering. But req…
  - Alt: xmax = 0 (plan's primary) — Well-trodden idiom, one statement. Relies on undocumented-but-stable PG internals. Reports inserted…

- **`jwt-interceptor-service-scope`** (spec-marker, P0259)
  - Q: Where's the scope boundary for `jwt-interceptor-service-scope`?
  - Default: tonic interceptor: extract `x-rio-tenant-token`, verify signature+expiry, attach `Claims` to request extensions. **Wired in THREE `main.rs`…
  - Alt: Scope to 2 services (scheduler + store); amend P0245's r[gw.jwt.verify] spec te… — Requires tracey bump on r[gw.jwt.verify] since spec text changes. If P0245 not yet merged, fix ther…
  - Alt: Add client-side token-attach interceptor for controller's outgoing gRPC calls (… — Different direction (attach not verify) and different concern — controller runs in-cluster with Ser…
  - Alt: Keep '3' aspirationally, add a gRPC admin endpoint to controller that needs pro… — Inventing work to satisfy a miscounted exit criterion. Controller's admin surface is the k8s API (C…

- **`k8s-finalizer-naming-convention`** (spec-marker, P0233)
  - Q: What shape should `k8s-finalizer-naming-convention` take on the wire/API?
  - Default: pub const MANAGER: &str = "rio-controller-wps"; pub const FINALIZER: &str = "workerpoolset.rio.build/finalizer";
  - Alt: FINALIZER = "rio.build/workerpoolset-cleanup" — consistent with the 2 existing finalizers (grep `rio.build/` finds all 3); slightly less Kubebuilde…
  - Alt: FINALIZER = "rio.build/wps-cleanup" — shortest, matches the MANAGER's `-wps` abbreviation; risk that `wps` is opaque to an operator readi…
  - Alt: keep as proposed + retrofit existing 2 finalizers to the new style — live-object migration needed (finalizers are stored in etcd on every object with that CRD; renaming…

- **`rate-key-spec-marker-update`** (spec-marker, P0261)
  - Q: Is the `rate-key-spec-marker-update` assumption valid?
  - Default: none — this is internal resilience. `r[gw.rate.per-tenant]` exists at gateway.md:657 but this plan doesn't change its contract (rate-limiti…
  - Alt: Update gateway.md:659-668 text to describe jti keying + LRU as current behavior… — Marker ID 'per-tenant' becomes a misnomer but IDs are opaque to tracey. P0213's r[impl] annotation …
  - Alt: Rename marker to r[gw.rate.per-session] (or r[gw.rate.keyed]), update text, re-… — Semantically cleanest. But if P0213 already landed an r[impl gw.rate.per-tenant] annotation, that b…
  - Alt: Leave spec as-is (plan's current choice) — Spec text says 'keyed on tenant_name' while code keys on jti — exactly the drift CLAUDE.md's 'Keepi…

**B2** (4 questions):

- **`seccomp-default-action-layering`** (spec-marker, P0223)
  - Q: What's the right default for `seccomp-default-action-layering`?
  - Default: "defaultAction": "SCMP_ACT_ALLOW" ... This is a **denylist on top of RuntimeDefault** — `defaultAction: ALLOW` because we layer on what Run…
  - Alt: Clone containerd's RuntimeDefault JSON (~350 syscalls allowlist, defaultAction:… — Correct 'on top of' semantics; ~350 lines but zero maintenance since it's copy-paste from upstream;…
  - Alt: defaultAction:SCMP_ACT_ERRNO + hand-curated allowlist per ADR-012:14 (mount, un… — Tightest security posture and matches the ADR; high risk of EPERM crashes on missed syscalls (glibc…
  - Alt: Keep defaultAction:ALLOW but rewrite spec to say 'trades ~40 RuntimeDefault blo… — Honest but indefensible — kexec_load/open_by_handle_at are higher-value attacker targets than ptrac…

- **`store-gc-sweep-metric-names`** (spec-marker, P0212)
  - Q: What shape should `store-gc-sweep-metric-names` take on the wire/API?
  - Default: metrics::counter!("rio_store_gc_paths_swept_total").increment(stats.paths_deleted as u64); metrics::counter!("rio_store_gc_chunks_enqueued_…
  - Alt: rio_store_gc_path_swept_total ← stats.paths_deleted; rio_store_gc_s3_key_enqueu… — Consistent with existing path_resurrected/chunk_resurrected naming; uses actual GcStats field names…
  - Alt: rio_store_gc_path_deleted_total (mirror the field name paths_deleted exactly in… — 'swept' is GC jargon, 'deleted' is what actually happened to narinfo rows; easier to correlate with…
  - Alt: Emit all four non-resurrected GcStats fields (paths_deleted, chunks_deleted, s3… — Complete observability of GC output; bytes_freed especially useful for capacity dashboards. Four de…

- **`wps-autoscale-target-source`** (spec-marker, P0234)
  - Q: What's the right threshold/limit for `wps-autoscale-target-source`?
  - Default: compute `desired = clamp(queued / target_queue_per_replica, min_replicas, max_replicas)` — autoscale formula is queue-depth-proportional (t…
  - Alt: Add `target_queue_per_replica: Option<i32>` (default 5) to SizeClassSpec in P02… — Clean per-class tuning (small builds may want 10/replica, large builds 2/replica). Requires P0232 C…
  - Alt: Read target from the child WorkerPool's `spec.autoscaling.target_value` (set by… — Keeps SizeClassSpec lean; indirects through the child. Requires an extra GET per class (or list). P…
  - Alt: Single WPS-level `spec.autoscaling.target_value` shared across all classes — Simplest CRD shape. Loses per-class tuning — the whole point of size classes is that small vs large…

- **`build-condition-event-mapping`** (multi-plan, P0238)
  - Q: How should we model `build-condition-event-mapping`?
  - Default: BuildEvent::Scheduled { .. } => vec![build_condition("Scheduled", "True", "Scheduled", "build assigned to scheduler")], BuildEvent::InputsR…
  - Alt: Map BuildEv::Started→Scheduled, first Progress with running>0→Building, DROP In… — Loses one of three spec'd conditions but avoids proto changes. InputsResolved is the least actionab…
  - Alt: Add BuildEvent::InputsResolved to types.proto — scheduler fires after closure s… — Proto-field blast radius: scheduler must be updated to emit it, wire compat concern, scope bleed in…
  - Alt: Derive InputsResolved from Progress.queued==0 && completed==0 heuristic — Fragile — queued may drop to 0 for reasons other than input resolution; a single-derivation build n…

**B3** (4 questions):

- **`buildstatus-critpath-workers-shape`** (multi-plan, P0270)
  - Q: What shape should `buildstatus-critpath-workers-shape` take on the wire/API?
  - Default: // Estimated seconds remaining on the critical path (longest chain of incomplete derivations weighted by ema_duration_secs). optional uint6…
  - Alt: CRD struct fields: `Option<i64> critical_path_remaining_secs` + `Vec<String> as… — Most consistent with P0238 context and controller's apply_event flow; loses the P0271 parallel-safe…
  - Alt: types.proto BuildStatus (QueryBuildStatus RPC) + CRD mirror — Dashboard could also read via gRPC-Web (admin.proto:9 mentions tonic-web), but doubles the field su…
  - Alt: Structured assignments: `Vec<WorkerAssignment { worker_id, drv_path }>` instead… — Richer for debugging 'which worker is building what', but heavier CRD status churn on every Derivat…

- **`ca-cutoff-multi-output-granularity`** (multi-plan, P0251)
  - Q: How should we model `ca-cutoff-multi-output-granularity`?
  - Default: // MVP: single bool. Multi-output CA is rare; per-output map is a followup if the VM test (P0254) shows it matters. state.ca_output_unchang…
  - Alt: Single bool with AND-fold: compute all queries, then `state.ca_output_unchanged… — Same data model (one bool), same field, zero extra complexity, correct for all output counts. Cutof…
  - Alt: HashMap<OutputName, bool> on DerivationState — Correct and maximally granular, but P0252's find_cutoff_eligible would need to know which output ea…
  - Alt: Short-circuit on first miss: break out of loop on !matched, set false — Correct and saves RPCs on first miss. Loses per-output metric granularity (can't distinguish 'all m…

- **`crd-k8s-type-passthrough`** (multi-plan, P0232)
  - Q: How should we model `crd-k8s-type-passthrough`?
  - Default: #[schemars(schema_with = "any_object")] // passthrough: k8s ResourceRequirements pub resources: serde_json::Value, — avoids duplicating k8s…
  - Alt: pub resources: ResourceRequirements + schema_with=any_object (non-Option) — Matches workerpool.rs pattern for schema, keeps Rust type safety, P0233 builder works with `Some(cl…
  - Alt: pub resources: serde_json::Value (plan's choice) — Schema output is identical (any_object controls that), but loses Rust-side type safety and forces P…
  - Alt: pub resources: Option<ResourceRequirements> + schema_with=any_object — Exact copy of workerpool.rs:70. Allows omitting resources per-class (cluster default); but makes 's…

- **`decision-gate-outcome-channel`** (multi-plan, P0241)
  - Q: Where's the scope boundary for `decision-gate-outcome-channel`?
  - Default: **Hidden check at dispatch:** `jq 'select(.origin=="P0220")' .claude/state/followups-pending.jsonl` — read the recorded outcome. If `needs-…
  - Alt: Fix jq to `select(.source_plan=="P0220")` and encode outcome in `description` s… — Zero schema change, works today. Stringly-typed: fragile to description rewording, no structured fi…
  - Alt: Add `payload: dict[str, Any] | None = None` to Followup model in state.py — One-line schema change makes Followup a proper decision-carrier for ALL future gate plans. Touches …
  - Alt: Skip followups sink — P0220 already annotates docs/src/phases/phase4c.md:41 wit… — No JSONL, no schema, no state.py edit. But doc-grep is fragile to rewording and loses the structure…

**B4** (4 questions):

- **`grpc-web-transport-flavor`** (multi-plan, P0277)
  - Q: What's the right default for `grpc-web-transport-flavor`?
  - Default: export const transport = createConnectTransport({ baseUrl: "/", useBinaryFormat: true });
  - Alt: createGrpcWebTransport({ baseUrl: "/", useBinaryFormat: true }) — Correct — matches Envoy grpc_web filter and P0273's application/grpc-web+proto curl validation. Bin…
  - Alt: createGrpcWebTransport({ baseUrl: "/", useBinaryFormat: false }) — grpc-web-text (base64). ~33% payload bloat. Only needed for browsers lacking fetch body streaming —…
  - Alt: Keep createConnectTransport, add Connect-protocol termination in Envoy — Not possible — Envoy has no Connect-protocol filter. Would require replacing Envoy with a Connect-n…

- **`jwt-pubkey-hotswap-shape`** (multi-plan, P0260)
  - Q: What shape should `jwt-pubkey-hotswap-shape` take on the wire/API?
  - Default: extend the existing `shutdown_signal` pattern with a SIGHUP handler that re-reads the pubkey ConfigMap mount. The interceptor from P0259 re…
  - Alt: Amend P0259 pre-merge to take Arc<RwLock<VerifyingKey>> from the start — Cleanest — avoids churning just-landed code. P0259 is still UNIMPL (hard dep of P0260), so the sign…
  - Alt: tokio::sync::watch::Receiver<VerifyingKey> instead of Arc<RwLock> — Zero new deps (tokio already present). receiver.borrow() in interceptor, SIGHUP handler calls tx.se…
  - Alt: arc-swap::ArcSwap<VerifyingKey> — Lock-free reads, the canonical crate for read-heavy/write-rare config reload. New dep (tiny, well-a…

- **`sita-degenerate-cutoffs`** (multi-plan, P0229)
  - Q: What's the fallback behavior for `sita-degenerate-cutoffs`?
  - Default: All durations identical → cutoffs degenerate (all boundaries at the same value). Handle: dedupe adjacent equal cutoffs OR clamp to `[prev-ε…
  - Alt: No-op: return equal cutoffs unchanged (what the test already asserts) — Preserves Vec length invariant so P0230's zip works. classify() routes everything to first-matching…
  - Alt: Dedupe adjacent equal cutoffs — BREAKS P0230. zip truncates silently — if N-1 cutoffs dedupe to 1, only class[0] gets updated, clas…
  - Alt: Clamp to [prev-ε, prev+ε] — Preserves length, but meaningless on first run (no prev — plan:76 returns raw unsmoothed). On subse…

- **`wps-classstatus-fields`** (multi-plan, P0232)
  - Q: How should we model `wps-classstatus-fields`?
  - Default: pub struct ClassStatus { pub name: String, pub effective_cutoff_secs: f64, pub queued: u64, pub child_pool: String, pub replicas: i32 } — s…
  - Alt: Add ready_replicas: i32 (6 fields total) — Matches spec + keeps child_pool. P0234 fetches it from child WorkerPool.status alongside replicas —…
  - Alt: Plan's 5 fields as-is — Spec-divergent. `kubectl describe wps` can't show Ready-vs-Desired without a separate `kubectl get …
  - Alt: Spec's 5 fields verbatim (drop child_pool) — Loses the WPS→WorkerPool name link. P0237 CLI would have to recompute `format!("{}-{}", ...)` clien…

### Batch C — single-plan moderate-impact

**C1** (4 questions):

- **`ca-skipped-source-states`** (single-plan, P0252)
  - Q: How should we model `ca-skipped-source-states`?
  - Default: (Queued, Skipped) => Ok(()), // CA cutoff — only Queued can skip
  - Alt: Also allow (Ready, Skipped) — Matches DependencyFailed cascade precedent; order-independent vs find_newly_ready(); Ready means no…
  - Alt: Also allow (Created, Skipped) — Covers pre-queue nodes; matches full DependencyFailed precedent; but Created→Completed cache-hit pa…
  - Alt: Queued-only + enforce cascade runs BEFORE find_newly_ready() — Keeps state machine minimal but couples correctness to call ordering in completion.rs — fragile acr…

- **`envoy-image-pinning`** (single-plan, P0282)
  - Q: Which dependency for `envoy-image-pinning`?
  - Default: envoyImage: envoyproxy/envoy:v1.29-latest
  - Alt: Pinned patch version (e.g., envoyproxy/envoy:v1.29.5) matching whatever digest … — Requires manual bump for CVEs, but reproducible, consistent across nodes, and matches the VM-test i…
  - Alt: Pinned digest (envoyproxy/envoy@sha256:...) — Strongest reproducibility and exactly matches docker-pulled.nix convention. Ugly in values.yaml, tw…
  - Alt: Keep v1.X-latest but set imagePullPolicy: Always on the envoy container only — Fixes node-skew (all nodes converge on restart) and actually delivers the 'auto security patches' b…

- **`keyset-cursor-tiebreaker`** (single-plan, P0271)
  - Q: How should we model `keyset-cursor-tiebreaker`?
  - Default: "SELECT * FROM builds WHERE submitted_at < $1 ORDER BY submitted_at DESC LIMIT $2" ... let next_cursor = builds.last().map(|b| b.submitted_…
  - Alt: Compound keyset: WHERE (b.submitted_at, b.build_id) < ($1, $2) with row-value c… — Postgres supports row-value < natively. Matches existing ORDER BY at db.rs:283. Cursor must encode …
  - Alt: Single-column + document the limitation — T3's 'no duplicates, no gaps' assertion becomes false advertising. Acceptable for a low-stakes admi…
  - Alt: Force unique submitted_at via CREATE UNIQUE INDEX ON builds (submitted_at) — Turns a pagination glitch into INSERT failures under concurrency. Strictly worse.

- **`run-gc-extracted-signature`** (single-plan, P0212)
  - Q: What shape should `run-gc-extracted-signature` take on the wire/API?
  - Default: pub async fn run_gc(pool: &PgPool, backend: Arc<dyn ChunkBackend>, dry_run: bool, progress_tx: mpsc::Sender<GcProgress>) -> Result<GcStats>
  - Alt: Add grace_hours: u32, extra_roots: &[String] as positional params — Minimal change, preserves gRPC API fidelity; cron caller passes grace_hours=2, extra_roots=&[]. Six…
  - Alt: Bundle into struct GcParams { dry_run, grace_hours, extra_roots } passed by ref — More extensible if future GC knobs appear; slightly more ceremony (new type) for only 3 fields today
  - Alt: Keep signature as-is, hardcode grace_hours=2 + empty extra_roots inside run_gc — Breaks existing gRPC TriggerGC semantics — user-provided extra_roots silently ignored, grace_period…

**C2** (2 questions):

- **`ssa-field-manager-conditions`** (single-plan, P0238)
  - Q: What shape should `ssa-field-manager-conditions` take on the wire/API?
  - Default: &PatchParams::apply("rio-controller-build-conditions").force()
  - Alt: Reuse MANAGER="rio-controller" + existing patch_status() — route through apply_… — Single field manager owns status.conditions; no clobbering. Minor con: one manager for all status w…
  - Alt: Separate manager but ONLY write via set_condition into a full BuildStatus and h… — Requires refactoring existing Failed/Cancelled arms to use the new manager too. Pointless churn — n…

- **`tenant-scope-fail-open-vs-closed`** (single-plan, P0264)
  - Q: What's the fallback behavior for `tenant-scope-fail-open-vs-closed`?
  - Default: let where_clause = match tenant_id { Some(tid) => "WHERE tenant_id = $1 OR tenant_id IS NULL", // shared chunks visible None => "", // anon…
  - Alt: Fail-closed: None => return Err(Status::unauthenticated("tenant scoping require… — Matches security posture. If P0259 interceptor is truly global, this branch is unreachable anyway —…
  - Alt: Feature-flag the scoping (config bool tenant_scoped_chunks) — Single-tenant deployments set it false and get the old behavior; multi-tenant sets true and require…
  - Alt: Keep as written — fail-open for backward-compat — Only defensible if store has legitimate unauth'd callers post-P0259. Plan doesn't identify any. Sil…

### Batch D — confirm groups worth a quick ack (top by blast)

**D1** (4 questions):

- **`ca-detect-field-shape`** (proto-field, P0248)
  - Q: What shape should `ca-detect-field-shape` take on the wire/API?
  - Default: // Phase 5 CA cutoff: set by gateway translate.rs from has_ca_floating_outputs() || is_fixed_output(). Scheduler uses this to gate the hash…
  - Alt: enum CaKind { INPUT_ADDRESSED=0; FIXED_OUTPUT=1; FLOATING_CA=2 } — More expressive but YAGNI — no consumer branches on the 3-way split. Proto3 enum default (=0=INPUT_…
  - Alt: add only `bool has_ca_floating_outputs` — scheduler ORs with existing `is_fixed… — Avoids semantic redundancy (for FOD, both the new bool and field 7 would be true) but pushes the CA…
  - Alt: no new field — gateway sets existing `is_fixed_output=7` to true for floating-C… — Zero proto churn but semantically wrong: WorkAssignment:304 uses `is_fixed_output` to grant network…

- **`graph-node-cap`** (proto-field, P0245)
  - Q: What's the right threshold/limit for `graph-node-cap`?
  - Default: Graph rendering MUST degrade to a sortable table when the node count exceeds 2000. dagre layout on >2000 nodes freezes the main thread. Abo…
  - Alt: Virtualized graph rendering (only layout visible viewport) — Lets you render 50k+ nodes but @xyflow doesn't support this natively; major custom work. Table-degr…
  - Alt: Server-side layout (send x,y coords in proto) — Moves the CPU to scheduler which has other things to do; loses client-side re-layout on resize/filt…
  - Alt: Single threshold (just 2000→table, no Worker tier) — Simpler but 500-2000 node graphs would jank the main thread during layout. The Worker tier is worth…

- **`graph-rpc-thin-vs-fat-node`** (proto-field, P0276)
  - Q: How should we model `graph-rpc-thin-vs-fat-node`?
  - Default: Does NOT reuse `DerivationNode` — that carries `bytes drv_content` (types.proto:32, ≤64KB/node). 2000 nodes × 64KB = 128MB > `max_encoding_…
  - Alt: Mark drv_content optional on DerivationNode, leave empty for graph RPC — Pollutes DerivationNode with dual semantics (submit-time vs query-time); still ships 5 irrelevant f…
  - Alt: Server-streaming `stream GraphNode` instead of unary response — Unbounded but P0280:84 polls at 5s intervals — streaming is mismatched to poll-and-diff; grpc-web (…
  - Alt: Paginated fetch with cursor — Adds stateful cursor management; P0245:126 already degrades viz to a table above 2000 nodes so the …

- **`heartbeat-store-degraded-field-type`** (proto-field, P0205)
  - Q: What shape should `heartbeat-store-degraded-field-type` take on the wire/API?
  - Default: bool store_degraded = 9;
  - Alt: enum StoreHealth { HEALTHY=0; DEGRADED=1; } store_health = 9; — Future-proofs for richer states (SLOW, PARTIAL). But scheduler consumes it identically to bool toda…
  - Alt: uint32 consecutive_fuse_failures = 9; — Expose raw counter, let scheduler apply threshold. Pushes policy to wrong layer — worker owns the b…
  - Alt: optional bool store_degraded = 9; — proto3 explicit-presence. Scheduler could distinguish 'old worker, field absent' from 'new worker, …

**D2** (2 questions):

- **`proto-status-string-vs-enum`** (proto-field, P0276)
  - Q: How should we model `proto-status-string-vs-enum`?
  - Default: string status = 4; // derivations.status CHECK constraint values
  - Alt: New `enum DerivationStatus` with 9 variants mirroring the CHECK constraint — Diverges from types.proto:686/694 precedent; P0252 adding 'skipped' becomes a proto-wire change (ol…
  - Alt: Reuse existing BuildResultStatus enum (types.proto:262) — Wrong semantics — BuildResultStatus is per-attempt Nix build outcome (BUILT/TIMED_OUT/etc), this is…

- **`sizeclass-status-proto-shape`** (proto-field, P0231)
  - Q: What shape should `sizeclass-status-proto-shape` take on the wire/API?
  - Default: message SizeClassStatus { string name = 1; double effective_cutoff_secs = 2; double configured_cutoff_secs = 3; uint64 queued = 4; uint64 r…
  - Alt: Use `uint32` for queued/running to match existing convention (types.proto:87-89… — More consistent with codebase; actor/mod.rs:798 already documents the 4B bound as 'LEAST of our pro…
  - Alt: Single `cutoff_secs` field + separate `drift_secs` delta instead of effective+c… — Smaller wire footprint; CLI can't show 'configured → effective' arrow, just magnitude. Less self-do…
  - Alt: Fold into the existing ClusterSnapshot (command.rs:334) as a repeated per_class… — Reuses the existing polling path (controller already calls ClusterSnapshot) — fewer RPCs. But coupl…
