# Multi-Tenancy

rio-build supports multi-tenant operation where multiple teams or projects share a single cluster with data isolation and resource controls.

## Tenant Identity

Tenants are identified via SSH key mapping. The gateway's `authorized_keys` file includes tenant annotations:

```
# Format: <key-type> <public-key> <tenant-name>
ssh-ed25519 AAAA... team-infra
ssh-ed25519 BBBB... team-platform
```

Each SSH connection is associated with exactly one tenant. See [`gw.auth.tenant-from-key-comment`](./components/gateway.md) for how the gateway extracts the tenant name from the `authorized_keys` entry comment, and [`sched.tenant.resolve`](./components/scheduler.md) for how the scheduler resolves the name to a UUID.

### Signed Tenant Tokens

> **Landed:** JWT token flow is complete — lib + issuance + verify middleware + jti revocation + K8s ConfigMap mount + SIGHUP hot-swap. See `r[gw.jwt.issue]`, `r[gw.jwt.verify]`, `r[gw.jwt.dual-mode]`, `r[sec.jwt.pubkey-mount]`.

Tenant identity is cryptographically bound using signed JWT tokens rather than plain gRPC metadata:

1. **Token issuance**: When a client authenticates via SSH, the gateway issues a short-lived JWT containing:
   - `sub`: the `tenant_id`
   - `iat`: issuance timestamp
   - `exp`: expiry (default: duration of the SSH session + grace period)
   - `jti`: unique token ID for revocation tracking
2. **Signing**: Tokens are signed with an ed25519 key held by the gateway. The signing key is stored as a Kubernetes Secret (recommend KMS/Vault for production). Key rotation follows the same procedure as narinfo signing keys.
3. **Propagation**: The signed JWT propagates through all internal gRPC calls in the `x-rio-tenant-token` metadata header, replacing the unsigned `tenant_id` field.
4. **Verification**: All downstream services (scheduler, store, controller) verify the JWT signature and expiry before processing any request. An invalid or expired token results in `UNAUTHENTICATED` gRPC status.
5. **Key distribution**: The gateway's public verification key is distributed to all services via a Kubernetes ConfigMap mounted at startup. Services reload the key on SIGHUP for zero-downtime rotation.

## Data Isolation

All persistent data carries a `tenant_id` foreign key:

| Table | Tenant Column | Isolation Level |
|-------|--------------|-----------------|
| `builds` | `tenant_id` (nullable FK → `tenants`) | Query-level filtering |
| `derivations` | `tenant_id` (nullable FK → `tenants`) | Query-level filtering |
| `path_tenants` | `tenant_id` (PK component) | N:M junction — path is visible to a tenant iff junction row exists |
| `chunk_tenants` | `tenant_id` (PK component) | N:M junction — `FindMissingChunks` scope |
| `content_index` | `tenant_id` (nullable, unused) | — (predates junction-table design; migration 002) |
| `realisations` | `tenant_id` (nullable) | Query-level filtering |
| `assignments` | *(none)* | Implicit via `derivation_id` FK --- no direct `tenant_id` column; tenant scope is derived by joining to `derivations` |

Query-level filtering ensures tenants can only see their own builds and metadata through the `AdminService` and `SchedulerService` RPCs. The gateway injects `tenant_id` (as a signed JWT) into all requests.

## Shared Resources

### Workers

Workers are shared across tenants. Per-build overlay filesystem isolation ensures that one tenant's build cannot access another tenant's build artifacts or intermediate state. The Nix sandbox provides additional process-level isolation.

### Store Paths

Store paths can be shared across tenants. Since store paths are content-addressed (or input-addressed with deterministic hashes), identical paths built by different tenants are deduplicated at the chunk level. This is a significant storage efficiency benefit.

## Per-Tenant Features

### Signing Keys

Each tenant can have their own ed25519 signing key for narinfo signatures. This allows tenants to maintain independent trust chains for their binary caches. The cluster-wide key remains the fallback for tenants without a key.

### GC Policies

