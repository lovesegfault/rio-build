# Remediation 19: Store GC robustness

**Parent finding:** [§2.14](../phase4a.md#214-store-gc-robustness)
**Findings:** `store-mark-not-in-null-trap`, `store-trigger-gc-unbounded-extra-roots`, `store-nar-buffer-no-global-limit`, `pg-putpath-shared-lock-pool-exhaustion` (P1); `store-grace-i32-cast-truncation`, `store-pin-unvalidated-and-leaks-sqlx`, `store-append-sigs-unbounded-growth` (P2, bundled — same files, trivial)
**Blast radius:** P1 — silent GC-off, PutPath DoS via extra_roots, store OOM, 21st concurrent upload fails
**Effort:** ~4 h implementation + one VM cycle

---

## Ground truth vs §2.14

§2.14 cites `SELECT store_path FROM nodes ... FROM marks`. Actual table names
are `narinfo` and the recursive CTE is inlined in `compute_unreachable`
(`mark.rs:72-116`), not a separate `marks` table. The `NOT IN` is at line 110;
the subquery is the `reachable` CTE. Same bug, corrected location.

§2.14 says the marks table has `store_path TEXT NOT NULL` so the NULL trap is
"currently unreachable." **Confirmed but fragile** — the CTE's seed (d) is
`SELECT unnest($2::text[])`. Rust `&[String]` can't bind NULL elements, so
today `$2` is NULL-free. But seed (d) is exactly where a future "optional
extra_root" change (`Vec<Option<String>>`, or SQL-side `LEFT JOIN`) would
inject a NULL into the CTE, and `NOT IN (… NULL …)` evaluates to `UNKNOWN` for
every row. GC silently returns zero sweep candidates. No error. No log.

---

## 1. `NOT IN` → `NOT EXISTS` (mark.rs:110)

`NOT IN` with a subquery containing NULL: SQL three-valued logic says
`x NOT IN (a, NULL)` = `x <> a AND x <> NULL` = `… AND UNKNOWN` = `UNKNOWN`.
`WHERE UNKNOWN` filters to false. Zero rows returned. GC does nothing.

`NOT EXISTS` is NULL-safe: the correlated subquery returns either 0 rows
(EXISTS → false → NOT EXISTS → true, candidate survives) or ≥1 row
(candidate filtered). A NULL in `reachable.store_path` produces a row where
`store_path = n.store_path` is UNKNOWN — the subquery still returns 0 rows
for that match, so NOT EXISTS stays true. No silent swallow.

```diff
--- a/rio-store/src/gc/mark.rs
+++ b/rio-store/src/gc/mark.rs
@@ -106,10 +106,14 @@ pub async fn compute_unreachable(
         SELECT n.store_path_hash
           FROM narinfo n
           JOIN manifests m USING (store_path_hash)
          WHERE m.status = 'complete'
-           AND n.store_path NOT IN (SELECT store_path FROM reachable)
+           -- NOT EXISTS is NULL-safe. NOT IN (…NULL…) = UNKNOWN for every
+           -- row → zero sweep candidates → silent GC-off. reachable's
+           -- seed (d) unnest($2) is Rust-bound (can't be NULL today), but
+           -- this hardens against future CTE changes / migration drift.
+           AND NOT EXISTS (
+             SELECT 1 FROM reachable r WHERE r.store_path = n.store_path
+           )
         "#,
     )
```

Planner note: PG usually rewrites `NOT IN (uncorrelated subquery)` to a hash
anti-join, same plan as `NOT EXISTS`. So no perf regression — verify with
`EXPLAIN` in the test if paranoid, but this is a well-known equivalence.

---

## 2. Bound + validate `extra_roots` (admin.rs:69-79)

`req.extra_roots` goes straight from the wire into `compute_unreachable`
(admin.rs:228) with no size check and no syntax check. The mark CTE runs
under `GC_MARK_LOCK_ID` exclusive (admin.rs:214), which blocks **every**
`PutPath` (they take the shared side at put_path.rs:308). 10M garbage strings
→ mark CTE spins on `unnest` → every upload hangs until the CTE finishes or
the connection times out.

Reuse `MAX_BATCH_PATHS` (10_000 — `grpc/mod.rs:54`). 10k live-build outputs is
already absurd; the scheduler's GcRoots actor sends ~tens in practice. Validate
each with the existing `validate_store_path` helper — `extra_roots` may contain
paths NOT yet in narinfo (in-flight build outputs, per mark.rs:30-34), but they
must still be **syntactically** valid store paths or the CTE's `n.store_path =
r.store_path` join on garbage produces false matches against real paths.

Validate **before** spawning the GC task — fail the RPC synchronously so the
client gets `INVALID_ARGUMENT`, not a buried stream error.

```diff
--- a/rio-store/src/grpc/admin.rs
+++ b/rio-store/src/grpc/admin.rs
@@ -67,8 +67,25 @@ impl rio_proto::StoreAdminService for StoreAdminServiceImpl {
     ) -> Result<Response<Self::TriggerGCStream>, Status> {
         rio_proto::interceptor::link_parent(&request);
         let req = request.into_inner();
+
+        // Bound extra_roots BEFORE spawning. Mark runs under
+        // GC_MARK_LOCK_ID exclusive; a 10M-element array stalls the CTE
+        // on unnest() and blocks every PutPath (shared side of same lock)
+        // for the duration. Reuse MAX_BATCH_PATHS — 10k live-build outputs
+        // is already implausible; GcRoots actor sends ~tens.
+        rio_common::grpc::check_bound(
+            "extra_roots",
+            req.extra_roots.len(),
+            crate::grpc::MAX_BATCH_PATHS,
+        )?;
+        // Syntactically valid store paths only. Not-in-narinfo is fine
+        // (in-flight outputs, mark.rs seed-d handles it); garbage strings
+        // are not — the CTE join on store_path = store_path against junk
+        // is dead weight at best, a false match at worst.
+        for root in &req.extra_roots {
+            crate::grpc::validate_store_path(root)?;
+        }
+
         // proto3 `optional uint32`: None = unset (use default 2h),
```

Also export `MAX_BATCH_PATHS` and `validate_store_path` from `grpc/mod.rs`
(currently module-private — add `pub(crate)` on both, mod.rs:54 and :59).

---

## 3. Weighted byte semaphore on in-flight NAR (put_path.rs:360-395)

`MAX_NAR_SIZE` is 4 GiB (`rio-common/src/limits.rs:12`). Per-request bound is
enforced at put_path.rs:384. No global bound. 10 concurrent 4 GiB uploads =
40 GiB RSS from `nar_data: Vec<u8>` buffers alone. Store OOMs.

### Acquisition point: per-chunk, not trailer

`nar_size` arrives in the trailer (put_path.rs:397-402) **after** all chunks
are buffered. Acquiring permits "after trailer arrives" bounds the hash/store
step but not buffering — the `Vec` is already full. Too late.

**Acquire incrementally per chunk**, before `nar_data.extend_from_slice`:

```rust
let permit = self.nar_bytes_budget.acquire_many(chunk.len() as u32).await
    .map_err(|_| Status::resource_exhausted("NAR buffer budget closed"))?;
nar_data.extend_from_slice(&chunk);
held_permits.push(permit);  // released on drop (handler exit)
```

`acquire_many` takes `u32`; gRPC chunks are ≤ tonic's default 4 MiB message
limit, so `chunk.len() as u32` never truncates. `tokio::sync::Semaphore` max
permits is `usize::MAX >> 3` — a 32 GiB budget (34_359_738_368 permits) fits
comfortably on 64-bit.

### Struct change

```diff
--- a/rio-store/src/grpc/mod.rs
+++ b/rio-store/src/grpc/mod.rs
@@ -103,6 +103,18 @@ pub(crate) fn metadata_status(context: &str, e: metadata::MetadataError) -> Stat
 pub struct StoreServiceImpl {
     pool: PgPool,
     chunk_backend: Option<Arc<dyn ChunkBackend>>,
     chunk_cache: Option<Arc<ChunkCache>>,
     signer: Option<Arc<Signer>>,
     hmac_verifier: Option<Arc<rio_common::hmac::HmacVerifier>>,
+    /// Global budget for in-flight NAR bytes across ALL concurrent PutPath
+    /// handlers. Each handler acquires `chunk.len()` permits before extending
+    /// its `nar_data: Vec<u8>`; permits release on handler drop. Default
+    /// `8 * MAX_NAR_SIZE` (32 GiB) — lets 8× max-size uploads run in parallel
+    /// before the 9th blocks. Configurable via `store.toml nar_buffer_budget_
+    /// bytes` (TODO: plumb config; hardcoded for phase4a).
+    ///
+    /// NOT shared with GetPath's chunk cache — that's moka-bounded separately
+    /// (chunk_cache above). This bounds ONLY the per-request accumulation
+    /// Vec, which is the OOM vector: 10 × 4 GiB = 40 GiB RSS.
+    nar_bytes_budget: Arc<tokio::sync::Semaphore>,
 }
```

Default in `new()` and `with_chunk_cache()`:

```rust
nar_bytes_budget: Arc::new(tokio::sync::Semaphore::new(
    (8 * MAX_NAR_SIZE) as usize
)),
```

`.with_nar_budget(bytes: usize)` builder for tests + future config.

### Accumulation loop diff

```diff
--- a/rio-store/src/grpc/put_path.rs
+++ b/rio-store/src/grpc/put_path.rs
@@ -356,10 +356,16 @@
         // Step 4: Accumulate NAR chunks into a buffer.
-        // Bound accumulation to prevent a malicious/buggy client OOMing us.
-        // nar_size arrives in the trailer, so bound by MAX_NAR_SIZE during
-        // accumulation; the trailer value is checked after.
+        // Two bounds:
+        //   (a) per-request MAX_NAR_SIZE — enforced in-loop below
+        //   (b) GLOBAL nar_bytes_budget — acquired per-chunk, released on
+        //       handler drop. 10 concurrent 4 GiB uploads = 40 GiB RSS; the
+        //       budget backpressures the 9th+ upload instead of OOM.
+        // Permits held in a Vec so drop-on-any-exit releases them. No
+        // scopeguard needed — SemaphorePermit's Drop does the right thing.
         let mut nar_data = Vec::new();
         let mut trailer: Option<rio_proto::types::PutPathTrailer> = None;
+        let mut _held_permits: Vec<tokio::sync::SemaphorePermit<'_>> =
+            Vec::new();
         loop {
             let msg = match stream.message().await {
@@ -381,6 +387,22 @@
                     }
                     let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
                     if new_len > MAX_NAR_SIZE {
                         // ... existing MAX_NAR_SIZE rejection ...
                     }
+                    // Global byte budget. acquire_many(u32) — chunk.len()
+                    // is ≤ tonic's 4 MiB default message cap, so the cast
+                    // never truncates. `await` here backpressures the
+                    // client: if the budget is exhausted, recv stalls,
+                    // gRPC flow control propagates, client send blocks.
+                    // No need to drain_stream on error — acquire_many only
+                    // errs if the semaphore is closed (never happens;
+                    // budget lives for the process), so .expect() is
+                    // correct but we map to resource_exhausted for safety.
+                    let permit = self
+                        .nar_bytes_budget
+                        .acquire_many(chunk.len() as u32)
+                        .await
+                        .map_err(|_| {
+                            Status::resource_exhausted("NAR buffer budget closed")
+                        })?;
+                    _held_permits.push(permit);
                     nar_data.extend_from_slice(&chunk);
                 }
```

**Starvation note:** a slow 4 GiB uploader holding 4 GiB of permits for 5 minutes
doesn't starve small uploads — `acquire_many` is FIFO per tokio's semaphore
fairness, and a small upload only needs ~MB of permits. The bad case (N slow
max-size uploads, all others blocked) is the intended behavior: better than OOM.

---

## 4. Transaction-scoped mark lock + populate placeholder refs (put_path.rs:298)

### The exhaustion

`main.rs:175` sets `.max_connections(20)`. Each PutPath holds one dedicated
`pool.acquire()` from line 298 through line 571 — the full accumulate + hash +
store duration. A 4 GiB upload at 100 MB/s = 40 seconds. 21st concurrent upload
waits on pool checkout → sqlx's default `acquire_timeout` (30s) → fails with
`PoolTimedOut`.

### Why the lock is held that long today

The placeholder narinfo (written by `insert_manifest_uploading`, inline.rs:36-47)
has **empty references** (it's `ON CONFLICT DO NOTHING` with no refs column in
the INSERT). The placeholder protects the **path itself** via mark's seed (b)
(`WHERE m.status = 'uploading'`), but NOT its references — the CTE walks
`narinfo."references"`, which is `'{}'` for the placeholder.

So: upload of A, A references B, B is old with no other root. If mark runs
mid-upload, B is swept. Upload completes, A's narinfo now references a deleted
B. The long-held session lock is preventing exactly this.

### The structural fix

References are known at metadata time (put_path.rs:222-226 bounds them). **Pass
them into `insert_manifest_uploading` and write them into the placeholder.**
Then mark's CTE walks them → references protected from the instant the
placeholder exists. The lock only needs to cover the placeholder insert itself
(atomic, ~ms), which means `pg_try_advisory_xact_lock_shared` inside the
existing tx works.

```diff
--- a/rio-store/src/metadata/inline.rs
+++ b/rio-store/src/metadata/inline.rs
@@ -24,11 +24,27 @@
 #[instrument(skip(pool), fields(store_path_hash = hex::encode(store_path_hash)))]
 pub async fn insert_manifest_uploading(
     pool: &PgPool,
     store_path_hash: &[u8],
     store_path: &str,
+    references: &[String],
 ) -> Result<bool> {
     let mut tx = pool.begin().await?;

+    // Take GC_MARK_LOCK_ID shared, transaction-scoped. Auto-releases at
+    // commit/rollback — no dedicated pool connection, no scopeguard dance.
+    // Mark (admin.rs:214) takes this exclusive; it blocks until no
+    // placeholder-insert tx is in flight. That's ~ms, not the full upload.
+    //
+    // `try` variant returns false if mark is running. Return a retriable
+    // error — the caller re-checks `check_manifest_complete` + retries.
+    // Blocking variant would deadlock against mark's exclusive wait if
+    // the pool is saturated (mark wants a connection, PutPath holds them).
+    let locked: bool = sqlx::query_scalar(
+        "SELECT pg_try_advisory_xact_lock_shared($1)"
+    )
+    .bind(crate::gc::GC_MARK_LOCK_ID)
+    .fetch_one(&mut *tx)
+    .await?;
+    if !locked {
+        return Err(MetadataError::Serialization); // retriable; maps to ABORTED
+    }
+
     // narinfo placeholder first (manifests has FK to narinfo). ON CONFLICT
-    // DO NOTHING: if another uploader already inserted, we don't clobber
-    // their (possibly real, possibly placeholder) row.
+    // DO NOTHING: if another uploader already inserted, we don't clobber.
+    // REFERENCES POPULATED HERE — mark's CTE walks them from the instant
+    // this tx commits. This is what lets the lock be tx-scoped instead of
+    // held for the full upload: the placeholder itself protects its refs.
     sqlx::query(
         r#"
-        INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size)
-        VALUES ($1, $2, $3, 0)
+        INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size,
+                             "references")
+        VALUES ($1, $2, $3, 0, $4)
         ON CONFLICT (store_path_hash) DO NOTHING
         "#,
     )
     .bind(store_path_hash)
     .bind(store_path)
     .bind(&[0u8; 32] as &[u8])
+    .bind(references)
     .execute(&mut *tx)
     .await?;
```

Then **delete** put_path.rs:285-315 and :558-571 (the session-lock acquire /
scopeguard / explicit unlock). `insert_manifest_uploading` call at :322
gains a `&info.references_as_strings()` argument (or whatever accessor
`ValidatedPathInfo` has — likely `.references` with a `.map(|p| p.to_string())`).

**Caller-side retry:** `metadata_status` (mod.rs:84-100) already maps
`MetadataError::Serialization` → `Status::aborted(..retry)`. The existing
concurrent-upload retry at put_path.rs:335-354 doesn't cover this, but tonic
clients retry `ABORTED`. For phase4a that's sufficient; a bounded-backoff
in-handler retry is TODO(phase4b) if mark contention shows up in metrics.

### What about `complete_manifest`?

The complete tx (inline.rs:78, `update_narinfo_complete`) overwrites
references with the same values (they don't change between metadata and
trailer). If a concurrent GC runs between placeholder-commit and
complete-commit, the placeholder's refs protect the closure. If the upload
fails mid-accumulate, `abort_upload` → `delete_manifest_uploading` removes
the placeholder → refs no longer protected → correct (the upload didn't
happen).

One edge: placeholder inserted, upload stalls, orphan sweep reaps it (orphan
timeout), refs become unprotected. That's **correct** — a stalled upload
shouldn't pin its refs forever. The orphan timeout is the intended bound.

---

## 5. Clamp `grace_hours` before `as i32` (mark.rs:113)

`grace_hours: u32` → `.bind(grace_hours as i32)`. `u32` > `i32::MAX`
(2_147_483_647) wraps to negative. `make_interval(hours => -N)` is a negative
interval → `now() - (-N hours)` = `now() + N hours` = **future**. `created_at >
future` is false for everything. Grace period protects nothing. Combined with
no other roots → everything sweepable.

Proto is `optional uint32 grace_period_hours`. `u32::MAX` hours ≈ 489957 years.
Clamp at one year — nobody wants more grace than that, and if they do they're
misusing grace as "never GC" (should be a pin).

```diff
--- a/rio-store/src/gc/mark.rs
+++ b/rio-store/src/gc/mark.rs
@@ -111,7 +111,12 @@ pub async fn compute_unreachable(
            AND NOT EXISTS (…)
         "#,
     )
-    .bind(grace_hours as i32)
+    // Clamp before i32 cast. u32 > i32::MAX wraps negative →
+    // make_interval(hours => negative) → now() - (negative) = future →
+    // grace protects NOTHING → everything sweepable. 24*365 = one year
+    // ceiling; "infinite grace" is a misuse (use PinPath instead).
+    .bind(grace_hours.min(24 * 365) as i32)
     .bind(extra_roots)
```

Also clamp at the gRPC boundary (admin.rs:74) so the log line shows the
effective value, not the requested one. But the **correctness** fix is in
mark.rs — defense in depth against callers that bypass admin.rs.

---

## 6. Validate + scrub errors in PinPath/UnpinPath (admin.rs:320-400)

Two problems per RPC:

1. `req.store_path` goes directly into `sha2::Sha256::digest` (line 329, 387)
   with no `validate_store_path`. Garbage input → garbage hash → FK violation
   → "path not in store". Misleading, and lets an attacker probe for timing
   differences on hash-collision attempts.

2. `format!("pin failed: {e}")` (line 371) and `format!("unpin failed: {e}")`
   (line 393) leak sqlx error chains to the client. Includes PG connection
   string if the pool hiccups. Same anti-pattern `internal_error()` (mod.rs:68)
   was built to kill.

```diff
--- a/rio-store/src/grpc/admin.rs
+++ b/rio-store/src/grpc/admin.rs
@@ -324,6 +324,7 @@ impl rio_proto::StoreAdminService for StoreAdminServiceImpl {
         rio_proto::interceptor::link_parent(&request);
         let req = request.into_inner();
+        crate::grpc::validate_store_path(&req.store_path)?;
         // Compute store_path_hash from the path. narinfo keys on
         // SHA-256 of the store path string.
         use sha2::Digest;
@@ -368,7 +369,9 @@
                         message: format!("path not in store: {}", req.store_path),
                     }))
                 } else {
-                    Err(Status::internal(format!("pin failed: {e}")))
+                    // Log the full sqlx chain server-side; don't leak it.
+                    // Same pattern as grpc::internal_error (mod.rs:68).
+                    Err(crate::grpc::internal_error("PinPath: insert gc_roots", e))
                 }
             }
         }
