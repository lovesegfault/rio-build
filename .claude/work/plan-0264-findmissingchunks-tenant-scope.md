# Plan 0264: FindMissingChunks per-tenant scoping (CUTTABLE)

Scope `FindMissingChunks` by tenant: tenant A's worker can only dedup against tenant A's chunks, not tenant B's. Per `phase5.md:23`: "optional, at the cost of dedup savings."

**CUTTABLE — zero downstream deps.** Coordinator cuts on schedule slip.

## Entry criteria

- [P0263](plan-0263-worker-client-side-chunker.md) merged (chunker exists, something to scope)
- [P0259](plan-0259-jwt-verify-middleware.md) merged (`Claims.sub` available in request extensions)

## Tasks

### T1 — `feat(migrations):` chunk_tenants junction table (audit Batch A #5)

**Audit finding:** single `tenant_id` column on `chunks` is WRONG. Chunks are content-addressed + `ON CONFLICT DO UPDATE`. Tenant A uploads libc.so, tenant B uploads identical bytes → single column = one tenant owns it, B either overwrites A's ownership or is told "missing" forever. Many-to-many needs a junction. `path_tenants` (migration 009) is the precedent.

NEW `migrations/017_chunk_tenants.sql` (renumber at dispatch):

```sql
-- Many-to-many: same bytes uploaded by multiple tenants (glibc is in
-- every closure). INSERT ON CONFLICT DO NOTHING — no overwrite race.
-- Matches path_tenants precedent (009_phase4.sql).
CREATE TABLE chunk_tenants (
    blake3_hash BYTEA NOT NULL REFERENCES chunks(blake3_hash) ON DELETE CASCADE,
    tenant_id   UUID  NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    PRIMARY KEY (blake3_hash, tenant_id)
);
-- FindMissingChunks: "which of these hashes does tenant X NOT see?"
CREATE INDEX chunk_tenants_tenant_idx ON chunk_tenants(tenant_id, blake3_hash);
```

### T2 — `feat(store):` scope FindMissingChunks query via junction

MODIFY [`rio-store/src/grpc/chunk.rs`](../../rio-store/src/grpc/chunk.rs):

```rust
let tenant_id = request.extensions().get::<Claims>().map(|c| c.sub);
// With tenant: chunk is "present" if it's in chunk_tenants for this tenant
// OR it predates tenancy (no junction row at all = shared legacy).
// Audit C #29: FAIL-CLOSED. After P0259, the interceptor is wired globally
// in store/main.rs — Claims is always attached. None = wiring bug (forgot
// .layer(jwt_interceptor) on the Server). Loud error, not silent cross-tenant leak.
let missing: Vec<_> = match tenant_id {
    Some(tid) => sqlx::query_scalar!(
        "SELECT h FROM UNNEST($1::bytea[]) AS h
         WHERE NOT EXISTS (SELECT 1 FROM chunk_tenants
                           WHERE blake3_hash = h AND tenant_id = $2)",
        &hashes, tid
    ).fetch_all(&pool).await?,
    None => return Err(Status::unauthenticated(
        "tenant-scoped FindMissingChunks requires auth (jwt_interceptor not wired?)"
    )),
};
```

### T3 — `feat(store):` PutChunk records tenant in junction

MODIFY same file — `PutChunk` does `INSERT INTO chunk_tenants (blake3_hash, tenant_id) VALUES ($1, $2) ON CONFLICT DO NOTHING` after the chunk itself is upserted. Second upload of same bytes by same tenant = idempotent; by DIFFERENT tenant = new junction row.

### T4 — `test(store):` tenant A cannot dedup against tenant B

```rust
#[tokio::test]
async fn find_missing_scoped_by_tenant() {
    // Tenant A PutChunk(hash=X). Tenant B FindMissingChunks([X]) → X is MISSING for B.
}
```

## Exit criteria

- `/nbr .#ci` green

## Tracey

none — optional optimization. No marker.

## Files

```json files
[
  {"path": "migrations/017_chunk_tenants.sql", "action": "NEW", "note": "T1: junction table (audit Batch A #5; renumber at dispatch)"},
  {"path": "rio-store/src/grpc/chunk.rs", "action": "MODIFY", "note": "T2+T3: scope FindMissing + record tenant on PutChunk"},
  {"path": "rio-store/src/metadata/chunked.rs", "action": "MODIFY", "note": "T2: query change"}
]
```

```
migrations/
└── 017_chunk_tenants.sql         # T1 junction (renumber at dispatch)
rio-store/src/
├── grpc/chunk.rs                 # T2+T3
└── metadata/chunked.rs           # T2
```

## Dependencies

```json deps
{"deps": [263, 259], "soft_deps": [], "note": "CUTTABLE — zero downstream. phase5.md:23 'optional at cost of dedup savings'. Coordinator cuts on schedule slip."}
```

**Depends on:** [P0263](plan-0263-worker-client-side-chunker.md) — chunker exists. [P0259](plan-0259-jwt-verify-middleware.md) — `Claims.sub` in extensions.
**Conflicts with:** `chunk.rs` serial after P0262 (transitive via P0263).