> **Implemented:** per-tenant GC retention via the `path_tenants` junction + 6th UNION arm in the mark CTE (`r[store.gc.tenant-retention]`).

Garbage collection retention policies are configurable per tenant:

| Parameter | Description |
|-----------|-------------|
| `gc_grace_period` | Time between mark and sweep phases |
| `gc_retention_days` | Minimum retention for completed build outputs |
| `gc_max_store_size` | Maximum total store size for the tenant |

### Resource Quotas

Per-tenant store quota (`tenants.gc_max_store_bytes`) is enforced at the gateway before `SubmitBuild` — see `r[store.gc.tenant-quota-enforce]`. Over-quota tenants receive `STDERR_ERROR` with current usage / limit; the SSH connection stays open so the user can GC and retry. Enforcement is eventually-consistent (30s TTL cache on `tenant_store_bytes`); a few MB of race-window overflow is acceptable.

`NULL` `gc_max_store_bytes` (the default) disables the gate for that tenant. Single-tenant mode (empty `authorized_keys` comment) never quota-checks.

| Parameter | Description |
|-----------|-------------|
| `max_concurrent_builds` | Maximum builds running simultaneously |
| `max_dag_size` | Maximum derivations in a single build DAG |
| `max_store_size` | Maximum total store usage (enforced pre-SubmitBuild via `gc_max_store_bytes`) |
| `max_nar_upload_size` | Maximum single NAR upload size |

## Security Considerations

### `FindMissingChunks` Scoping

The `FindMissingChunks` RPC can reveal whether another tenant has built a specific package (by probing for chunk existence). Two scoping options:

1. **Global scope** (default): All tenants share the chunk namespace. Maximum dedup savings, but tenants can infer each other's build activity.
2. **Per-tenant scope**: Each tenant has a separate chunk namespace. No cross-tenant information leakage, but reduced dedup (identical chunks stored per-tenant).

> **Current state:** per-tenant scoping is implemented via the
> `chunk_tenants` junction table (migration 018). `FindMissingChunks`
> and `PutChunk` both require a JWT with `Claims.sub` (fail-closed on
> missing — `UNAUTHENTICATED`). A chunk is reported present to a
> tenant IFF that tenant has a junction row; different tenants can
> share the same `chunks` row (dedup preserved) while each sees only
> their own uploads. The global chunk namespace (option 1 above) is
> no longer available.

### Build Activity Leakage

Beyond `FindMissingChunks`, a tenant can observe shared derivation scheduling (e.g., a shared derivation completes faster than expected, implying another tenant built it first). This is inherent to the DAG merging optimization and is documented as an accepted risk.

## Implementation Status

| Feature | Plan | Status |
|---------|------|--------|
| `tenant_id` columns + FK | migration 009 | done |
| `path_tenants` junction table | [P0206](../../.claude/work/plan-0206-path-tenants-migration-upsert.md) | done |
| Per-tenant GC retention | [P0207](../../.claude/work/plan-0207-mark-cte-tenant-retention.md) | done |
| Quota reject at SubmitBuild | [P0255](../../.claude/work/plan-0255-quota-reject-submitbuild.md) | done |
| Per-tenant signing keys | [P0256](../../.claude/work/plan-0256-per-tenant-signing-keys.md) | done |
| JWT issuance + verify middleware | [P0257](../../.claude/work/plan-0257-jwt-lib-claims-sign-verify.md)–[P0260](../../.claude/work/plan-0260-jwt-dual-mode-k8s-sighup.md) | done |
| Per-tenant narinfo filter | [P0272](../../.claude/work/plan-0272-per-tenant-narinfo-filter.md) | done |
| `chunk_tenants` per-tenant scope | migration 018 + [P0264](../../.claude/work/plan-0264-findmissingchunks-tenant-scope.md) | done |

> **Warning:** Multi-tenant safety depends on the JWT verify interceptor being LIVE (pubkey mounted and non-`None` in scheduler/store `main.rs`). With the interceptor inert, downstream services do not validate tenant identity and query-level filtering is bypassable. Do not expose a deployment with inert JWT verification to untrusted tenants.
