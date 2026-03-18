# Phase 4b Partition — Final Plan (P0204–P0219)

---

## §0. Assumptions & open questions — CONFIRM BEFORE `/plan`

| # | Assumption | Evidence | If wrong |
|---|---|---|---|
| **A1** | **Migration goes in NEW file `012_path_tenants.sql`, NOT appended to 009.** | `sqlx::migrate!` at `rio-store/src/lib.rs:34` is checksum-checked. Migrations 010+011 already exist after 009. Appending to 009 changes its checksum → `VersionMismatch` on any persistent DB (prod, staging, long-lived dev). Ephemeral test PG (rio-test-support) runs fresh so wouldn't catch this until deploy. The 009 header comment at line 7 ("Part C appended later") was the design intent BEFORE 010/011 landed — that intent is now overtaken. | If you prefer 009-append: every dev must `DROP DATABASE` + re-migrate. Acceptable cost only if zero persistent deployments exist. **Default: 012.** |
| **A2** | `tracey-validate` CI check passes with new uncovered markers present. | Verified: `tracey query status` shows 167/167 covered today; CI greps for `0 total error(s)` in `tracey query validate` output. Uncovered markers are NOT errors — they show in `tracey query uncovered` (a different query). P0204 adds 11 markers → 167/178 covered, 0 errors. | If CI actually gates on coverage %: P0204 must be split into per-plan marker additions. **Default: single upfront P0204.** |
| **A3** | `build.options.build_timeout` dual-use is correct semantics. | Proto comment at `types.proto:315` says "Max **total** build time in seconds" — the field's documented intent was ALWAYS per-build-overall. The current worker-side use at `actor/build.rs:567` (forwarded as per-derivation daemon timeout via `min_nonzero`) is an incidental defense-in-depth floor, not the primary semantics. P0214 implements the documented intent; the worker-side floor stays as belt-and-suspenders. | If you want strictly separate fields: P0214 becomes a proto-change plan (adds `overall_timeout_secs = 4` to `BuildOptions`), which serializes with P0205. **Default: dual-use, document clearly.** |
| **A4** | `BUILD_RESULT_STATUS_TIMED_OUT = 12` exists and is the correct status for both maxSilentTime and per-build timeout. | Verified at `types.proto:278` with doc-comment "Scheduler treats as permanent-no-reassign." | N/A — confirmed. |
| **A5** | `narinfo.tenant_id` drop is safe in the SAME migration as `path_tenants` create. | Zero Rust readers verified (grep across `rio-*/src/**/*.rs` for `narinfo.*tenant_id` returns empty). Column added at `migrations/002_store.sql:30`, never queried. The `cache_server/auth.rs:36` TODO reads `tenants.cache_token`, not `narinfo.tenant_id`. | If a hidden reader exists: migration fails fast at CI with a compile error. Low risk. |
| **A6** | Standalone `dockerImages.rio-cli` NOT in scope. | rio-cli already in scheduler image at `nix/docker.nix:177-181`; `kubectl exec` path works today. Laptop-direct use is nice-to-have. | If wanted: trivial 5-line add to P0216, no dep impact. |
| **A7** | 8 existing untested markers (167 covered − 159 verified = 8) are NOT in 4b scope. | Pre-existing gap; phase4b.md doesn't mention closing them. | If wanted: add P0220 "untested-marker sweep" as wave-3 filler. **Default: defer.** |

---

## §1. Ground-truth reconciliation vs. stale phase4b.md

**DONE — P0204 marks `[x]`, no plan:**

| phase4b.md claim | Reality | Evidence |
|---|---|---|
| NAR scanner critical, `upload.rs:223 sends Vec::new()` | **FALSE** — landed in 4a remediation | `r[impl worker.upload.references-scanned]` at `rio-worker/src/upload.rs:150`; RefScanSink (Boyer-Moore, NOT aho-corasick); `r[verify]` at `:799`; VM assertion `lifecycle.nix:1800-1811`; migrations 010+011 backfill |
| Proposed marker name `worker.refs.nar-scan` | Actual marker is `worker.upload.references-scanned` — do NOT rename code | `upload.rs:150` |
| Karpenter task open | **DONE** | `infra/eks/karpenter.tf` (79L) + `templates/karpenter.yaml` + `templates/workerpool.yaml` |
| rio-cli needs scaffolding | **PARTIAL** — 257L with CreateTenant/ListTenants/Status only | `rio-cli/src/main.rs:97-120` |
| rio-cli packaging | **DONE** — in scheduler image | `nix/docker.nix:177-181` |
| cli.nix needs creation | **EXISTS** — 140L | `nix/tests/scenarios/cli.nix` |

**Critical path shift:** `tenants.gc_retention_hours` (migration `009_phase4.sql:16`, default 168h) is DEAD DATA today. `rio-cli create-tenant --gc-retention-hours 720` sets it; `mark.rs` never reads it. **Every tenant gets the global grace regardless of their configured retention.** This replaces NAR-scanner as the correctness front.

---

## §2. Verified risks that change the partition shape

