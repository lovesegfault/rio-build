# Plan 0461: Upstream substitution — foundation (migration + proto + narinfo verify)

rio-store currently has no upstream-cache substitution: a cold-store `cargo xtask k8s rsb p#hello` must either pre-seed the store or build everything from source. This plan lays the schema and wire foundation for per-tenant configurable upstream caches (cache.nixos.org etc.) with signature-gated cross-tenant visibility.

This is the **first of four** plans for the feature. It lands the additive pieces that have no behavior on their own: migration 026 (`tenant_upstreams` table), proto extensions (`FindMissingPathsResponse.substitutable_paths` + StoreAdmin upstream-CRUD RPCs), and `rio-nix` narinfo signature verification. [P0462](plan-0462-upstream-substitution-core.md) wires the core fetch logic; [P0463](plan-0463-upstream-substitution-surface.md) surfaces it via CLI/gateway/helm; [P0464](plan-0464-upstream-substitution-validation.md) adds the VM test.

The design is block-and-fetch (synchronous substitution inside `QueryPathInfo`/`GetPath` miss-handling), per-tenant upstream lists, configurable `sig_mode` (keep/add/replace), and signature-based cross-tenant visibility — a path substituted by tenant A is visible to tenant B only if B's trusted-keys list covers one of the path's signatures.

## Tasks

### T1 — `feat(migrations):` 026_tenant_upstreams — per-tenant upstream cache config

New migration at [`migrations/026_tenant_upstreams.sql`](../../migrations/026_tenant_upstreams.sql) following the [`tenant_keys`](../../rio-store/src/metadata/tenant_keys.rs) precedent (per-tenant config table, cascade-delete on tenant removal):

```sql
CREATE TABLE tenant_upstreams (
    id             SERIAL PRIMARY KEY,
    tenant_id      UUID NOT NULL REFERENCES tenants(id) ON DELETE CASCADE,
    url            TEXT NOT NULL,          -- https://cache.nixos.org
    priority       INT  NOT NULL DEFAULT 50, -- lower = tried first
    trusted_keys   TEXT[] NOT NULL DEFAULT '{}',  -- key-name:base64pubkey
    sig_mode       TEXT NOT NULL DEFAULT 'keep'
                   CHECK (sig_mode IN ('keep','add','replace')),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, url)
);
CREATE INDEX tenant_upstreams_tenant_idx ON tenant_upstreams(tenant_id, priority);
```

`sig_mode` controls what signatures land in `narinfo.signatures` after substitution: `keep` stores the upstream's sigs as-is, `add` stores upstream sigs PLUS a fresh rio-signed sig, `replace` stores only the rio sig. The `trusted_keys` array is the tenant's allowlist for accepting upstream narinfo (reuse [`narinfo.signatures TEXT[]`](../../migrations/002_store.sql) shape — same `key-name:base64` format).

Per [CLAUDE.md § Migration files are frozen](../../CLAUDE.md): after writing the SQL, run `cargo test -p rio-store --test migrations`, copy the SHA into `PINNED` at [`rio-store/tests/migrations.rs`](../../rio-store/tests/migrations.rs). Add the `M_026` doc-const to [`rio-store/src/migrations.rs`](../../rio-store/src/migrations.rs) with rationale.

### T2 — `feat(proto):` FindMissingPathsResponse.substitutable_paths + StoreAdmin upstream RPCs

At [`rio-proto/proto/types.proto:176`](../../rio-proto/proto/types.proto), extend `FindMissingPathsResponse`:

```proto
message FindMissingPathsResponse {
  repeated string missing_paths = 1;
  repeated string substitutable_paths = 2;  // subset of missing_paths available upstream
}
```

Field 2 is additive; existing clients ignore it (proto forward-compat). [P0463](plan-0463-upstream-substitution-surface.md) teaches the gateway's [`wopQueryMissing`](../../rio-gateway/src/handler/opcodes_read.rs) to wire these into the `willSubstitute` set (currently always empty at `:763`).

At [`rio-proto/proto/store.proto:100`](../../rio-proto/proto/store.proto), add three RPCs to `StoreAdminService` (after `ResignPaths`):

```proto
rpc ListUpstreams(rio.types.ListUpstreamsRequest) returns (rio.types.ListUpstreamsResponse);
rpc AddUpstream(rio.types.AddUpstreamRequest) returns (rio.types.UpstreamInfo);
rpc RemoveUpstream(rio.types.RemoveUpstreamRequest) returns (google.protobuf.Empty);
```

Add the message types to [`types.proto`](../../rio-proto/proto/types.proto) near the existing admin types:

```proto
message UpstreamInfo {
  int32 id = 1;
  string tenant_id = 2;
  string url = 3;
  int32 priority = 4;
  repeated string trusted_keys = 5;
  string sig_mode = 6;
}
message ListUpstreamsRequest { string tenant_id = 1; }
message ListUpstreamsResponse { repeated UpstreamInfo upstreams = 1; }
message AddUpstreamRequest {
  string tenant_id = 1;
  string url = 2;
  int32 priority = 3;
  repeated string trusted_keys = 4;
  string sig_mode = 5;
}
message RemoveUpstreamRequest { string tenant_id = 1; string url = 2; }
```

### T3 — `feat(rio-nix):` NarInfo::verify_sig — ed25519 signature check against trusted keys

