# Plan 992487505: admin-CLI for cluster_key_history + tenant_keys — write-side validation

[`cluster_key_history.rs:7-8`](../../rio-store/src/metadata/cluster_key_history.rs) says "Insert/retire are admin-CLI territory" but no CLI exists. Same gap at [`tenant_keys.rs:5`](../../rio-store/src/metadata/tenant_keys.rs) ("insert, revoke, rotate ... admin-CLI territory"). Manual `psql INSERT` is the only populate path — no validation, no audit log of WHO rotated.

**Closes [P992487504](plan-992487504-cluster-key-history-load-validation.md) from the write side.** P992487504 validates at load (defense against bad data already in the DB). This plan validates at write (rejects before the data gets in). Both land: write-side blocks typos, read-side catches out-of-band psql inserts.

[`rio-cli/src/`](../../rio-cli/src/) has precedent: `cutoffs.rs`, `gc.rs`, `logs.rs`, `status.rs`, `upstream.rs`, `wps.rs` — all `clap` subcommands hitting scheduler/store gRPC. This adds `keys.rs`.

discovered_from=521. origin=reviewer.

## Entry criteria

- [P0521](plan-0521-cluster-key-rotation-contradiction.md) merged (`cluster_key_history` table + migration 027 exist) — **DONE**

## Tasks

### T1 — `feat(proto):` AdminService key-management RPCs

Key management is a store-side operation (tables live in `rio-store`). Check if store has an `AdminService` already, or if this goes through scheduler's `AdminService` (which `rio-cli` already talks to per `sched.admin.*` markers). **Prefer store-side** — the scheduler shouldn't proxy DB writes for signing keys it doesn't own.

If `rio-store` has no admin gRPC surface yet, this is the first — scope accordingly (new `StoreAdminService` service in the proto, new server impl).

```protobuf
// rio-proto/proto/store_admin.proto (NEW if no store admin surface exists)
// or append to existing store.proto AdminService

service StoreAdmin {
  // cluster_key_history — rotation audit trail
  rpc AddClusterKeyHistory(AddClusterKeyHistoryRequest) returns (AddClusterKeyHistoryResponse);
  rpc RetireClusterKeyHistory(RetireClusterKeyHistoryRequest) returns (RetireClusterKeyHistoryResponse);
  rpc ListClusterKeyHistory(google.protobuf.Empty) returns (ListClusterKeyHistoryResponse);

  // tenant_keys — per-tenant signing key lifecycle
  rpc AddTenantKey(AddTenantKeyRequest) returns (AddTenantKeyResponse);
  rpc RevokeTenantKey(RevokeTenantKeyRequest) returns (RevokeTenantKeyResponse);
  rpc ListTenantKeys(ListTenantKeysRequest) returns (ListTenantKeysResponse);
}

message AddClusterKeyHistoryRequest {
  // name:base64(pubkey) — what Signer::trusted_key_entry emits
  string pubkey_entry = 1;
  // Optional grace period hint; if unset, no retired_at (manual retire)
  optional google.protobuf.Duration grace_period = 2;
}
// ... (retire = set retired_at, not delete — per store.md:216 post-T992487508)
```

**Check at dispatch:** `grep -r 'StoreAdmin\|store.*Admin' rio-proto/proto/` — if a store admin service already exists, extend it. If not, adding one is ~50L proto + server wiring.

### T2 — `feat(store):` server-side RPC handlers with parse validation

Handlers go in `rio-store/src/grpc/` (wherever the gRPC impls live). Each `Add*` validates the pubkey format BEFORE the INSERT:

```rust
// rio-store/src/grpc/admin.rs (or wherever store grpc handlers live)

async fn add_cluster_key_history(
    &self,
    req: Request<AddClusterKeyHistoryRequest>,
) -> Result<Response<AddClusterKeyHistoryResponse>, Status> {
    let entry = req.into_inner().pubkey_entry;

    // Same parse chain P992487504 uses at load — reject malformed
    // BEFORE it's in the DB. parse_trusted_key_entry is pub(crate).
    crate::signing::parse_trusted_key_entry(&entry)
        .map_err(|reason| Status::invalid_argument(format!(
            "malformed pubkey entry {entry:?}: {reason}. \
             Expected name:base64(32-byte-ed25519-pubkey) — \
             what `nix key generate-secret | nix key convert-secret-to-public` emits."
        )))?;

    // r[impl store.key.admin-cli]
    sqlx::query("INSERT INTO cluster_key_history (pubkey) VALUES ($1)")
        .bind(&entry)
        .execute(&self.pool).await
        .map_err(|e| Status::internal(e.to_string()))?;

    tracing::info!(entry = %entry, "cluster_key_history: prior key added");
    Ok(Response::new(AddClusterKeyHistoryResponse {}))
}
```