| Risk | Verification | Mitigation |
|---|---|---|
| **R-MIG** (fka R9): 009-append breaks checksum | `ls migrations/` shows 010+011 exist; `sqlx::migrate!` at `lib.rs:34` is checksum-checked | **P0206 uses `migrations/012_path_tenants.sql`.** P0204 updates 009 header comment? **NO** — even comment changes alter checksum. Leave 009 alone entirely; note the deviation in phase4b.md only. |
| **R-SYNC** (fka R3): FUSE callbacks are NOT tokio | `fetch.rs:268-269`: "SYNC with internal block_on — caller is either a FUSE thread (dedicated blocking) or spawn_blocking. **Never call from async.**" `ensure_cached` at `:54` is `fn` not `async fn`. | **P0209 circuit breaker uses `std::sync::atomic::AtomicU32` + `parking_lot::Mutex<Option<Instant>>` ONLY.** No `tokio::sync`. No `.await`. Pattern reference: `rio-scheduler/src/actor/breaker.rs` (3-state shape, but that one is single-threaded-actor so uses plain `u32` — FUSE needs atomics). |
| **R-TOUT** (fka R2): `build_timeout` already used per-derivation | `actor/build.rs:567` computes `min_nonzero` per-drv and sends to worker. Proto doc at `types.proto:315` says "Max **total** build time" — the per-drv use is incidental. | **P0214 uses the field as documented (per-build total).** Worker-side per-drv floor stays as defense-in-depth. Document both semantics in P0214. Not a bug — the field's intent was always "total." |
| **R-XMAX** (fka R1): batch RETURNING xmax untested idiom | `chunked.rs:120-127` is `.execute()`, returns `()`; needs `fetch_all` + `RETURNING (xmax = 0)`. First PG UNNEST-batch-RETURNING in codebase. | **P0208 includes a 30-minute psql spike BEFORE Rust:** `INSERT INTO t SELECT unnest(ARRAY[1,1,2]) ON CONFLICT DO UPDATE SET x=x RETURNING id, (xmax=0)` → verify per-row xmax. Fallback in plan doc: `RETURNING blake3_hash, (chunks.ctid IS NOT NULL AND refcount = 1)` or `updated_at` column approach. |
| **R-CONN**: controller GC cron connect-in-loop | `lang-gotchas.md`: "eager `.connect().await?` in main = process-exit"; tonic has no default connect timeout. | **P0212 connects fresh per-iteration INSIDE the loop** with `tokio::time::timeout(30s, connect_store_admin(...))`; on Err → `warn!` + increment `result=failure` counter + `continue`. Never `?`-propagate connect error out of the loop. |
| **R-GOVN**: governor key eviction | `DefaultKeyedRateLimiter` uses `dashmap`, no auto-eviction. | Acceptable for 4b: `tenant_name` comes from `authorized_keys` comment (operator-controlled at `server.rs:70`, can't be forged). Document in P0213; `TODO(phase5)` key-eviction. |

---

## §3. Wave structure

```
Wave 0 (serial prologue, ~1 merge cycle total — both plans are trivial):
  P0204 doc-sync+markers  →  P0205 proto-pin

Wave 1 (4-wide, file-disjoint — spike the hard parts):
  P0206 path_tenants migration+upsert ┐
  P0207 mark CTE tenant seed          ├─ all parallel
  P0208 xmax RETURNING refactor       │
  P0209 FUSE circuit breaker          ┘

Wave 2 (7-wide, 2 soft-serial pairs):
  P0210 heartbeat plumb      [needs P0205+P0209]
  P0211 sched consume        [needs P0205; soft-serialize w/ P0214 on actor/worker.rs]
  P0212 GC automation        [soft: gc/mod.rs w/ P0207]
  P0213 gateway ratelimit    [only Cargo.toml touch]
  P0214 per-build timeout    [soft-serialize w/ P0211]
  P0215 maxSilentTime        [soft: scheduling.nix w/ P0214]
  P0216 rio-cli complete     [fully disjoint]

Wave 3 (filler, any time after P0204):
  P0217 NormalizedName       [disjoint]
  P0218 nar_buffer config    [disjoint]
  P0219 per-worker failures  [soft: completion.rs w/ P0206]
```

**Why Wave 0 serial is cheap:** P0204 is doc-only (~minutes). P0205 is 1 proto line + 1 doc line + 1 default-roundtrip test (~30min). Total prologue cost ≈ 1 merge cycle. In return: every downstream plan references stable marker IDs, and the proto file (27 prior collisions) is locked before any consumer touches it.

**Why Wave 1 is 4-wide not wider:** These are the only plans where implementation surprises could reshape downstream plans. P0206's completion.rs call-site, P0207's empty-table UNION correctness, P0208's PG idiom, P0209's sync-only threading — each has a failure mode that would change wave-2 shape. Wave-3 fillers can start alongside wave-1 if coordinator has idle slots.

---

## §4. Partition table

### P0204 — phase4b doc-sync + 11 spec-marker seeding

| Field | Value |
|---|---|
| **Tasks** | **(1) phase4b.md corrections:** Mark `[x]` lines 13 (NAR scanner, ref commit 9165dc2), 39 (Karpenter), 40 (rio-cli packaging — scheduler-image only, strike standalone). Strike proposed marker `worker.refs.nar-scan` at line 64 → note actual is `worker.upload.references-scanned` (already impl+verify). Update line 22 "Migration 009 Part C" → "Migration 012 (was 009 Part C — see §0.A1: 010/011 now exist, checksum-locked)". Update Carried-forward table line 70: `upload.rs:223 Vec::new()` → resolved 4a remediation 02. **(2) Re-tag deferrals:** `rio-worker/src/upload.rs:166` `TODO(phase4b)` → `TODO(phase4c)` (trailer-refs — gated on "if pre-scan cost measurable", no evidence). `rio-scheduler/src/db.rs:9` `TODO(phase4b)` → `TODO(phase4c)` (query! macro — blocked on .sqlx/ Crane-source-filter work). **(3) Seed 11 spec markers** (standalone paragraphs, col 0, blank line before): `r[store.gc.tenant-retention]` + `r[store.gc.tenant-quota]` in `store.md` (expand line 194 one-liner to a real section); `r[store.cas.xmax-inserted]` near `store.md:202`; `r[sched.gc.path-tenants-upsert]` near `scheduler.md:88`; `r[sched.timeout.per-build]` near `scheduler.md:376`; `r[sched.retry.per-worker-budget]` near poison section; `r[worker.fuse.circuit-breaker]` + `r[worker.heartbeat.store-degraded]` in `worker.md` near FUSE section; `r[worker.silence.timeout-kill]` near `worker.md:152`; `r[gw.rate.per-tenant]` + `r[gw.conn.cap]` in `gateway.md`; `r[ctrl.gc.cron-schedule]` in `controller.md`. **(4) Update deferral prose** at `challenges.md:100` + `failure-modes.md:52`: change "Phase 4 deferral" → "Phase 4b: see `r[worker.fuse.circuit-breaker]`" (forward-ref, NOT removing prose — P0210 does that). |
| **Primary files** | `docs/src/phases/phase4b.md`, `docs/src/components/store.md`, `docs/src/components/scheduler.md`, `docs/src/components/worker.md`, `docs/src/components/gateway.md`, `docs/src/components/controller.md`, `docs/src/challenges.md`, `docs/src/failure-modes.md`, `rio-worker/src/upload.rs` (1-line TODO retag), `rio-scheduler/src/db.rs` (1-line TODO retag) |
| **Deps** | — (frontier root) |
| **Tracey** | Defines 11 new markers. Exit: `tracey query uncovered` MUST show exactly these 11. |
| **Serialization** | **MUST merge before Wave 1** (all downstream plans reference these marker IDs in `r[impl]`/`r[verify]` annotations). |
| **Exit** | `/nbr .#ci` green. `tracey query validate` = 0 errors. `tracey query uncovered` = exactly 11 new rules with the IDs above. `grep -c 'TODO(phase4b)' rio-*/` = 5 (down from 7; two retagged to 4c). No `r[impl]`/`r[verify]` in this plan. |

---

### P0205 — proto: pin `HeartbeatRequest.store_degraded = 9`

| Field | Value |
|---|---|
| **Tasks** | **(1)** Insert at `types.proto:368` (after `size_class = 8;`, before `}`): `bool store_degraded = 9;` with doc-comment: "FUSE circuit breaker open — worker cannot fetch from store. Scheduler treats like `draining`: `has_capacity()` returns false. Cleared when breaker closes/half-opens. Default false — wire-compatible (old workers don't send it, scheduler reads false)." **(2)** One-line changelog entry in `proto.md` if a changelog section exists. **(3)** Default-false roundtrip test in `rio-proto/tests/` (construct `HeartbeatRequest::default()`, serialize, deserialize, assert `store_degraded == false`). **(4)** Fix the single call site at `rio-worker/src/runtime.rs:124` (struct literal for `HeartbeatRequest`): add `store_degraded: false,` explicitly — prefer explicit over `..Default::default()` for a heartbeat-critical struct. |
| **Primary files** | `rio-proto/proto/types.proto`, `rio-worker/src/runtime.rs` (1 line), `docs/src/components/proto.md` |
| **Deps** | P0204 (doc-comment references `r[worker.heartbeat.store-degraded]`) |
| **Tracey** | None — proto is plumbing. Marker impl/verify land in P0210/P0211. |
| **Serialization** | **ONLY types.proto touch in 4b** (27 prior collisions on this file). **HARD SERIAL: must merge before P0210, P0211.** `runtime.rs` 1-liner doesn't collide with P0210's larger edit (same line, but P0210 replaces `false` with real value — clean rebase). |
| **Exit** | `/nbr .#ci` green. Field 9 occupied. `HeartbeatRequest::default().store_degraded == false`. |

---

### P0206 — path_tenants: migration 012 + scheduler upsert + completion hook

**Critical path head.** If the completion.rs tenant-lookup turns out to need a different borrow pattern than `self.builds.get(build_id).and_then(|b| b.tenant_id)`, downstream P0207 VM test reshapes.

| Field | Value |
|---|---|
| **Tasks** | **(1) Migration `012_path_tenants.sql`** (NEW file — NOT 009 append per §0.A1): `CREATE TABLE path_tenants(store_path_hash BYTEA NOT NULL, tenant_id UUID NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE, first_referenced_at TIMESTAMPTZ NOT NULL DEFAULT now(), PRIMARY KEY(store_path_hash, tenant_id)); CREATE INDEX path_tenants_retention_idx ON path_tenants(tenant_id, first_referenced_at); ALTER TABLE narinfo DROP COLUMN tenant_id;` — no FK→narinfo (follows `migrations/007_live_pins.sql` precedent). Header comment documents why 012 not 009-Part-C. **(2) Stale comment fix:** `migrations/002_store.sql:30` comment "multi-tenancy deferred to Phase 4" — leave it (changing 002 = checksum break). Add note in 012 header instead. **(3) `SchedulerDb::upsert_path_tenants`** in `db.rs` after `pin_live_inputs` (`:591-625` pattern): `pub async fn upsert_path_tenants(&self, output_paths: &[String], tenant_ids: &[Uuid]) -> Result<u64, sqlx::Error>` — SHA-256 each path (`sha2::Sha256::digest(p.as_bytes()).to_vec()` per `:602`), build cartesian product (`paths × tenants`), `INSERT INTO path_tenants(store_path_hash, tenant_id) SELECT * FROM unnest($1::bytea[], $2::uuid[]) ON CONFLICT DO NOTHING`, return `.rows_affected()`. **(4) Call site** in `completion.rs` after `:377` `trigger_log_flush`: `let tenant_ids: Vec<Uuid> = interested_builds.iter().filter_map(|id| self.builds.get(id)?.tenant_id).collect();` (field at `state/build.rs:104` is `Option<Uuid>`). If non-empty AND `output_paths` non-empty → `if let Err(e) = self.db.upsert_path_tenants(&output_paths, &tenant_ids).await { warn!(?e, "path_tenants upsert failed; GC retention may under-retain") }` — best-effort, never fail completion. **(5) Unit test** (ephemeral PG): 2 tenants + 1 drv → dedup → 1 execution → completion → 2 rows (same hash, distinct tenant_id); call again → 0 new rows (ON CONFLICT). **(6) VM assertion** in `lifecycle.nix` extending `gc-sweep` subtest near `:1800`: after building with tenant SSH key, `SELECT count(*) FROM path_tenants WHERE store_path_hash = digest($out, 'sha256')` ≥ 1. |
| **Primary files** | `migrations/012_path_tenants.sql` (NEW), `rio-scheduler/src/db.rs`, `rio-scheduler/src/actor/completion.rs`, `rio-scheduler/src/actor/tests/completion.rs`, `nix/tests/scenarios/lifecycle.nix` |
| **Deps** | P0204 |
| **Tracey** | `r[impl sched.gc.path-tenants-upsert]` at completion.rs call site; `r[verify ...]` on db.rs unit test |
| **Serialization** | `completion.rs` collides with P0219 (`:68` vs `:377` — distant, different functions, low risk). `lifecycle.nix` collides with P0207 — **merge P0206 first** so P0207's VM test can assert positive row counts. `db.rs` has `TODO(phase4c)` retag from P0204 at `:9` — distant, no conflict. |
| **Exit** | `/nbr .#ci` green. Ephemeral-PG dedup test: 2 tenants × 3 paths = 6 rows, re-call = 0 new. VM test: `path_tenants` row appears post-build. |

---

### P0207 — mark CTE tenant-retention seed + quota query + cache_server auth TODO

**Decoupled from P0206** — empty `path_tenants` table makes the new UNION arm a no-op (0 rows). This plan's unit test seeds rows directly; can develop in parallel with P0206, but **VM test needs P0206's upsert** to produce real rows, so merge-order P0206→P0207.

| Field | Value |
|---|---|
| **Tasks** | **(1) 6th UNION arm** in `mark.rs` after existing 5th arm (`:74`): `UNION SELECT n.store_path FROM narinfo n JOIN path_tenants pt USING (store_path_hash) JOIN tenants t ON t.tenant_id = pt.tenant_id WHERE pt.first_referenced_at > now() - make_interval(hours => t.gc_retention_hours)`. Union-of-retention: path survives if ANY tenant's window covers it. Update doc-comment at `:101` (currently "UNION of five seeds") → "six seeds". **(2) NEW `rio-store/src/gc/tenant.rs`**: `pub async fn tenant_store_bytes(pool: &PgPool, tenant_id: Uuid) -> Result<i64, sqlx::Error>` = `SELECT COALESCE(SUM(n.nar_size), 0)::bigint FROM narinfo n JOIN path_tenants pt USING (store_path_hash) WHERE pt.tenant_id = $1`. Accounting only — enforcement is Phase 5. **(3) `gc/mod.rs`**: add `pub mod tenant;` at `:44` (after existing `pub mod sweep;`). **(4) Close `cache_server/auth.rs:36` TODO**: change write-only `AuthenticatedTenant(Option<String>)` to carry `(tenant_id: Option<Uuid>, tenant_name: Option<String>)` — lookup both from `tenants` table by `cache_token`. Keep write-only for now; leave `TODO(phase5): per-tenant narinfo filtering via JOIN path_tenants WHERE tenant_id = $auth`. Delete `TODO(phase4b)`. **(5) Unit test**: seed narinfo + path_tenants with `first_referenced_at` inside/outside retention → run mark → inside-window is reachable, outside is not. **(6) VM test** in `lifecycle.nix` extending `gc-sweep`: backdate `path_tenants.first_referenced_at` past `tenants.gc_retention_hours` AND `narinfo.created_at` past global grace → `TriggerGC` → swept. Control: inside tenant window but past global grace → survives (tenant retention extends global). |
| **Primary files** | `rio-store/src/gc/mark.rs`, `rio-store/src/gc/tenant.rs` (NEW), `rio-store/src/gc/mod.rs`, `rio-store/src/cache_server/auth.rs`, `nix/tests/scenarios/lifecycle.nix`, `docs/src/components/store.md` |
| **Deps** | P0204 (markers). **Soft dep P0206** (merge-order only; unit test is self-seeding). |
| **Tracey** | `r[impl store.gc.tenant-retention]` at mark.rs UNION arm; `r[impl store.gc.tenant-quota]` at tenant.rs; `r[verify ...]` on seeded unit test + VM test |
| **Serialization** | `lifecycle.nix` ↔ P0206 (merge P0206 first). `gc/mod.rs` ↔ P0212 (P0207 adds 1 line at `:44`, P0212 adds `run_gc` fn elsewhere in 465-line file — trivial). |
| **Exit** | `/nbr .#ci` green. Unit test proves inside-window retained, outside unreachable. VM test proves tenant retention extends global grace. `grep 'TODO(phase4b)' rio-store/src/cache_server/auth.rs` = empty. |

---

### P0208 — xmax inserted-check: signature-changing upsert refactor

**Spike the PG idiom before committing Rust.** phase4b.md reads "add RETURNING clause" — reality is a return-type change (`Result<()>` → `Result<HashSet<Vec<u8>>>`), a deleted query in cas.rs, and first batch-RETURNING-from-UNNEST in the codebase.

| Field | Value |
|---|---|
| **Tasks** | **(0) PRE-RUST: 30-minute psql spike.** In ephemeral PG: `CREATE TABLE t(k int PRIMARY KEY, v int DEFAULT 0); INSERT INTO t(k) SELECT unnest(ARRAY[1,1,2,3]) ON CONFLICT(k) DO UPDATE SET v = t.v RETURNING k, (xmax = 0) AS inserted;` → verify output is per-row with correct `inserted` bools (expect: `(1,true),(1,false),(2,true),(3,true)` OR possibly dedupe — document what actually happens). If xmax semantics are surprising, **fallback**: add `updated_at TIMESTAMPTZ DEFAULT now()` column via migration 013, upsert bumps it with `ON CONFLICT DO UPDATE SET updated_at = now()`, drain checks `updated_at < $marked_at`. Document chosen approach in plan doc comments. **(1)** `chunked.rs:117-127` change `.execute()` → `query_as::<_, (Vec<u8>, bool)>(r#"INSERT ... ON CONFLICT ... DO UPDATE SET refcount = chunks.refcount + EXCLUDED.refcount RETURNING blake3_hash, (xmax = 0) AS inserted"#).fetch_all()`. Return type `Result<(), ...>` → `Result<HashSet<Vec<u8>>, ...>` (hashes where inserted). **(2)** `cas.rs`: caller at `:147` now gets inserted set. Thread to `do_upload` (`:160`) as new param, replacing the `SELECT blake3_hash, refcount FROM chunks` re-query at `:194-217` — delete that query entirely. **(3)** `drain.rs:109`: update comment to reference xmax upstream fix; keep `still_dead` re-check as belt-and-suspenders. Delete `TODO(phase4b)`. **(4)** Unit test: sequential-simulated concurrent PutPath — first upsert 2 chunks → returns {A,B}; second upsert with 1 overlap → returns {} for overlap. **(5)** Integration test: `tokio::join!` two PutPath with same chunk → exactly one gets `inserted=true`. |
| **Primary files** | `rio-store/src/metadata/chunked.rs`, `rio-store/src/cas.rs`, `rio-store/src/gc/drain.rs`, `docs/src/components/store.md` |
| **Deps** | P0204 |
| **Tracey** | `r[impl store.cas.xmax-inserted]` at chunked.rs RETURNING clause; `r[verify ...]` on concurrent test |
| **Serialization** | **File-disjoint from all other 4b plans.** chunked.rs has `r[impl store.chunk.refcount-txn]` + `r[impl store.put.wal-manifest]` — unchanged semantics, no tracey bump. |
| **Exit** | `/nbr .#ci` green. psql spike result documented in PR description. Concurrent test: exactly one inserted=true per shared chunk. `grep 'TODO(phase4b)' rio-store/src/gc/drain.rs` = empty. |

---

### P0209 — FUSE circuit breaker (worker-side only, std::sync ONLY)

**R-SYNC constraint:** `fetch.rs:268-269` is explicit — FUSE callbacks are on fuser's thread pool, "Never call from async." No tokio::sync.

| Field | Value |
|---|---|
| **Tasks** | **(1) NEW `rio-worker/src/fuse/circuit.rs`**: `pub struct CircuitBreaker { consecutive_failures: AtomicU32, open_since: parking_lot::Mutex<Option<std::time::Instant>>, threshold: u32, auto_close_after: Duration }`. **ALL std::sync / parking_lot — ZERO tokio::sync.** Pattern reference: `rio-scheduler/src/actor/breaker.rs` for state-machine shape (but that one is single-threaded, uses plain `u32` — this needs `AtomicU32`). States: closed (failures < threshold), open (elapsed < auto_close), half-open (elapsed ≥ auto_close → next call probes). Methods: `check(&self) -> Result<(), Errno>` (EIO if open, Ok if closed/half-open — pure sync, no `.await`); `record(&self, ok: bool)` (success → reset+close, failure → increment+maybe-open — pure sync); `is_open(&self) -> bool` (for heartbeat). Defaults: threshold=5, auto_close=30s. **(2) Wire into `ensure_cached` at `fetch.rs:54`**: `self.circuit.check()?` first line. At the fetch result site (after the `block_on` completes): `self.circuit.record(result.is_ok())`. Timeout (WAIT_DEADLINE at `:44` exceeded) counts as failure → `record(false)`. **(3) `fuse/mod.rs`**: `pub mod circuit;`; add `circuit: CircuitBreaker` field on `NixStoreFs`. Expose `pub fn circuit(&self) -> &CircuitBreaker` for heartbeat read (P0210). **(4) Metric**: `metrics::gauge!("rio_worker_fuse_circuit_open").set(1.0/0.0)` on state transition. Register in `lib.rs` describe block. **(5) Unit tests** (plain `#[test]`, NOT tokio — it's all sync): 4 failures → closed; 5th → open; open → `check()` = EIO; `std::thread::sleep(31s)` → half-open probes once; probe success → closed; probe failure → re-open. **NOTE:** `tokio::time::pause` DOES NOT WORK — this is `std::time::Instant`. Either use real sleeps (bad, slow) or inject a `Clock` trait. Prefer Clock injection: `trait Clock { fn now(&self) -> Instant; }`, `struct SystemClock; struct MockClock(Mutex<Instant>)`. |
| **Primary files** | `rio-worker/src/fuse/circuit.rs` (NEW), `rio-worker/src/fuse/fetch.rs`, `rio-worker/src/fuse/mod.rs`, `rio-worker/src/lib.rs` |
| **Deps** | P0204 |
| **Tracey** | `r[impl worker.fuse.circuit-breaker]` at circuit.rs; `r[verify ...]` on state-transition tests |
| **Serialization** | **File-disjoint from all other Wave 1 plans.** |
| **Exit** | `/nbr .#ci` green. State-transition tests cover closed→open→half-open→closed AND closed→open→half-open→open. Metric registered. Zero `tokio::sync` / `async fn` in circuit.rs. |

---

### P0210 — heartbeat: plumb circuit.is_open() → store_degraded

20 lines of plumbing once P0205+P0209 merge.

| Field | Value |
|---|---|
| **Tasks** | **(1)** `build_heartbeat_request` at `runtime.rs:77-124`: new param `store_degraded: bool`. Replace `store_degraded: false` (from P0205) at `:124` with `store_degraded`. **(2)** Caller in runtime.rs heartbeat loop: read `fuse_fs.circuit().is_open()`, pass to `build_heartbeat_request`. Needs `Arc<NixStoreFs>` or a narrow `Arc<CircuitBreaker>` handle threaded from FUSE mount init → heartbeat loop (check `main.rs` wiring for the cleanest path). **(3)** 4 test call sites at `runtime.rs:704/734/752/765`: pass `false`. **(4)** Close deferrals: `challenges.md:100` + `failure-modes.md:52` — DELETE the "Phase 4 deferral" blockquotes entirely (behavior now implemented). |
| **Primary files** | `rio-worker/src/runtime.rs`, `rio-worker/src/main.rs`, `docs/src/challenges.md`, `docs/src/failure-modes.md` |
| **Deps** | **P0205 + P0209** (hard — needs both proto field and `CircuitBreaker::is_open()`) |
| **Tracey** | `r[impl worker.heartbeat.store-degraded]` at runtime.rs set site |
| **Serialization** | `runtime.rs` disjoint from other wave-2 plans. `main.rs` is top collision file (35 prior) but only P0210 touches it in 4b. |
| **Exit** | `/nbr .#ci` green. One test with mock circuit open → `HeartbeatRequest.store_degraded == true`. Deferral blocks removed. |

---

### P0211 — scheduler: consume store_degraded in WorkerState + has_capacity

| Field | Value |
|---|---|
| **Tasks** | **(1)** Add `pub store_degraded: bool` to `WorkerState` at `state/worker.rs:76` (next to `draining`). Default `false` at `:104`. **(2)** `has_capacity()` at `:124-128`: `!self.draining && !self.store_degraded && ...arithmetic`. Update comment at `:56-57` (currently mentions draining only) to mention both flags. **(3)** `handle_heartbeat` at `actor/worker.rs:290`: wherever fields are copied from `HeartbeatRequest` → `WorkerState`, add `w.store_degraded = req.store_degraded;`. On false→true transition: `info!(worker_id, "marked store-degraded; removing from assignment pool")`. **(4)** Unit test in `state/worker.rs`: `has_capacity()` returns false when `store_degraded=true` regardless of capacity arithmetic. **(5)** Integration test in actor tests: heartbeat with `store_degraded=true` → worker NOT in `best_worker()` result. |
| **Primary files** | `rio-scheduler/src/state/worker.rs`, `rio-scheduler/src/actor/worker.rs` |
| **Deps** | **P0205** (hard — `req.store_degraded` doesn't exist before proto regen). P0210 is NOT a hard dep (scheduler can consume a field nobody sends yet — wire-default false). |
| **Tracey** | `r[verify worker.heartbeat.store-degraded]` on integration test (scheduler verifies worker contract) |
| **Serialization** | **actor/worker.rs ↔ P0214** (`handle_heartbeat:290` vs `handle_tick:440` — 150 lines apart, different functions). Low merge risk but serialize if same-day; prefer P0214 first (no deps). |
| **Exit** | `/nbr .#ci` green. `has_capacity` unit test covers new flag. `best_worker` excludes degraded worker. |

---

### P0212 — GC automation: run_gc extraction + sweep metrics + controller cron

| Field | Value |
|---|---|
| **Tasks** | **(1)** `rio-store/src/gc/mod.rs`: new `pub async fn run_gc(pool: &PgPool, backend: Arc<dyn ChunkBackend>, dry_run: bool, progress_tx: mpsc::Sender<GcProgress>) -> Result<GcStats>` — extract body of `trigger_gc` from `grpc/admin.rs:344-539+` (advisory locks `GC_LOCK_ID`+`GC_MARK_LOCK_ID` at `:24`, mark→sweep→spawn-drain). `grpc/admin.rs::trigger_gc` becomes thin wrapper (create channel, spawn `run_gc`, stream-forward `GcProgress`). **Move** `r[impl store.gc.empty-refs-gate]` annotation from admin.rs to its new location. **(2)** `sweep.rs`: `metrics::counter!("rio_store_gc_paths_swept_total").increment(stats.paths_deleted as u64)` + `"rio_store_gc_chunks_enqueued_total".increment(...)`. `GcStats` fields already exist at `gc/mod.rs:79-94`. Register in `rio-store/src/lib.rs` describe block. **(3)** NEW `rio-controller/src/reconcilers/gc_schedule.rs`: `pub async fn run(store_admin_addr: String, interval: Duration, shutdown: CancellationToken)`. Loop: `select! { _ = shutdown.cancelled() => break, _ = interval.tick() => { ... } }`. Each tick: `match tokio::time::timeout(Duration::from_secs(30), connect_store_admin(&addr)).await { Ok(Ok(client)) => { /* TriggerGC, drain stream logging GcProgress */ }, _ => { warn!(...); counter!("rio_controller_gc_runs_total", "result" => "connect_failure").increment(1); continue; } }` — **NEVER `?`-propagate connect error out of the loop** (R-CONN). On stream success → `"result" => "success"`. Default interval 24h, configurable via `controller.toml gc_interval_hours` (0 = disabled). **(4)** Wire in `rio-controller/src/main.rs` via `spawn_monitored("gc-cron", ...)` near `:346`. **(5)** `observability.md:185` — flip `rio_controller_gc_runs_total` from "(Phase 4+) not yet emitted" to actual row. **(6)** Unit test with `tokio::time::pause` + mock StoreAdminClient: advance 24h → 1 TriggerGC call → counter=1. |
| **Primary files** | `rio-store/src/gc/mod.rs`, `rio-store/src/gc/sweep.rs`, `rio-store/src/grpc/admin.rs` (shrink), `rio-store/src/lib.rs`, `rio-controller/src/reconcilers/gc_schedule.rs` (NEW), `rio-controller/src/reconcilers/mod.rs`, `rio-controller/src/main.rs`, `docs/src/observability.md`, `docs/src/components/controller.md` |
| **Deps** | P0204. No dep on P0206/P0207 — GC automation works with/without tenant retention. |
| **Tracey** | `r[impl ctrl.gc.cron-schedule]` at gc_schedule.rs; `r[verify ...]` via paused-time unit test |
| **Serialization** | `gc/mod.rs` ↔ P0207 (P0207 adds `pub mod tenant;` at `:44`, P0212 adds `run_gc` fn elsewhere — trivial). `grpc/admin.rs` only here. `controller/main.rs` only here. |
| **Exit** | `/nbr .#ci` green. Paused-time unit test: 1 call per 24h. Connect-fail path: counter increments `result=connect_failure`, loop continues. Metrics registered. observability.md updated. |

---

### P0213 — gateway: per-tenant rate limiter + connection cap + RESOURCE_EXHAUSTED

| Field | Value |
|---|---|
| **Tasks** | **(1)** `Cargo.toml [workspace.dependencies]`: `governor = "0.6"`. `rio-gateway/Cargo.toml`: `governor.workspace = true`. **(2) NEW `rio-gateway/src/ratelimit.rs`**: `pub struct TenantLimiter(Arc<DefaultKeyedRateLimiter<String>>)`. `check(&self, tenant: Option<&str>) -> Result<(), governor::NotUntil<...>>` — `None` → key `"__anon__"`. Quota `per_minute(nonzero!(10)).allow_burst(nonzero!(30))`, configurable via `gateway.toml`. Comment: "`tenant_name` is from authorized_keys comment (operator-controlled, can't be forged) — no key-eviction needed. `TODO(phase5)`: eviction if keys become client-controlled." **(3) Rate limiter hook**: hold `Arc<TenantLimiter>` on gateway shared state (`server.rs`, same Arc passed to each session). Check before `submit_build` at `handler/build.rs:235` — read `ctx.tenant_name` (field at `server.rs:250`). On `Err(NotUntil)` → `stderr.error(format!("rate limit: too many builds from tenant '{t}' ({}/min) — wait ~{}s", rate, wait)).await` + early return (do NOT close connection). **(4) Connection cap**: `Arc<Semaphore::new(cfg.max_connections.unwrap_or(1000))>` on server. `try_acquire_owned()` before spawning session in accept loop → permit held by session task (dropped on disconnect). At cap → russh `Disconnect::TooManyConnections`. **(5) RESOURCE_EXHAUSTED** (closes `queries.rs:225` TODO): add `MetadataError::ResourceExhausted(String)` variant at `metadata/mod.rs:53`. Map sqlx pool-timeout → this. In `rio-store/src/grpc/mod.rs:84` error-conversion: `ResourceExhausted` → `Status::resource_exhausted`. In gateway `handler/build.rs` SubmitBuild error match: `Code::ResourceExhausted` → retryable `STDERR_ERROR` "scheduler backpressure — retry in ~10s". Delete `TODO(phase4b)`. **(6) Unit tests**: TenantLimiter — 30 pass, 31st fails, different tenant isolated. Semaphore — 1001st `try_acquire_owned` fails. **(7) VM test** in `security.nix` (TAIL append): config rate to 2/min burst 3, fire 4 rapid builds from same tenant SSH key → 4th gets STDERR_ERROR containing "rate limit". |
| **Primary files** | `Cargo.toml`, `rio-gateway/Cargo.toml`, `rio-gateway/src/ratelimit.rs` (NEW), `rio-gateway/src/server.rs`, `rio-gateway/src/lib.rs`, `rio-gateway/src/handler/build.rs`, `rio-store/src/metadata/mod.rs`, `rio-store/src/metadata/queries.rs`, `rio-store/src/grpc/mod.rs`, `nix/tests/scenarios/security.nix`, `docs/src/components/gateway.md` |
| **Deps** | P0204 |
| **Tracey** | `r[impl gw.rate.per-tenant]` at ratelimit.rs; `r[impl gw.conn.cap]` at server.rs; `r[verify ...]` on unit + VM |
| **Serialization** | **ONLY Cargo.toml touch in 4b** (governor is the only new dep). `server.rs` disjoint (P0217 NormalizedName might touch `:70` trim site — different concern, low risk). `grpc/mod.rs` ↔ P0218 (P0218 touches `:142` TODO, P0213 touches `:84` — distant). |
| **Exit** | `/nbr .#ci` green. security.nix: 4th build rejected with "rate limit" in error. `grep 'TODO(phase4b)' rio-store/src/metadata/queries.rs` = empty. |

---

### P0214 — scheduler: per-build overall timeout in handle_tick

**R-TOUT resolved:** `build_timeout` proto doc (`types.proto:315`) says "Max total build time" — this implements the documented intent. Worker-side per-drv floor at `actor/build.rs:567` stays as defense-in-depth.

| Field | Value |
|---|---|
| **Tasks** | **(1)** `handle_tick` at `actor/worker.rs:440`, after the existing per-derivation backstop loop (`:464`, `r[sched.backstop.timeout]`): add per-BUILD loop. `for (build_id, build) in &self.builds { if build.options.build_timeout > 0 && build.submitted_at.elapsed().as_secs() > build.options.build_timeout { timed_out.push(*build_id); } }`. For each: cancel non-terminal derivations (reuse cancel path from `CancelBuild` handler) → `transition_build(Failed { status: BuildResultStatus::TimedOut, reason: format!("build_timeout {}s exceeded (wall-clock since submission)", t) })`. **(2)** Doc-comment above the new loop explicitly distinguishing from `r[sched.backstop.timeout]` (per-derivation heuristic est×3) AND noting worker also receives `build_timeout` as per-derivation daemon floor (`actor/build.rs:567`) — both are defense-in-depth under this total-time check. **(3)** Unit test with `tokio::time::pause`: submit build with `build_timeout=60`, advance 61s, tick → Failed with `TimedOut`. **(4)** VM test in `scheduling.nix` (TAIL append): submit with `--option timeout 10`, build `sleep 60` → Failed with TimedOut within ~15s. **(5)** `errors.md:97` row "Not implemented (Phase 4)" → "Implemented." |
| **Primary files** | `rio-scheduler/src/actor/worker.rs`, `nix/tests/scenarios/scheduling.nix`, `docs/src/errors.md`, `docs/src/components/scheduler.md` |
| **Deps** | P0204 |
| **Tracey** | `r[impl sched.timeout.per-build]` in handle_tick; `r[verify ...]` on paused-time test |
| **Serialization** | **actor/worker.rs ↔ P0211** (`handle_tick:440` vs `handle_heartbeat:290` — different fns). Merge P0214 first (no deps) → P0211 rebases trivially. `scheduling.nix` ↔ P0215 (both TAIL-append subtests — low risk). `errors.md` ↔ P0215 (different rows). |
| **Exit** | `/nbr .#ci` green. Paused-time test: Failed at timeout+1s. Backstop test at `:464` still passes (independent). |

---

### P0215 — worker: maxSilentTime enforcement

| Field | Value |
|---|---|
| **Tasks** | **(1)** `stderr_loop.rs`: `max_silent_time` param already at `:49`. Add `let mut last_output = Instant::now();` before the read loop, reset on every STDERR_NEXT/STDERR_READ/STDERR_WRITE. In the `select!`, new arm: `_ = tokio::time::sleep_until((last_output + Duration::from_secs(max_silent_time)).into()), if max_silent_time > 0 => { return Ok(StderrLoopOutcome::SilenceTimeout) }` (verify actual return-type enum name). **(2)** Caller in `executor/daemon/mod.rs`: on `SilenceTimeout` → `cgroup.kill()` (method at `cgroup.rs:180`, needs `CgroupHandle` in scope — verify) → `BuildResult { status: BuildResultStatus::TimedOut, error_msg: format!("no output for {max_silent_time}s (maxSilentTime)") }`. **(3)** Note: local nix-daemon MAY enforce maxSilentTime itself (forwarded via `client_set_options` at `:80`) — rio-side enforcement is the authoritative backstop for the `TimedOut` result. **(4)** Unit test with `tokio::time::pause`: mock stderr source sends one line then nothing → advance past `max_silent_time` → `SilenceTimeout`. **(5)** VM test in `scheduling.nix` (TAIL append): `buildCommand = "echo start; sleep 60"` + `--option max-silent-time 5` → TimedOut within ~10s wall-clock (NOT 60s). **(6)** `errors.md:96` row "Not implemented" → "Implemented." |
| **Primary files** | `rio-worker/src/executor/daemon/stderr_loop.rs`, `rio-worker/src/executor/daemon/mod.rs`, `nix/tests/scenarios/scheduling.nix`, `docs/src/errors.md`, `docs/src/components/worker.md` |
| **Deps** | P0204 |
| **Tracey** | `r[impl worker.silence.timeout-kill]` at stderr_loop.rs; `r[verify ...]` on unit + VM |
| **Serialization** | `stderr_loop.rs` disjoint. `scheduling.nix` ↔ P0214 (both TAIL-append — serialize merge, low risk). `errors.md` ↔ P0214 (rows 96 vs 97 — adjacent but distinct). |
| **Exit** | `/nbr .#ci` green. VM test: kill at ~5s silence, NOT 60s. Unit test: SilenceTimeout outcome. |

---

### P0216 — rio-cli: remaining subcommands + --json + cli.nix extension

**All RPCs exist.** `GetBuildLogs`/`TriggerGC` are STREAMING — use stream-drain loop, NOT the `rpc()` helper at `main.rs:43-45` (comment already warns about this).

| Field | Value |
|---|---|
| **Tasks** | **(1)** `#[arg(long, global = true)] json: bool` on `CliArgs` near `scheduler_addr`. Each handler: `if cli.json { println!("{}", serde_json::to_string_pretty(&resp)?) } else { /* existing table */ }`. Verify prost types have `serde::Serialize` — if not, add thin wrapper structs or enable `prost-wkt-types` serde feature. **(2)** New `Cmd` variants at `main.rs:97-120`: `Workers` (ListWorkers — detail view; keep `Status` as summary), `Builds { #[arg(long)] status: Option<String>, #[arg(long)] limit: u32 }` (ListBuilds filtered), `Logs { build_id: String }` (GetBuildLogs STREAMING — drain loop printing each chunk to stdout), `Gc { #[arg(long)] dry_run: bool }` (TriggerGC STREAMING — drain loop printing `GcProgress`), `PoisonClear { drv_hash: String }` (ClearPoison). **(3)** Extend `cli.nix` (TAIL append after `:140`): `rio-cli workers --json | jq -e '.workers | length >= 1'`; `rio-cli builds` returns ≥1 after submitting; `rio-cli gc --dry-run` exits 0; `rio-cli poison-clear <nonexistent-hash>` returns NotFound (error-path). **(4)** Smoke test via rio-test-support ephemeral scheduler (per phase4b.md:37). |
| **Primary files** | `rio-cli/src/main.rs`, `nix/tests/scenarios/cli.nix` |
| **Deps** | P0204 only (no new markers — CLI is tooling under existing `r[sched.admin.*]` markers) |
| **Tracey** | No new markers. Extends `r[verify sched.admin.list-workers]`+`r[verify sched.admin.clear-poison]` coverage. |
| **Serialization** | **Fully disjoint** — only plan touching `rio-cli/src/main.rs` and `cli.nix`. |
| **Exit** | `/nbr .#ci` green. All 5 new subcommands in cli.nix. `--json | jq -e .` parses. |

---

### P0217 — NormalizedName newtype (refactor filler)

Goes in `rio-common`, not scheduler-only — it's used across 4 crates.

| Field | Value |
|---|---|
| **Tasks** | **(1)** NEW (or extend existing) `rio-common/src/tenant.rs`: `#[derive(Clone, Debug, PartialEq, Eq, Hash)] pub struct NormalizedName(String)`. `impl TryFrom<&str>` — trim, reject empty/whitespace-only, `.to_string()`. `Display`, `AsRef<str>`, `From<NormalizedName> for String`. **(2)** Replace ad-hoc `.trim()` sites: `rio-scheduler/src/grpc/mod.rs:189` (the TODO — 4th site), `rio-gateway/src/server.rs:70` (authorized_keys comment trim), `rio-store/src/cache_server/auth.rs` tenant lookup, + grep for other `tenant.*trim` patterns. **(3)** Delete `TODO(phase4b)` at `grpc/mod.rs:189`. |
| **Primary files** | `rio-common/src/tenant.rs` (NEW or extend), `rio-common/src/lib.rs`, `rio-scheduler/src/grpc/mod.rs`, `rio-gateway/src/server.rs`, `rio-store/src/cache_server/auth.rs` |
| **Deps** | P0204 only. **Schedule late** — wide file footprint. |
| **Tracey** | None — pure refactor, zero behavior change. |
| **Serialization** | `server.rs:70` ↔ P0213 (P0213 reads `tenant_name` for rate limiter — P0217 would change the type). **Merge after P0213.** `cache_server/auth.rs` ↔ P0207 (P0207 changes the struct to carry tenant_id — P0217 wraps name). **Merge after P0207.** |
| **Exit** | `/nbr .#ci` green. Zero behavior change — existing tests prove it. `grep 'TODO(phase4b)' rio-scheduler/src/grpc/mod.rs` = empty. |

---

### P0218 — store config: plumb nar_buffer_budget_bytes

| Field | Value |
|---|---|
| **Tasks** | **(1)** `rio-store/src/main.rs:50` Config struct (uses `rio_common::config::load`): add `nar_buffer_budget_bytes: Option<u64>`. **(2)** Pass to `StoreServer::with_nar_budget()` (builder exists per `grpc/mod.rs:141` comment) at startup, replacing `DEFAULT_NAR_BUDGET` hardcode at `:153` when `Some`. **(3)** Delete `TODO(phase4b)` at `grpc/mod.rs:142`. **(4)** Config roundtrip test: `nar_buffer_budget_bytes = 12345` in `store.toml` → value reaches StoreServer. |
| **Primary files** | `rio-store/src/main.rs`, `rio-store/src/grpc/mod.rs` |
| **Deps** | P0204 only |
| **Tracey** | None — config plumbing. |
| **Serialization** | `grpc/mod.rs:142` ↔ P0213 (`grpc/mod.rs:84` — distant, different function). File-disjoint in practice. |
| **Exit** | `/nbr .#ci` green. `grep 'TODO(phase4b)' rio-store/src/grpc/mod.rs` = empty. |

---

### P0219 — per-worker failure counts (separate from distinct-worker poison)

| Field | Value |
|---|---|
| **Tasks** | **(1)** Add `pub per_worker_failures: HashMap<WorkerId, u32>` to `DerivationState` at `state/derivation.rs:200` (beside `failed_workers: HashSet`). Default `HashMap::new()` at `:291`. **NOT persisted** — `:354`/`:421` db-load paths initialize empty (in-memory retry budget, reset on scheduler restart is acceptable — matches pre-4a poison TTL behavior). Document in field doc-comment. **(2)** `completion.rs:68` where `failed_workers.insert(worker_id)`: also `*state.per_worker_failures.entry(worker_id.clone()).or_insert(0) += 1`. **(3)** `assignment.rs` `best_worker` filter (near existing `!drv.failed_workers.contains()`): add `&& drv.per_worker_failures.get(&worker_id).copied().unwrap_or(0) < MAX_SAME_WORKER_RETRIES` (const = 2). **(4)** Poison threshold at `completion.rs:76` STAYS on `failed_workers.len()` (distinct-worker count) — unchanged semantics. **(5)** Unit test: fail 3× on worker A → `per_worker_failures[A]=3`, `failed_workers.len()=1` → NOT poisoned, but A excluded from `best_worker`. Fail on A,B,C → `failed_workers.len()=3` → poisoned. |
| **Primary files** | `rio-scheduler/src/state/derivation.rs`, `rio-scheduler/src/actor/completion.rs`, `rio-scheduler/src/assignment.rs` |
| **Deps** | P0204 |
| **Tracey** | `r[impl sched.retry.per-worker-budget]` at assignment.rs filter; `r[verify ...]` on unit test |
| **Serialization** | `completion.rs:68` ↔ P0206 (`:377` — distant, different functions). Low risk; serialize if same-day. `assignment.rs` disjoint. |
| **Exit** | `/nbr .#ci` green. 3×-same-worker test: no poison + A excluded. 3-distinct test: poisoned. |

---

## §5. Collision matrix (intra-4b)

| File | Plans | Resolution |
|---|---|---|
| `rio-proto/proto/types.proto` | **P0205 only** | Single writer. 27 prior collisions — this is the cleanest 4b gets. |
| `migrations/012_path_tenants.sql` | **P0206 only** (NEW file) | Single writer. Sidesteps 009 checksum entirely. |
| `Cargo.toml` workspace | **P0213 only** | Single writer (governor is only new dep). |
| `rio-scheduler/src/actor/worker.rs` | P0211 (`:290`), P0214 (`:440`) | **P0214 first** (no deps) → P0211 rebases. 150 lines apart, different fns. |
| `rio-scheduler/src/actor/completion.rs` | P0206 (`:377`), P0219 (`:68`) | 300 lines apart, different fns. Parallel OK; P0206 first if same-day. |
| `rio-store/src/gc/mod.rs` | P0207 (1 line at `:44`), P0212 (`run_gc` fn) | Trivially mergeable. Parallel OK. |
| `rio-store/src/grpc/mod.rs` | P0213 (`:84`), P0218 (`:142`) | Distant, different fns. Parallel OK. |
| `rio-gateway/src/server.rs` | P0213 (Semaphore+Limiter Arc), P0217 (`:70` trim) | **P0213 first** → P0217 rebases (filler goes last anyway). |
| `rio-store/src/cache_server/auth.rs` | P0207 (tenant_id tuple), P0217 (NormalizedName wrap) | **P0207 first** → P0217 rebases. |
| `nix/tests/scenarios/lifecycle.nix` | P0206, P0207 | **P0206 first** (P0207 VM test needs P0206's rows). Both TAIL-append. |
| `nix/tests/scenarios/scheduling.nix` | P0214, P0215 | Both TAIL-append new subtests. Serialize merge, low risk. |
| `docs/src/errors.md` | P0214 (row 97), P0215 (row 96) | Adjacent rows, different lines. Auto-merge. |
| `docs/src/components/*.md` | **P0204 only** | All markers seeded upfront → zero downstream doc collisions. |

**Peak parallelism:** Wave 1 (4) + Wave 3 fillers (3) = 7 concurrent. Wave 2 = 7 (with 2 soft-serial pairs). DAG-runner can comfortably run 7-8 agents.

---

## §6. Coverage check — every phase4b.md item + every TODO(phase4b)

| phase4b.md line | Item | Plan | Status |
|---|---|---|---|
| 13 | NAR scanner | P0204 marks `[x]` | DONE (4a) |
| 18 | xmax inserted-check | P0208 | open |
| 22 | Migration 009 Part C | P0206 (→ **012**, see A1) | open |
| 23 | upsert_path_tenants | P0206 | open |
| 24 | Mark CTE per-tenant seed | P0207 | open |
| 25 | Per-tenant quota query | P0207 | open |
| 26 | Audit narinfo.tenant_id readers | P0206 (trivial — grep empty, bundled with drop) | open |
| 30 | run_gc orchestration | P0212 | open |
| 31 | Controller GC cron | P0212 | open |
| 32 | Sweep metrics | P0212 | open |
| 36 | rio-cli subcommands | P0216 | partial→complete |
| 37 | rio-cli integration smoke | P0216 | open |
| 39 | Karpenter | P0204 marks `[x]` | DONE (4a) |
| 40 | rio-cli flake packaging | P0204 marks `[x]` (scheduler-image only) | DONE |
| 44 | Per-tenant rate limiter | P0213 | open |
| 45 | Connection cap + backpressure | P0213 | open |
| 46-49 | FUSE circuit breaker + proto + scheduler | P0205+P0209+P0210+P0211 | open |
| 50 | maxSilentTime enforcement | P0215 | open |
| 51 | Per-build timeout | P0214 | open |
| 52 | Per-worker failure counts | P0219 | open |
| 57 | VM Section B (GC+refs) | P0206+P0207 | open (refs part DONE at `lifecycle.nix:1800-1811`) |
| 58 | VM Section C (rate-limit) | P0213 | open |
| 59 | VM Section D (maxSilentTime) | P0215 | open |
| 60 | VM Section E (cli smoke) | P0216 | open |
| 64 | Tracey markers | P0204 seeds; each plan annotates | open |

| TODO(phase4b) location | Disposition | Plan |
|---|---|---|
| `rio-store/src/gc/drain.rs:109` xmax | In-scope | P0208 |
| `rio-scheduler/src/grpc/mod.rs:189` NormalizedName | In-scope filler | P0217 |
| `rio-store/src/cache_server/auth.rs:36` tenant scoping | Subsumed (tenant_id tuple + Phase5 TODO) | P0207 |
| `rio-store/src/grpc/mod.rs:142` nar_buffer config | In-scope filler | P0218 |
| `rio-store/src/metadata/queries.rs:225` RESOURCE_EXHAUSTED | Subsumed (backpressure translation) | P0213 |
| `rio-worker/src/upload.rs:166` trailer-refs | **DEFER 4c** — gated on "if pre-scan cost measurable" | P0204 retags |
| `rio-scheduler/src/db.rs:9` query!() | **DEFER 4c** — blocked on .sqlx/ build-system work | P0204 retags |

**100% coverage. 2 explicit 4c deferrals, both justified.**

---

## §7. Deferred to phase 4c (NOT a 4b plan)

| Item | Location | Rationale |
|---|---|---|
| trailer-refs protocol extension | `rio-worker/src/upload.rs:166` | TODO text self-gates: "if pre-scan cost becomes measurable." No measurement. Re-tag `TODO(phase4c)` in P0204. |
| query!() macro migration | `rio-scheduler/src/db.rs:9` | Blocked on `.sqlx/` in Crane source filter + `cargo sqlx prepare` tooling. Touches every query in a 2017-line file. Serializes with P0206. Zero user-visible. Re-tag `TODO(phase4c)` in P0204. |
| Orphan-drv recovery sweep | `docs/src/remediations/phase4a/01-*.md:515` | "Resource-growth nuisance, not correctness." No urgency signal. |
| 2-replica failover VM test | `docs/src/components/scheduler.md:528` | Requires `mkControlNode` decomposition + PG network-trust + dnsmasq. Large infra lift. Unit coverage exists. |
| Gateway→scheduler trace parentage | `nix/tests/scenarios/observability.nix:268` | Requires manual span-before-`#[instrument]` OR proto change. Observability-only. |
| 009 header comment stale | `migrations/009_phase4.sql:7` | Would change checksum. Leave alone; document deviation in phase4b.md. |
| 002 header comment stale | `migrations/002_store.sql:8,30` | Same — checksum-locked. |

---

## §8. Verification commands

```bash
# After P0204 merges:
nix develop /root/src/rio-build/main -c tracey query uncovered
# MUST show exactly 11 rules: store.gc.tenant-retention, store.gc.tenant-quota,
#   store.cas.xmax-inserted, sched.gc.path-tenants-upsert, sched.timeout.per-build,
#   sched.retry.per-worker-budget, worker.fuse.circuit-breaker,
#   worker.heartbeat.store-degraded, worker.silence.timeout-kill,
#   gw.rate.per-tenant, gw.conn.cap, ctrl.gc.cron-schedule
#   (12 total — §4.P0204.Tracey lists 11 but counting gives 12. RECOUNT before merge.)

nix develop /root/src/rio-build/main -c tracey query validate
# MUST be 0 errors (uncovered ≠ error)

grep -c 'TODO(phase4b)' /root/src/rio-build/main/rio-*/src/**/*.rs
# MUST be 5 (was 7; upload.rs:166 and db.rs:9 retagged to 4c)

# After each plan merges:
/nbr .#ci   # single gate

# After all 16 plans merge (phase-4b tag candidate):
nix develop /root/src/rio-build/main -c tracey query uncovered
# MUST be empty (back to 167/178 → 178/178)

grep -rn 'TODO(phase4b)' /root/src/rio-build/main/rio-*/src/
# MUST be empty

/nbr .#coverage-full   # backgrounded, non-gating — verify upsert_path_tenants covered

# Coordinator invocation:
python3 /root/src/rio-build/main/.claude/lib/state.py collisions-regen
python3 /root/src/rio-build/main/.claude/lib/state.py dag-render
# Expected frontier after P0204+P0205: [206,207,208,209,212,213,214,215,216,217,218,219]
/dag-run
```

---

## §9. Summary table (for `rio-planner --inline` invocation)

| P# | Title | Wave | Deps | Key files | Markers |
|---|---|---|---|---|---|
| P0204 | doc-sync + 11-12 spec markers | 0 | — | `docs/src/**`, upload.rs:166 retag, db.rs:9 retag | defines 11-12 |
| P0205 | proto: `store_degraded = 9` | 0 | P0204 | `types.proto:368`, `runtime.rs:124` | — |
| P0206 | path_tenants: migration 012 + upsert | 1 | P0204 | `migrations/012` (NEW), `db.rs`, `completion.rs`, `lifecycle.nix` | sched.gc.path-tenants-upsert |
| P0207 | mark CTE tenant seed + quota | 1 | P0204 (soft:P0206) | `mark.rs`, `gc/tenant.rs` (NEW), `auth.rs`, `lifecycle.nix` | store.gc.tenant-retention, store.gc.tenant-quota |
| P0208 | xmax RETURNING + psql pre-spike | 1 | P0204 | `chunked.rs`, `cas.rs`, `drain.rs` | store.cas.xmax-inserted |
| P0209 | FUSE circuit breaker (std::sync!) | 1 | P0204 | `fuse/circuit.rs` (NEW), `fetch.rs`, `fuse/mod.rs` | worker.fuse.circuit-breaker |
| P0210 | heartbeat plumb store_degraded | 2 | P0205,P0209 | `runtime.rs`, `worker/main.rs`, `challenges.md`, `failure-modes.md` | worker.heartbeat.store-degraded (impl) |
| P0211 | scheduler consume store_degraded | 2 | P0205 | `state/worker.rs`, `actor/worker.rs:290` | worker.heartbeat.store-degraded (verify) |
| P0212 | GC automation: run_gc+cron+metrics | 2 | P0204 | `gc/mod.rs`, `grpc/admin.rs`, `reconcilers/gc_schedule.rs` (NEW), `controller/main.rs` | ctrl.gc.cron-schedule |
| P0213 | gateway ratelimit+cap+ResourceExh | 2 | P0204 | `Cargo.toml`, `ratelimit.rs` (NEW), `server.rs`, `handler/build.rs`, `queries.rs`, `security.nix` | gw.rate.per-tenant, gw.conn.cap |
| P0214 | per-build timeout | 2 | P0204 | `actor/worker.rs:440`, `scheduling.nix`, `errors.md` | sched.timeout.per-build |
| P0215 | maxSilentTime enforcement | 2 | P0204 | `stderr_loop.rs`, `daemon/mod.rs`, `scheduling.nix`, `errors.md` | worker.silence.timeout-kill |
| P0216 | rio-cli subcommands + --json | 2 | P0204 | `rio-cli/src/main.rs`, `cli.nix` | — (extends sched.admin.*) |
| P0217 | NormalizedName newtype | 3 | P0204 (after P0207,P0213) | `rio-common/src/tenant.rs` (NEW), `grpc/mod.rs:189`, `server.rs:70`, `auth.rs` | — (refactor) |
| P0218 | nar_buffer_budget config plumb | 3 | P0204 | `store/main.rs`, `grpc/mod.rs:142` | — (config) |
| P0219 | per-worker failure HashMap | 3 | P0204 | `state/derivation.rs:200`, `completion.rs:68`, `assignment.rs` | sched.retry.per-worker-budget |

**16 plans. Critical path depth: 3 (P0204→P0206→P0207). Peak concurrency: 7-8. Phase completion lower bound: 4 merge waves.**