At [`rio-nix/src/narinfo.rs`](../../rio-nix/src/narinfo.rs), add a verifier beside the existing [`NarInfo::parse`](../../rio-nix/src/narinfo.rs) (line 81). The fingerprint format is already documented at `:249` ("verifying a narinfo `Sig:` line reconstructs this string"):

```rust
impl NarInfo {
    /// Verify that at least one `Sig:` entry is signed by a trusted key.
    ///
    /// `trusted_keys` is a slice of `name:base64(pubkey)` strings (same
    /// format as nix `trusted-public-keys`). Returns the name of the
    /// first matching trusted key, or `None` if no sig verifies.
    pub fn verify_sig(&self, trusted_keys: &[String]) -> Option<String> {
        // 1. Build fingerprint: "1;{store_path};{nar_hash};{nar_size};{refs}"
        // 2. For each Sig entry "keyname:base64sig":
        //    - find trusted_keys entry with matching keyname
        //    - ed25519 verify(pubkey, fingerprint, sig)
        //    - return Some(keyname) on first success
        // 3. None if nothing verified
    }
}
```

Use [`ed25519_dalek::VerifyingKey`](https://docs.rs/ed25519-dalek) — same crate [`rio-store/src/signing.rs`](../../rio-store/src/signing.rs) already uses for signing. Add unit tests with a known-good narinfo + sig pair (generate via `nix-store --generate-binary-cache-key` + `nix store sign`).

## Exit criteria

- `/nixbuild .#ci` green
- `psql -c '\d tenant_upstreams'` shows the table with `sig_mode` CHECK constraint and `(tenant_id, priority)` index
- `cargo test -p rio-store --test migrations` green with `026` in `PINNED`
- `grep 'substitutable_paths' target/*/build/rio-proto-*/out/rio.types.rs` — generated Rust has the new field
- `NarInfo::verify_sig` returns `Some(keyname)` for a valid sig and `None` for a tampered narinfo
- `StoreAdminService` codegen includes `list_upstreams`/`add_upstream`/`remove_upstream` stubs

## Tracey

Adds new markers to component specs:
- `r[store.substitute.upstream]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) — T1 implements (schema), T2 implements (wire)
- `r[store.substitute.sig-mode]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) — T1 implements (CHECK constraint)

References existing markers:
- `r[store.signing.fingerprint]` — T3 reuses the same fingerprint format for verification

## Spec additions

New section in [`docs/src/components/store.md`](../../docs/src/components/store.md), inserted after `### Realisation Signing` (line 208) and before `## Two-Phase Garbage Collection`:

```markdown
## Upstream Cache Substitution

r[store.substitute.upstream]
rio-store MAY be configured with per-tenant upstream binary caches (`tenant_upstreams` table). On `QueryPathInfo`/`GetPath` miss, the store queries each upstream in priority order (`ORDER BY priority ASC`), fetches the narinfo, verifies at least one `Sig:` line against the tenant's `trusted_keys`, and if valid ingests the NAR via the same chunked-CAS path as `PutPath`. Substitution is synchronous (block-and-fetch): the originating RPC waits for ingest to complete.

r[store.substitute.sig-mode]
Per-upstream `sig_mode` controls post-substitution signature storage: `keep` stores the upstream's `Sig:` lines unchanged; `add` stores upstream sigs plus a fresh signature from the tenant's active key (or cluster key); `replace` discards upstream sigs and stores only the rio-generated signature.

r[store.substitute.tenant-sig-visibility]
A substituted path is cross-tenant visible only by signature: tenant B's `QueryPathInfo` for a path substituted by tenant A returns the path IFF at least one of the stored `narinfo.signatures` verifies against a key in tenant B's `trusted_keys` (union of B's upstream `trusted_keys` arrays). This prevents tenant A from poisoning tenant B's store by substituting from a cache B doesn't trust.
```

The third marker (`tenant-sig-visibility`) is spec'd here but implemented in [P0462](plan-0462-upstream-substitution-core.md) — marker-first discipline so `tracey query uncovered` surfaces it immediately.

## Files

```json files
[
  {"path": "migrations/026_tenant_upstreams.sql", "action": "NEW", "note": "T1: per-tenant upstream config table"},
  {"path": "rio-store/src/migrations.rs", "action": "MODIFY", "note": "T1: M_026 doc-const"},
  {"path": "rio-store/tests/migrations.rs", "action": "MODIFY", "note": "T1: pin 026 checksum"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T2: substitutable_paths + Upstream messages"},
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "T2: StoreAdmin upstream RPCs"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "T3: verify_sig method"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "spec additions: r[store.substitute.*]"}
]
```

```
migrations/
└── 026_tenant_upstreams.sql       # T1: new table
rio-store/
├── src/migrations.rs              # T1: M_026 const
└── tests/migrations.rs            # T1: checksum pin
rio-proto/proto/
├── types.proto                    # T2: response field + messages
└── store.proto                    # T2: admin RPCs
rio-nix/src/
└── narinfo.rs                     # T3: verify_sig
docs/src/components/
└── store.md                       # spec markers
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [], "note": "foundation — no deps; additive schema+proto+lib"}
```

**Depends on:** none — migration 025 is the latest; proto fields are additive; `verify_sig` is a standalone method.

**Conflicts with:** [`types.proto`](../../rio-proto/proto/types.proto) is the hottest file (37 collisions) — serialize against anything touching proto. [`narinfo.rs`](../../rio-nix/src/narinfo.rs) and [`store.proto`](../../rio-proto/proto/store.proto) are cold.
