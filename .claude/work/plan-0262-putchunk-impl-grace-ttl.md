# Plan 0262: PutChunk impl + grace-TTL refcount (closes chunk.rs:62 TODO)

Per GT6: proto already exists. [`types.proto:585`](../../rio-proto/proto/types.proto) has `PutChunkRequest/Metadata/Response`; [`store.proto:67`](../../rio-proto/proto/store.proto) has `rpc PutChunk(stream)`. Server stub at [`chunk.rs:50-73`](../../rio-store/src/grpc/chunk.rs) returns `Status::unimplemented` with `TODO(phase5)` at `:62`. `FindMissingChunks` already fully implemented. **Zero proto work** — fill the server stub only.

Grace-TTL: a chunk with no manifest reference is held for `grace_seconds` before GC eligibility. Prevents a race where a worker's `PutChunk` arrives before its `PutPath` manifest (the chunk would be immediately GC-eligible otherwise).

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (`r[store.chunk.put-standalone]` + `r[store.chunk.grace-ttl]` seeded)

## Tasks

### T1 — `feat(store):` implement PutChunk stream handler

MODIFY [`rio-store/src/grpc/chunk.rs`](../../rio-store/src/grpc/chunk.rs) at `:62` — replace `Status::unimplemented`:

```rust
// r[impl store.chunk.put-standalone]
async fn put_chunk(
    &self,
    request: Request<Streaming<PutChunkRequest>>,
) -> Result<Response<PutChunkResponse>, Status> {
    let mut stream = request.into_inner();
    let mut metadata = None;
    let mut buf = Vec::new();
    while let Some(chunk) = stream.message().await? {
        match chunk.payload {
            Some(Payload::Metadata(m)) => metadata = Some(m),
            Some(Payload::Data(d)) => buf.extend_from_slice(&d),
            None => {}
        }
    }
    let meta = metadata.ok_or_else(|| Status::invalid_argument("missing metadata frame"))?;
    // Verify hash matches declared chunk_hash
    let computed = blake3::hash(&buf);
    if computed.as_bytes() != meta.chunk_hash.as_slice() {
        return Err(Status::invalid_argument("chunk hash mismatch"));
    }
    self.backend.put_chunk(&meta.chunk_hash, &buf).await
        .map_err(|e| Status::internal(format!("chunk store: {e}")))?;
    // created_at is set to now() by the INSERT — grace-TTL countdown starts
    Ok(Response::new(PutChunkResponse { chunk_hash: meta.chunk_hash }))
}
```

### T2 — `feat(store):` grace-TTL GC policy

MODIFY [`rio-store/src/gc/sweep.rs`](../../rio-store/src/gc/sweep.rs):

```rust
// r[impl store.chunk.grace-ttl]
// Chunks eligible for GC iff: zero manifest references AND created_at
// older than grace period. The grace period prevents a race where
// PutChunk arrives before PutPath (chunk briefly orphaned).
const CHUNK_GRACE_SECS: i64 = 300;  // 5 minutes

let orphaned_chunks = sqlx::query!(
    "SELECT chunk_hash FROM chunks c
     WHERE NOT EXISTS (SELECT 1 FROM manifest_chunks mc WHERE mc.chunk_hash = c.chunk_hash)
       AND c.created_at < now() - make_interval(secs => $1)",
    CHUNK_GRACE_SECS
).fetch_all(&pool).await?;
```

**Verify at dispatch:** `grep 'created_at' migrations/*.sql | grep chunks` — if the column is absent, add a hunk to [P0249](plan-0249-migration-batch-014-015-016.md) (coordinate via followup).

### T3 — `test(store):` PutChunk roundtrip + grace respects TTL

```rust
// r[verify store.chunk.put-standalone]
#[tokio::test]
async fn put_chunk_persists_and_verifies_hash() { ... }

// r[verify store.chunk.grace-ttl]
#[tokio::test]
async fn orphan_chunk_within_grace_not_swept() {
    // PutChunk, no manifest. GC sweep immediately → chunk survives.
    // Advance clock past grace. GC sweep → chunk deleted.
}
```

## Exit criteria

- `/nbr .#ci` green
- `rg 'TODO\(phase5\)' rio-store/src/grpc/chunk.rs` → 0
- `nix develop -c tracey query rule store.chunk.put-standalone` shows impl + verify
- `nix develop -c tracey query rule store.chunk.grace-ttl` shows impl + verify

## Tracey

References existing markers:
- `r[store.chunk.put-standalone]` — T1 implements, T3 verifies (seeded by P0245, sibling to `r[store.chunk.refcount-txn]`)
- `r[store.chunk.grace-ttl]` — T2 implements, T3 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-store/src/grpc/chunk.rs", "action": "MODIFY", "note": "T1: replace Status::unimplemented at :62 with real stream handler"},
  {"path": "rio-store/src/gc/sweep.rs", "action": "MODIFY", "note": "T2: grace-TTL policy (serial after 4b P0207 — 6th UNION arm)"}
]
```

```
rio-store/src/
├── grpc/chunk.rs                 # T1: PutChunk handler (closes :62 TODO)
└── gc/sweep.rs                   # T2: grace-TTL
```

## Dependencies

```json deps
{"deps": [245], "soft_deps": [207], "note": "GT6: zero proto work. chunk.rs no 4b/4c touch. sweep.rs soft-serial after 4b P0207 (6th UNION arm) — verify at dispatch. HIDDEN DEP: chunks.created_at column — grep at dispatch, add to P0249 if absent."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — markers seeded.
**Conflicts with:** `chunk.rs` no 4b/4c touch. `sweep.rs` soft-serial after 4b [P0207](plan-0207-mark-cte-tenant-retention-quota.md) (6th UNION arm — verify at dispatch).