@@ -384,11 +387,12 @@
         rio_proto::interceptor::link_parent(&request);
         let req = request.into_inner();
+        crate::grpc::validate_store_path(&req.store_path)?;
         use sha2::Digest;
         let hash: Vec<u8> = sha2::Sha256::digest(req.store_path.as_bytes()).to_vec();

         sqlx::query("DELETE FROM gc_roots WHERE store_path_hash = $1")
             .bind(&hash)
             .execute(&self.pool)
             .await
-            .map_err(|e| Status::internal(format!("unpin failed: {e}")))?;
+            .map_err(|e| crate::grpc::internal_error("UnpinPath: delete gc_roots", e))?;
```

Make `internal_error` `pub(crate)` in mod.rs (it's currently private fn at :68).

---

## 7. Bound + dedup cumulative signatures (queries.rs:232-244)

`signatures = signatures || $2` (array concat, queries.rs:234) is unbounded and
non-deduplicating. Per-request bound exists at the RPC layer (mod.rs:402,
`MAX_SIGNATURES = 100`). But across requests: client calls AddSignatures 1000
times with the same sig → `signatures` array grows to 1000 entries → narinfo
row bloats → every `query_path_info` pays for it.

Nix's own `AddSignatures` doesn't dedup (queries.rs:215-218 doc comment), so
we're not breaking client expectations by doing so. Dedup server-side + cap
cumulative at `MAX_SIGNATURES`:

```diff
--- a/rio-store/src/metadata/queries.rs
+++ b/rio-store/src/metadata/queries.rs
@@ -230,11 +230,24 @@ pub async fn append_signatures(pool: &PgPool, store_path: &str, sigs: &[String])
     // uploading (they need the nar_hash), so the manifest is always complete
     // when this is called — but coupling this function to that assumption
     // would break `nix store sign` against a path whose upload got stuck.
