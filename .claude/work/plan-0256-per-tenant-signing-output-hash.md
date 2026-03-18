# Plan 0256: Per-tenant signing keys + output_hash fix (SAFETY GATE 2/3)

**Absorbs [`opcodes_read.rs:493`](../../rio-gateway/src/handler/opcodes_read.rs) `TODO(phase5)` per GT5.** The `:493` TODO stores `output_hash` as zeros in the realisations table. Per GT5 analysis: cutoff does NOT read this field (it compares `nar_hash` via content_index). The TODO is a **signing concern** — signing a realisation needs the real hash. It's absorbed here, NOT in the CA spine.

Per-tenant signing: `signing.rs` currently has a single cluster `SigningKey`. This adds a `tenant_keys` lookup (migration 014 via [P0249](plan-0249-migration-batch-014-015-016.md)) with cluster-key fallback. A tenant with its own key produces narinfo that `nix store verify --trusted-public-keys tenant:<pk>` accepts for that tenant's paths only.

**SAFETY GATE 2 of 3** for `introduction.md:50` warning removal.

## Entry criteria

- [P0249](plan-0249-migration-batch-014-015-016.md) merged (`tenant_keys` table exists)
- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged (`r[store.tenant.sign-key]` seeded)

## Tasks

### T1 — `feat(store):` tenant-aware signing

MODIFY [`rio-store/src/signing.rs`](../../rio-store/src/signing.rs):

```rust
// r[impl store.tenant.sign-key]
/// Sign narinfo with tenant's active key if present, else cluster key.
pub async fn sign_narinfo(
    &self,
    tenant_id: Option<Uuid>,
    narinfo: &NarInfo,
) -> Result<Signature> {
    let key = match tenant_id {
        Some(tid) => tenant_keys::get_active_signing_key(&self.pool, tid)
            .await?
            .unwrap_or_else(|| self.cluster_key.clone()),
        None => self.cluster_key.clone(),
    };
    key.sign(narinfo.fingerprint())
}
```

### T2 — `feat(store):` tenant_keys query module

NEW [`rio-store/src/metadata/tenant_keys.rs`](../../rio-store/src/metadata/tenant_keys.rs) — **avoids `db.rs` (HOTTEST file)**:

```rust
pub async fn get_active_signing_key(
    pool: &PgPool,
    tenant_id: Uuid,
) -> sqlx::Result<Option<SigningKey>> {
    let row = sqlx::query!(
        "SELECT ed25519_seed FROM tenant_keys
         WHERE tenant_id = $1 AND revoked_at IS NULL
         ORDER BY created_at DESC LIMIT 1",
        tenant_id
    ).fetch_optional(pool).await?;
    Ok(row.map(|r| SigningKey::from_seed(&r.ed25519_seed)))
}
```

### T3 — `fix(gateway):` opcodes_read.rs:493 — real output_hash

MODIFY [`rio-gateway/src/handler/opcodes_read.rs`](../../rio-gateway/src/handler/opcodes_read.rs) at `:493` — fetch real `output_hash` via `QueryPathInfo` before `RegisterRealisation`:

```rust
// Before (TODO): output_hash stored as zeros
// After: fetch via QueryPathInfo — needed for SIGNING realisations,
//        NOT on the CA-cutoff critical path (cutoff uses content_index).
let path_info = store_client.query_path_info(&output_path).await?;
let output_hash = path_info.nar_hash;  // real hash, not zeros
```

### T4 — `docs:` close multi-tenancy.md:64 deferral

MODIFY [`docs/src/multi-tenancy.md`](../../docs/src/multi-tenancy.md) — remove the `:64` deferral block.

### T5 — `test(store):` tenant key vs cluster key

```rust
// r[verify store.tenant.sign-key]
#[tokio::test]
async fn tenant_with_key_signs_with_tenant_key() {
    // Seed tenant_keys row. Sign narinfo. Assert signature verifies
    // with tenant pubkey, NOT cluster pubkey.
}
#[tokio::test]
async fn tenant_without_key_falls_back_to_cluster() { ... }
```

Plus: `nix store verify --trusted-public-keys tenant:<pk>` passes only for that tenant's paths (integration fragment in security.nix if time permits).

## Exit criteria

- `/nbr .#ci` green
- `rg 'TODO\(phase5\)' rio-gateway/src/handler/opcodes_read.rs` → 0 (the `:493` TODO is closed)
- `nix develop -c tracey query rule store.tenant.sign-key` shows impl + verify

## Tracey

References existing markers:
- `r[store.tenant.sign-key]` — T1 implements, T5 verifies (seeded by P0245)

## Files

```json files
[
  {"path": "rio-store/src/signing.rs", "action": "MODIFY", "note": "T1: tenant-aware sign_narinfo()"},
  {"path": "rio-store/src/metadata/tenant_keys.rs", "action": "NEW", "note": "T2: get_active_signing_key() — avoids db.rs"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "T2: mod decl"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "T3: :493 fetch real output_hash (absorbs TODO)"},
  {"path": "docs/src/multi-tenancy.md", "action": "MODIFY", "note": "T4: close :64 deferral"}
]
```

```
rio-store/src/
├── signing.rs                    # T1: tenant-aware signing
└── metadata/
    ├── mod.rs                    # T2: mod decl
    └── tenant_keys.rs            # T2: NEW (avoids db.rs HOTTEST)
rio-gateway/src/handler/
└── opcodes_read.rs               # T3: :493 real output_hash
docs/src/
└── multi-tenancy.md              # T4: close deferral
```

## Dependencies

```json deps
{"deps": [249, 245], "soft_deps": [], "note": "SAFETY GATE 2/3. signing.rs no 4b/4c touch — low collision. AVOIDS db.rs via new metadata/tenant_keys.rs. Absorbs opcodes_read.rs:493 TODO (GT5: signing, not cutoff)."}
```

**Depends on:** [P0249](plan-0249-migration-batch-014-015-016.md) — `tenant_keys` table. [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) — marker seeded.
**Conflicts with:** `signing.rs` no 4b/4c touch. `opcodes_read.rs` low — disjoint from [P0253](plan-0253-ca-resolution-dependentrealisations.md)'s 3 sites (`:418/:577/:586`). Parallel with P0255/P0272.