**Retire, not delete** ([`cluster_key_history.rs:22-23`](../../rio-store/src/metadata/cluster_key_history.rs) + [P0295](plan-0295-doc-rot-batch-sweep.md) T992487508 fixes `store.md:216` to match):

```rust
async fn retire_cluster_key_history(...) -> ... {
    // UPDATE ... SET retired_at = now() WHERE pubkey = $1 AND retired_at IS NULL
    // NOT DELETE — row retained for audit per M_027 migrations.rs:296.
}
```

**Scope cut:** `tenant_keys` write path is the same shape (`Add`/`Revoke`/`List`). If time-boxed, ship cluster_key_history RPCs first (rotation is operator-facing, tenant_keys is tenant-onboarding which has other tooling paths). Leave `TODO(P992487505)` stubs for tenant_keys handlers.

### T3 — `feat(cli):` `rio-cli keys` subcommand

NEW [`rio-cli/src/keys.rs`](../../rio-cli/src/) following the sibling pattern (`cutoffs.rs`, `upstream.rs`):

```rust
// rio-cli/src/keys.rs
use clap::Subcommand;

#[derive(Subcommand)]
pub enum KeysCmd {
    /// Add a prior cluster public key to the rotation history.
    /// Used during key rotation — the OLD key's pubkey goes here so
    /// paths signed under it stay visible during the grace period.
    ClusterAdd {
        /// name:base64(pubkey) — output of
        /// `nix key convert-secret-to-public < old-key.sec`
        pubkey_entry: String,
    },
    /// End the grace period for a prior cluster key. Sets retired_at;
    /// row retained for audit (NOT deleted).
    ClusterRetire { key_name: String },
    /// List cluster key rotation history (active + retired).
    ClusterList,

    // tenant_keys — same shape, TODO(P992487505) if scope-cut in T2
    TenantAdd { tenant_id: String, pubkey_entry: String },
    TenantRevoke { tenant_id: String, key_name: String },
    TenantList { tenant_id: String },
}
```