+    //
+    // Dedup + bound: array(SELECT DISTINCT unnest(old || new)) dedups;
+    // the WHERE cardinality guard refuses to grow past MAX_SIGNATURES.
+    // Without the guard, 1000 × AddSignatures(same_sig) doesn't bloat the
+    // row (dedup catches it) but 1000 × AddSignatures(different_garbage_sig)
+    // still would. rows_affected = 0 when at cap → caller sees NOT_FOUND,
+    // which is WRONG semantically (path exists, we just refused). Return a
+    // typed error instead:
     let result = sqlx::query(
         r#"
-        UPDATE narinfo SET signatures = signatures || $2
+        UPDATE narinfo
+           SET signatures = array(
+                 SELECT DISTINCT unnest(signatures || $2::text[])
+               )
         WHERE store_path = $1
+          AND cardinality(signatures) < $3
         "#,
     )
     .bind(store_path)
     .bind(sigs)
+    .bind(rio_common::limits::MAX_SIGNATURES as i32)
     .execute(pool)
```

To disambiguate `rows == 0` (not-found vs at-cap), either: (a) pre-query
`SELECT cardinality(signatures) FROM narinfo WHERE store_path = $1`, or
(b) `RETURNING cardinality(signatures)` + check for `None` vs `Some(>=cap)`.
(b) is one round-trip:

```sql
UPDATE narinfo SET signatures = …
WHERE store_path = $1
RETURNING cardinality(signatures) AS new_count
```

`fetch_optional` → `None` = path not found (NOT_FOUND), `Some(count)` where
the previous count was already ≥ MAX = dedup was the only change (accept, OK).
Actually simpler: drop the `WHERE cardinality` guard, rely on dedup alone,
and post-query check `new_count`:

```rust
let row: Option<(i32,)> = sqlx::query_as(r#"
    UPDATE narinfo SET signatures = array(
      SELECT DISTINCT unnest(signatures || $2::text[])
    )
    WHERE store_path = $1
    RETURNING cardinality(signatures)
"#)
.bind(store_path).bind(sigs)
.fetch_optional(pool).await?;

match row {
    None => Ok(0), // not found — caller maps to NOT_FOUND
    Some((n,)) if n > MAX_SIGNATURES as i32 => {
        // Over cap post-dedup → client sent novel sigs and we grew.
        // Truncate or reject. Reject: clearer.
        Err(MetadataError::InvariantViolation(
            "signature count exceeds MAX_SIGNATURES".into()
        ))
    }
    Some(_) => Ok(1),
}
```

The reject-on-overflow is the honest behavior: client knows its sigs didn't
stick. `InvariantViolation` maps to `INTERNAL` via `metadata_status` — change
to a new `TooMany` variant mapping to `RESOURCE_EXHAUSTED` if we want clients
to distinguish.

---

## Tests

### T1. NOT EXISTS survives NULL in reachable

Inject a NULL into the CTE and assert mark still returns the expected
candidate. Can't easily inject via `extra_roots` (Rust `&[String]`), so
fixture-level: temporarily relax the column constraint, or use a raw SQL
seed. Simplest: add a one-off SQL seed to the test that `INSERT INTO
narinfo … VALUES (…, NULL, …)` with a migration override, or — cleaner —
assert at the SQL level:

```rust
// rio-store/src/gc/mark.rs tests
#[tokio::test]
async fn not_exists_survives_null_in_reachable() {
    let db = TestDb::new(&crate::MIGRATOR).await;
    // One old path, no roots → should be unreachable.
    let hash = seed_path(&db.pool, &test_store_path("victim"), &[], 48).await;

    // Force a NULL into the reachable set via extra_roots. Rust &[String]
    // can't carry NULL, so go through SQL: prepared statement with
    // $2 = ARRAY[NULL]::text[]. Use sqlx::query directly with the same
    // query body but manual bind:
    let rows: Vec<(Vec<u8>,)> = sqlx::query_as(COMPUTE_UNREACHABLE_SQL)
        .bind(2_i32)                                     // grace
        .bind(sqlx::types::Json(vec![None::<String>]))   // NULL element
        // ^ actually: bind a PG array with NULL. sqlx needs `&[Option<String>]`.
        .fetch_all(&db.pool).await.unwrap();

    // NOT IN: rows.is_empty() (NULL poisoned → zero candidates). FAILS HERE pre-fix.
    // NOT EXISTS: rows == vec![hash]. Passes post-fix.
    assert_eq!(rows, vec![(hash,)], "NULL in reachable must not poison the sweep");
}
```

Extract the SQL string to a `const COMPUTE_UNREACHABLE_SQL: &str` so the test
reuses exactly the production query. Bind `&[Option<String>]` for the array —
sqlx encodes `None` as SQL NULL inside `text[]`.

### T2. extra_roots bound rejects oversized

```rust
// rio-store/src/grpc/admin.rs tests
#[tokio::test]
async fn trigger_gc_rejects_oversized_extra_roots() {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
    let roots: Vec<String> = (0..10_001)
        .map(|i| format!("/nix/store/{:032}-p{}", "a", i))  // valid syntax
        .collect();
    let err = svc.trigger_gc(Request::new(GcRequest {
        extra_roots: roots, ..Default::default()
    })).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(err.message().contains("extra_roots"));
}

#[tokio::test]
async fn trigger_gc_rejects_malformed_extra_root() {
    // "not-a-store-path" fails validate_store_path
    let err = svc.trigger_gc(Request::new(GcRequest {
        extra_roots: vec!["not-a-store-path".into()], ..Default::default()
    })).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}
```

### T3. 11th upload blocks on budget semaphore

Use `#[tokio::test(start_paused = true)]` — but **no real TCP** (per
lang-gotchas: `start_paused` + real gRPC = spurious timeouts). Drive PutPath
via in-process `StoreServiceImpl::put_path` with a mock `Streaming`.

