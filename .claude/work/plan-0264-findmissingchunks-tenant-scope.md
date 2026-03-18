# Plan 0264: FindMissingChunks per-tenant scoping (CUTTABLE)

Scope `FindMissingChunks` by tenant: tenant A's worker can only dedup against tenant A's chunks, not tenant B's. Per `phase5.md:23`: "optional, at the cost of dedup savings."

**CUTTABLE — zero downstream deps.** Coordinator cuts on schedule slip.

## Entry criteria

- [P0263](plan-0263-worker-client-side-chunker.md) merged (chunker exists, something to scope)
- [P0259](plan-0259-jwt-verify-middleware.md) merged (`Claims.sub` available in request extensions)

## Tasks

### T1 — `feat(migrations):` chunks.tenant_id column

NEW `migrations/017_chunks_tenant_id.sql` (renumber at dispatch — `ls migrations/ | sort -V | tail -1`):

```sql
ALTER TABLE chunks ADD COLUMN tenant_id UUID NULL REFERENCES tenants(tenant_id);
CREATE INDEX chunks_tenant_idx ON chunks(tenant_id, chunk_hash);
```

NULL allowed for backward-compat (existing chunks have no tenant).

### T2 — `feat(store):` scope FindMissingChunks query

MODIFY [`rio-store/src/grpc/chunk.rs`](../../rio-store/src/grpc/chunk.rs):

```rust
let tenant_id = request.extensions().get::<Claims>().map(|c| c.sub);
let where_clause = match tenant_id {
    Some(tid) => "WHERE tenant_id = $1 OR tenant_id IS NULL",  // shared chunks visible
    None => "",  // anonymous: see all (backward-compat)
};
```

### T3 — `feat(store):` PutChunk records tenant_id

MODIFY same file — `PutChunk` INSERT includes `tenant_id` from `Claims`.

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
  {"path": "migrations/017_chunks_tenant_id.sql", "action": "NEW", "note": "T1: tenant_id column (renumber at dispatch)"},
  {"path": "rio-store/src/grpc/chunk.rs", "action": "MODIFY", "note": "T2+T3: scope FindMissing + record tenant on PutChunk"},
  {"path": "rio-store/src/metadata/chunked.rs", "action": "MODIFY", "note": "T2: query change"}
]
```

```
migrations/
└── 017_chunks_tenant_id.sql      # T1 (renumber at dispatch)
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