**Client-side validation too** — `parse_trusted_key_entry`-equivalent runs before the RPC so the operator sees the error immediately, not after a network round-trip. Server-side validates AGAIN (defense in depth; CLI isn't the only client).

Wire into [`main.rs`](../../rio-cli/src/main.rs) alongside the existing subcommands.

### T4 — `test(cli):` integration test — add, list, retire, list again

```rust
// r[verify store.key.admin-cli]
#[tokio::test]
async fn cluster_key_lifecycle_via_cli() {
    // TestDb + store grpc server.
    // 1. ClusterList → empty
    // 2. ClusterAdd "cache-v1:VALID_32B_B64" → ok
    // 3. ClusterList → 1 entry, retired_at=None
    // 4. ClusterAdd "bad:!!!" → InvalidArgument (server-side parse rejects)
    // 5. ClusterList → still 1 entry (bad not inserted)
    // 6. ClusterRetire "cache-v1" → ok
    // 7. ClusterList → 1 entry, retired_at=Some(..)
    // 8. load_cluster_key_history (P0521's query) → empty (retired filtered)
}
```

## Exit criteria

- `/nbr .#ci` green
- `rio-cli keys cluster-add 'test:!!!'` (against a test store) → exits nonzero with "malformed pubkey entry ... not valid base64"
- `rio-cli keys cluster-add 'test:VALID_B64'` → exits zero; `SELECT pubkey FROM cluster_key_history` → contains the entry
- `rio-cli keys cluster-retire test` → `SELECT retired_at FROM cluster_key_history WHERE pubkey LIKE 'test:%'` → NOT NULL; row NOT deleted
- `grep 'parse_trusted_key_entry\|malformed pubkey' rio-store/src/grpc/` -r → ≥1 hit (server-side validation uses the P992487504 helper)
- `cargo nextest run cluster_key_lifecycle_via_cli` → passes
- `grep 'admin-CLI territory' rio-store/src/metadata/cluster_key_history.rs rio-store/src/metadata/tenant_keys.rs` → either 0 hits (comments updated to point at the CLI) or the comments now say "see rio-cli keys"

## Tracey

References existing markers:
- `r[store.key.rotation-cluster-history]` ([`store.md:218`](../../docs/src/components/store.md)) — T2's `Retire` handler implements "operator ends the grace period by setting retired_at". The Add handler is the write-side of "loaded from cluster_key_history".

Adds new marker to component specs:
- `r[store.key.admin-cli]` → [`docs/src/components/store.md`](../../docs/src/components/store.md) after `:219` (see ## Spec additions below)

## Spec additions

**New `r[store.key.admin-cli]`** — goes to [`docs/src/components/store.md`](../../docs/src/components/store.md) after `:219` (blank line before, col 0):

```
r[store.key.admin-cli]
Cluster key rotation history and per-tenant signing keys MUST be manageable via `rio-cli keys`. The CLI validates pubkey entry format (`name:base64(32-byte-ed25519-pubkey)`) before INSERT — malformed entries are rejected with a specific reason (missing separator, bad base64, wrong length, invalid curve point). Retirement sets `retired_at`, preserving the audit trail; deletion is not exposed. Manual `psql` remains possible but bypasses validation — load-time checks (`r[store.key.rotation-cluster-history]`) catch malformed rows regardless.
```

## Files

```json files
[
  {"path": "rio-proto/proto/store_admin.proto", "action": "NEW", "note": "T1: StoreAdmin service + AddClusterKeyHistory/Retire/List RPCs + tenant_keys RPCs. May be store.proto MODIFY if store admin surface already exists — check at dispatch"},
  {"path": "rio-store/src/grpc/admin.rs", "action": "MODIFY", "note": "T2: RPC handlers with parse_trusted_key_entry validation. May be NEW if no store admin grpc exists. Reuses P992487504's parse helper"},
  {"path": "rio-store/src/metadata/cluster_key_history.rs", "action": "MODIFY", "note": "T2: NEW insert + retire fns (query surface for the handlers). :7-8 comment update → points at rio-cli keys. P992487504-T3 adds a test here — both additive"},
  {"path": "rio-store/src/metadata/tenant_keys.rs", "action": "MODIFY", "note": "T2: NEW insert + revoke fns. :5 comment update. Scope-cut candidate — ship cluster first, TODO stubs for tenant"},
  {"path": "rio-cli/src/keys.rs", "action": "NEW", "note": "T3: KeysCmd enum + cluster-add/retire/list + tenant-add/revoke/list subcommands"},
  {"path": "rio-cli/src/main.rs", "action": "MODIFY", "note": "T3: wire KeysCmd into clap parser alongside cutoffs/gc/logs/status/upstream/wps"},
  {"path": "rio-cli/tests/keys_integration.rs", "action": "NEW", "note": "T4: cluster_key_lifecycle_via_cli — TestDb + store grpc + CLI round-trip"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "Spec additions: r[store.key.admin-cli] after :219. P0295-T992487508 edits :216 (delete→retire wording) — adjacent lines, coordinate"}
]
```

```
rio-proto/proto/
└── store_admin.proto          # T1: NEW (or store.proto MODIFY)
rio-store/src/
├── grpc/admin.rs              # T2: handlers
└── metadata/
    ├── cluster_key_history.rs # T2: insert/retire fns
    └── tenant_keys.rs         # T2: insert/revoke fns (scope-cut?)
rio-cli/src/
├── keys.rs                    # T3: NEW subcommand
└── main.rs                    # T3: wire
rio-cli/tests/keys_integration.rs  # T4: NEW
docs/src/components/store.md   # spec marker
```

## Dependencies

```json deps
{"deps": [521], "soft_deps": [992487504], "note": "P0521 (DONE) created the cluster_key_history table. P992487504 (soft) extracts parse_trusted_key_entry — T2's validation reuses it. If P992487504 hasn't landed, duplicate the parse chain inline (same 4-step split_once/decode/try_into/from_bytes) with a TODO to swap for the helper."}
```

**Depends on:** [P0521](plan-0521-cluster-key-rotation-contradiction.md) (DONE) — created `cluster_key_history` table (migration 027).

**Soft-dep:** [P992487504](plan-992487504-cluster-key-history-load-validation.md) — extracts `parse_trusted_key_entry` helper that T2's server-side validation reuses. If P992487504 hasn't landed at dispatch, inline the 4-step parse (`split_once`/`b64.decode`/`[u8;32].try_into`/`VerifyingKey::from_bytes`) with a `TODO(P992487504): swap for parse_trusted_key_entry`.

**Conflicts with:** `store.md:216-219` — [P0295](plan-0295-doc-rot-batch-sweep.md) T992487508 edits `:216` (delete→retire wording). This plan adds a marker after `:219`. ADJACENT lines — coordinate at dispatch; prefer T992487508 lands first (1-word fix), then this plan's marker insert is a clean append. `cluster_key_history.rs` shared with P992487504-T3 (adds a test) — both additive. `rio-cli/src/main.rs` is low-traffic (subcommand wiring).