Actually simpler: test the semaphore in isolation, then integration-test via
a `with_nar_budget(small)` construction:

```rust
// rio-store/src/grpc/put_path.rs tests
#[tokio::test]
async fn nar_budget_backpressures_excess_concurrent() {
    let db = TestDb::new(&crate::MIGRATOR).await;
    // Budget = 10 × 4 KiB; each upload is exactly 4 KiB. 11th should block.
    let svc = StoreServiceImpl::new(db.pool.clone())
        .with_nar_budget(10 * 4096);

    // Start 10 uploads, each sending one 4 KiB chunk then PAUSING (no
    // trailer). They each hold 4096 permits. tokio::spawn each into a
    // JoinSet; they'll all reach the pause point (hung on stream.message()
    // waiting for more).
    //
    // Mock stream: yields one NarChunk(vec![0; 4096]) then pending_forever().
    // Implementation: tokio::sync::mpsc channel, send one chunk, DON'T close.
    let mut set = tokio::task::JoinSet::new();
    let svc = Arc::new(svc);
    let (pause_txs, streams): (Vec<_>, Vec<_>) = (0..10)
        .map(|i| mock_paused_stream(4096, i))
        .unzip();
    for stream in streams {
        let svc = svc.clone();
        set.spawn(async move { svc.put_path(stream).await });
    }

    // Give them time to reach the pause. yield_now × N is fragile;
    // instead, poll svc.nar_bytes_budget.available_permits() until == 0.
    tokio::time::timeout(Duration::from_secs(5), async {
        while svc.nar_bytes_budget.available_permits() > 0 {
            tokio::task::yield_now().await;
        }
    }).await.expect("10 uploads should exhaust the budget");

    // 11th upload: send one 4 KiB chunk. acquire_many should NOT complete
    // within a timeout (it's blocked on the exhausted semaphore).
    let (tx11, stream11) = mock_paused_stream(4096, 10);
    let svc11 = svc.clone();
    let handle11 = tokio::spawn(async move { svc11.put_path(stream11).await });

    // Assert handle11 is NOT ready within 100ms (it's blocked on acquire).
    tokio::select! {
        _ = &mut handle11 => panic!("11th upload should block, not complete"),
        _ = tokio::time::sleep(Duration::from_millis(100)) => {} // expected
    }

    // Release one of the first 10 (drop its pause_tx → stream closes →
    // handler errors on "no trailer" → permits released on drop).
    drop(pause_txs.into_iter().next().unwrap());

    // Now handle11 should make progress (acquire succeeds). It'll also
    // eventually error on "no trailer" but that's fine — we proved the
    // semaphore backpressured then released.
    tokio::select! {
        _ = handle11 => {} // handler ran to completion (with an error; fine)
        _ = tokio::time::sleep(Duration::from_secs(5)) => {
            panic!("11th upload should unblock after one of the 10 released")
        }
    }
}
```

