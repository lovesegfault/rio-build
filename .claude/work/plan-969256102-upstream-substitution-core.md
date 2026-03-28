# Plan 969256102: Upstream substitution — core (fetch, ingest, sig-gated visibility)

[P969256101](plan-969256101-upstream-substitution-foundation.md) landed the `tenant_upstreams` table, proto extensions, and `NarInfo::verify_sig`. This plan wires the actual substitution: an HTTP narinfo/NAR fetcher, per-tenant upstream lookup, sig-mode handling, and the critical cross-tenant sig-visibility gate. After this, `QueryPathInfo` and `GetPath` transparently substitute on miss; `FindMissingPaths` reports which missing paths are upstream-available.

**Second of four.** [P969256103](plan-969256103-upstream-substitution-surface.md) adds CLI/gateway/helm; [P969256104](plan-969256104-upstream-substitution-validation.md) adds VM validation.

## Entry criteria

- [P969256101](plan-969256101-upstream-substitution-foundation.md) merged — `tenant_upstreams` table exists, `FindMissingPathsResponse.substitutable_paths` field exists, `NarInfo::verify_sig` callable

## Tasks

### T1 — `feat(rio-store):` metadata/upstreams.rs — per-tenant upstream lookup

New module at [`rio-store/src/metadata/upstreams.rs`](../../rio-store/src/metadata/upstreams.rs) mirroring [`tenant_keys.rs`](../../rio-store/src/metadata/tenant_keys.rs) (per-tenant config accessor, kept out of hot `db.rs`):

```rust
pub struct Upstream {
    pub id: i32,
    pub url: String,
    pub priority: i32,
    pub trusted_keys: Vec<String>,
    pub sig_mode: SigMode,
}

#[derive(sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "lowercase")]
pub enum SigMode { Keep, Add, Replace }

/// Fetch all upstreams for a tenant, ordered by priority.
pub async fn list_for_tenant(pool: &PgPool, tenant_id: Uuid)
    -> Result<Vec<Upstream>, MetadataError>;

/// Union of all trusted_keys across a tenant's upstreams.
/// Used by the sig-visibility gate.
pub async fn tenant_trusted_keys(pool: &PgPool, tenant_id: Uuid)
    -> Result<Vec<String>, MetadataError>;

/// CRUD for the StoreAdmin RPCs.
pub async fn insert(pool: &PgPool, tenant_id: Uuid, req: &AddUpstreamRequest)
    -> Result<Upstream, MetadataError>;
pub async fn delete(pool: &PgPool, tenant_id: Uuid, url: &str)
    -> Result<u64, MetadataError>;
```

Wire into [`metadata/mod.rs`](../../rio-store/src/metadata/mod.rs) with `pub mod upstreams;`.

### T2 — `feat(rio-store):` substitute.rs — block-and-fetch upstream NAR ingest

New module at [`rio-store/src/substitute.rs`](../../rio-store/src/substitute.rs):

```rust
pub struct Substituter {
    pool: PgPool,
    chunk_backend: Option<Arc<dyn ChunkBackend>>,
    http: reqwest::Client,
}

impl Substituter {
    /// Try to substitute `store_path` from any of `tenant_id`'s upstreams.
    /// Returns Some(PathInfo) if fetched+ingested, None if no upstream has it.
    ///
    /// Flow per-upstream (priority order):
    /// 1. GET {url}/{hash}.narinfo
    /// 2. NarInfo::parse + verify_sig(upstream.trusted_keys) — reject if None
    /// 3. GET {url}/{narinfo.url} (the NAR, possibly .xz/.zst compressed)
    /// 4. Decompress, stream into cas::put_chunked (same path as PutPath:551)
    /// 5. Apply sig_mode:
    ///    - Keep: store upstream sigs via append_signatures(queries.rs:270)
    ///    - Add:  upstream sigs + fresh Signer::sign()
    ///    - Replace: only fresh sig
    /// 6. Return the PathInfo
    pub async fn try_substitute(&self, tenant_id: Uuid, store_path: &str)
        -> Result<Option<PathInfo>, SubstituteError>;

    /// Batch check: which of `paths` exist on ANY of the tenant's upstreams.
    /// HEAD requests only — no NAR download. Feeds substitutable_paths.
    pub async fn check_available(&self, tenant_id: Uuid, paths: &[String])
        -> Result<Vec<String>, SubstituteError>;
}
```

Reuse: [`NarInfo::parse`](../../rio-nix/src/narinfo.rs) at `:81` for step 2; [`cas::put_chunked`](../../rio-store/src/grpc/put_path.rs) at `:551` for step 4; [`append_signatures`](../../rio-store/src/metadata/queries.rs) at `:270` for step 5's dedup-append. Add [`reqwest`](https://docs.rs/reqwest) with `stream` feature if not already a dep. Metrics: `rio_store_substitute_total{result,upstream}`, `rio_store_substitute_bytes_total`, `rio_store_substitute_duration_seconds`.

### T3 — `feat(rio-store):` signing.rs — any_sig_trusted verification helper

At [`rio-store/src/signing.rs`](../../rio-store/src/signing.rs), add a verification counterpart to the existing `Signer`:

```rust
/// Check if any of `sigs` (narinfo `Sig:` lines) verifies against
/// any of `trusted_keys`. Returns the matching key name or None.
///
/// This is the cross-tenant visibility gate: called from
/// query_path_info with the REQUESTING tenant's trusted-keys
/// union, not the substituting tenant's.
pub fn any_sig_trusted(
    sigs: &[String],
    trusted_keys: &[String],
    fingerprint: &str,
) -> Option<String>;
```

