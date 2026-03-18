# Plan 0198: Rem-19 — Store GC robustness: NOT EXISTS, byte semaphore, placeholder refs, grace clamp

## Design

**P1 (HIGH).** Seven fixes bundled; one commit. Structural defense against GC sweep catastrophe.

1. **`mark.rs`: `NOT IN` → `NOT EXISTS`.** `NOT IN (…NULL…)` = `UNKNOWN` for every row → zero sweep candidates → silent GC-off. `NOT EXISTS` is NULL-safe. Seed (d) `unnest($2)` can't bind NULL today (Rust `&[String]`), but this hardens against future CTE/migration drift. SQL extracted to const so tests can bind `&[Option<String>]` directly.

2. **`admin.rs`: bound + validate `extra_roots` BEFORE spawning GC task.** Mark runs under `GC_MARK_LOCK_ID` exclusive; 10M garbage strings stalls the CTE on `unnest()` and blocks every `PutPath` for the duration. Reuse `MAX_BATCH_PATHS=10k`; `GcRoots` actor sends ~tens in practice.

3. **Global `nar_bytes_budget` semaphore:** `MAX_NAR_SIZE` = 4 GiB per-request; no global bound → 10 concurrent = 40 GiB RSS → OOM. Per-chunk `acquire_many(chunk.len())` before `extend_from_slice`; permits drop on handler exit. Default `8 × MAX_NAR_SIZE` (32 GiB).

4. **STRUCTURAL:** pass `references` into `insert_manifest_uploading`, populate placeholder's `references` column. Mark's CTE walks them from commit → closure protected WITHOUT holding a session lock for the full upload. Replace the ~60-line session-lock/scopeguard/unlock dance with a transaction.

5-7: Grace clamp (prevent negative-grace integer underflow), orphan sweep bounds, `content_index` UNIQUE constraint check.

Remediation doc: `docs/src/remediations/phase4a/19-store-gc-robustness.md` (770 lines).

## Files

```json files
[
  {"path": "rio-store/src/gc/mark.rs", "action": "MODIFY", "note": "NOT IN → NOT EXISTS; SQL extracted to const"},
  {"path": "rio-store/src/gc/orphan.rs", "action": "MODIFY", "note": "orphan sweep bounds"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "bound extra_roots BEFORE spawn; grace clamp"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "nar_bytes_budget semaphore wiring"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "per-chunk acquire_many before extend_from_slice"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "references in placeholder; session-lock dance → tx"},
  {"path": "rio-store/src/metadata/inline.rs", "action": "MODIFY", "note": "insert_manifest_uploading references param"},
  {"path": "rio-store/src/metadata/chunked.rs", "action": "MODIFY", "note": "same"},
  {"path": "rio-store/src/metadata/queries.rs", "action": "MODIFY", "note": "CTE walk"},
  {"path": "rio-store/src/content_index.rs", "action": "MODIFY", "note": "UNIQUE constraint check"},
  {"path": "rio-store/src/cache_server/mod.rs", "action": "MODIFY", "note": "budget semaphore plumbing"},
  {"path": "rio-store/tests/grpc/hash_part.rs", "action": "MODIFY", "note": "regression test"}
]
```

## Tracey

- `r[verify store.gc.two-phase]` — `261e78c`

1 marker annotation. (The VM e2e in P0181 adds another `# r[verify store.gc.two-phase]`.)

## Entry

- Depends on P0148: phase 3b complete (extends 3b GC infrastructure)

## Exit

Merged as `70ca57f` (plan doc) + `261e78c` (fix). `.#ci` green. `NOT IN (NULL)` → zero candidates proved in test; `NOT EXISTS` passes same test.