`mock_paused_stream` wraps a `tokio_stream::wrappers::ReceiverStream` in
whatever adapter the existing put_path tests use for `Streaming<PutPathRequest>`
(check `tests/grpc/main.rs` — likely a `tonic::Request::new(stream)` over a
channel).

### T4. Placeholder refs protect closure during upload

Integration: seed old path B (48h ago, no roots). Start PutPath for A
referencing B. After placeholder insert but before trailer, run
`compute_unreachable`. Assert B is NOT in the result.

```rust
#[tokio::test]
async fn placeholder_refs_protect_closure() {
    let db = TestDb::new(&crate::MIGRATOR).await;
    let b = test_store_path("old-dep");
    let _b_hash = seed_path(&db.pool, &b, &[], 48).await; // old, no roots

    // Placeholder for A, referencing B. Direct call — we're testing
    // insert_manifest_uploading's new behavior, not the full gRPC path.
    let a = test_store_path("uploader");
    let a_hash: Vec<u8> = sha2::Sha256::digest(a.as_bytes()).to_vec();
    let inserted = insert_manifest_uploading(
        &db.pool, &a_hash, &a, &[b.clone()]
    ).await.unwrap();
    assert!(inserted);

    // B is now protected via A's placeholder refs → seed (b) → CTE walk.
    let unreachable = compute_unreachable(&db.pool, 2, &[]).await.unwrap();
    assert!(unreachable.is_empty(),
        "B should be protected by A's placeholder references");
}
```