Thin wrapper delegating to `NarInfo::verify_sig` logic (or factor the common ed25519-verify loop into `rio-nix` and call it from both).

### T4 — `feat(rio-store):` grpc/mod.rs — QueryPathInfo/GetPath substitute-on-miss hook

At [`rio-store/src/grpc/mod.rs:542`](../../rio-store/src/grpc/mod.rs) (`query_path_info`), insert between `fetch_optional` returning `None` and the `NotFound` error:

```rust
// Not in local store — try upstream substitution if tenant has upstreams configured.
if let Some(tenant_id) = request_tenant_id(&request) {
    if let Some(info) = self.substituter.try_substitute(tenant_id, &req.store_path).await? {
        return Ok(Response::new(info.into()));
    }
}
```

Same pattern at `get_path_impl` entry (in [`grpc/get_path.rs`](../../rio-store/src/grpc/get_path.rs)).

**Sig-visibility gate** — when `query_path_info` DOES find a local row: if the row was substituted (detectable via a new `narinfo.substituted_by` UUID column OR by checking `path_tenants` doesn't include the requesting tenant), call `any_sig_trusted(row.signatures, tenant_trusted_keys(requesting_tenant), fingerprint)` and return `NotFound` if `None`. This is `r[store.substitute.tenant-sig-visibility]`.

### T5 — `feat(rio-store):` grpc/mod.rs — FindMissingPaths populate substitutable_paths

At [`rio-store/src/grpc/mod.rs:563`](../../rio-store/src/grpc/mod.rs) (`find_missing_paths`), after computing `missing`:

```rust
let substitutable = if let Some(tid) = request_tenant_id(&request) {
    self.substituter.check_available(tid, &missing).await.unwrap_or_default()
} else {
    Vec::new()
};
Ok(Response::new(FindMissingPathsResponse {
    missing_paths: missing,
    substitutable_paths: substitutable,
}))
```

### T6 — `feat(rio-store):` grpc/admin.rs — ListUpstreams/AddUpstream/RemoveUpstream handlers

At [`rio-store/src/grpc/admin.rs`](../../rio-store/src/grpc/admin.rs), add the three handlers wired to `upstreams::list_for_tenant`/`insert`/`delete`. Validate `sig_mode` is one of `keep|add|replace`, `url` is https, `trusted_keys` entries match `name:base64` format.

## Exit criteria

- `/nixbuild .#ci` green
- `Substituter::try_substitute` integration test: seed a fake upstream (wiremock or local http server), configure one `tenant_upstreams` row, call `try_substitute`, verify the path lands in `narinfo` + `manifests` with correct sigs per `sig_mode`
- Cross-tenant visibility test: tenant A substitutes path P signed by key K; tenant B with K in `trusted_keys` sees P; tenant C without K gets `NotFound`
- `FindMissingPaths` returns non-empty `substitutable_paths` when upstream has missing paths
- `rio_store_substitute_total` metric increments on substitution

## Tracey

References existing markers (added by P969256101):
- `r[store.substitute.upstream]` — T2, T4 implement (block-and-fetch flow)
- `r[store.substitute.sig-mode]` — T2 implements (keep/add/replace branching)
- `r[store.substitute.tenant-sig-visibility]` — T3, T4 implement (the `any_sig_trusted` gate)

References pre-existing markers:
- `r[store.signing.fingerprint]` — T3 reuses fingerprint for verification
- `r[store.singleflight]` — T2 SHOULD coalesce concurrent substitutions of the same path (same singleflight machinery as GetPath)

## Files

```json files
[
  {"path": "rio-store/src/metadata/upstreams.rs", "action": "NEW", "note": "T1: per-tenant upstream lookup"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "T1: pub mod upstreams"},
  {"path": "rio-store/src/substitute.rs", "action": "NEW", "note": "T2: block-and-fetch Substituter"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "T2: pub mod substitute"},
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T3: any_sig_trusted"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T4,T5: substitute hooks"},
  {"path": "rio-store/src/grpc/get_path.rs", "action": "MODIFY", "note": "T4: GetPath substitute-on-miss"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "T6: upstream CRUD handlers"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "T2: construct Substituter, pass to StoreServiceImpl"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "T2: reqwest dep (if missing)"}
]
```

```
rio-store/src/
├── metadata/
│   ├── upstreams.rs          # T1: NEW — lookup/CRUD
│   └── mod.rs                # T1: re-export
├── substitute.rs             # T2: NEW — Substituter
├── lib.rs                    # T2: mod decl
├── signing.rs                # T3: any_sig_trusted
├── grpc/
│   ├── mod.rs                # T4,T5: hooks at :542/:563
│   ├── get_path.rs           # T4: miss hook
│   └── admin.rs              # T6: CRUD handlers
├── main.rs                   # T2: wire Substituter
└── Cargo.toml                # T2: reqwest
```

## Dependencies

```json deps
{"deps": [969256101], "soft_deps": [], "note": "needs migration 026 + proto types + verify_sig"}
```

**Depends on:** [P969256101](plan-969256101-upstream-substitution-foundation.md) — `tenant_upstreams` table, `substitutable_paths` proto field, `NarInfo::verify_sig`.

**Conflicts with:** [`grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) (21 collisions) and [`main.rs`](../../rio-store/src/main.rs) (33 collisions) are hot — serialize with concurrent store work. `substitute.rs` and `upstreams.rs` are new files, zero collision.