### T5. PinPath rejects garbage, scrubs sqlx

```rust
#[tokio::test]
async fn pin_path_rejects_malformed() {
    let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
    let err = svc.pin_path(Request::new(PinPathRequest {
        store_path: "../../etc/passwd".into(), source: "t".into(),
    })).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

#[tokio::test]
async fn pin_path_internal_error_is_scrubbed() {
    // Force a non-FK DB error. Easiest: close the pool, then call.
    // Or: use a pool pointing at a stopped postgres.
    let db = TestDb::new(&crate::MIGRATOR).await;
    let svc = StoreAdminServiceImpl::new(db.pool.clone(), None);
    db.pool.close().await; // all acquires fail
    // Seed nothing; this path would FK-violate, but pool-closed errs first.
    let err = svc.pin_path(Request::new(PinPathRequest {
        store_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-p".into(),
        source: "t".into(),
    })).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::Internal);
    // The message must NOT contain sqlx/postgres internals.
    assert!(!err.message().contains("sqlx"));
    assert!(!err.message().contains("PoolTimedOut"));
    assert_eq!(err.message(), "storage operation failed"); // internal_error's generic
}
```

### T6. Signature dedup + cap

```rust
#[tokio::test]
async fn append_signatures_dedups() {
    // seed_complete(…), then append same sig twice
    let _ = append_signatures(&db.pool, &path, &["sig:abc".into()]).await.unwrap();
    let _ = append_signatures(&db.pool, &path, &["sig:abc".into()]).await.unwrap();
    let (sigs,): (Vec<String>,) = sqlx::query_as(
        "SELECT signatures FROM narinfo WHERE store_path = $1"
    ).bind(&path).fetch_one(&db.pool).await.unwrap();
    assert_eq!(sigs.len(), 1, "dedup should collapse repeated sig");
}
```

---

## Checklist

- [ ] `mark.rs:110` — `NOT IN` → `NOT EXISTS (SELECT 1 FROM reachable r WHERE r.store_path = n.store_path)`
- [ ] `mark.rs:113` — `.bind(grace_hours.min(24 * 365) as i32)`
- [ ] `mark.rs` — extract SQL to `const COMPUTE_UNREACHABLE_SQL: &str` for T1 reuse
- [ ] `admin.rs:69` — `check_bound("extra_roots", …, MAX_BATCH_PATHS)` + `validate_store_path` loop, before spawn
- [ ] `admin.rs:324,385` — `validate_store_path(&req.store_path)?` at top of PinPath/UnpinPath
- [ ] `admin.rs:371,393` — `internal_error("PinPath…", e)` / `internal_error("UnpinPath…", e)` instead of `format!("{e}")`
- [ ] `grpc/mod.rs:54,59,68` — `pub(crate)` on `MAX_BATCH_PATHS`, `validate_store_path`, `internal_error`
- [ ] `grpc/mod.rs:106` — add `nar_bytes_budget: Arc<Semaphore>` field; init to `8 * MAX_NAR_SIZE` in `new()` / `with_chunk_cache()`; add `.with_nar_budget(usize)` builder
- [ ] `put_path.rs:360` — `_held_permits: Vec<SemaphorePermit>` + per-chunk `acquire_many(chunk.len() as u32)` before `extend_from_slice`
- [ ] `put_path.rs:285-315,558-571` — delete session-lock acquire + scopeguard + unlock
- [ ] `put_path.rs:322` — pass `&info.references` (stringified) to `insert_manifest_uploading`
- [ ] `metadata/inline.rs:26` — add `references: &[String]` param; `pg_try_advisory_xact_lock_shared` inside tx; populate `"references"` column in placeholder INSERT; return `MetadataError::Serialization` on lock-not-acquired
- [ ] `metadata/chunked.rs`, `metadata/mod.rs` — update `insert_manifest_uploading` call sites (test fixtures at chunked.rs:328,368; mod.rs:479)
- [ ] `queries.rs:232` — `array(SELECT DISTINCT unnest(signatures || $2))` + `RETURNING cardinality`; over-cap → `MetadataError` (new variant or `InvariantViolation`)
- [ ] Tests T1–T6 above
- [ ] `tracey query rule store.gc.two-phase` — add `r[verify]` on T1 (NULL-safety is part of mark correctness)
- [ ] `observability.md` — `rio_store_nar_budget_available_permits` gauge (optional; expose via `semaphore.available_permits()` in a background loop)
