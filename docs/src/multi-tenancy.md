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

> **Scheduled:** JWT token flow → [P0257](../.claude/work/plan-0257-jwt-lib-claims-sign-verify.md) (lib) + [P0258](../.claude/work/plan-0258-jwt-issuance-gateway.md) (issuance) + [P0259](../.claude/work/plan-0259-jwt-verify-middleware.md) (verify) + [P0260](../.claude/work/plan-0260-jwt-dual-mode-k8s-sighup.md) (dual-mode). Until the spine lands: `tenant_id` is an empty string in all gRPC metadata.

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
| `builds` | `tenant_id` (nullable) | Query-level filtering |
| `derivations` | `tenant_id` (nullable) | Query-level filtering |
| `narinfo` | `tenant_id` (nullable) | Query-level filtering |
| `content_index` | `tenant_id` (nullable) | Query-level filtering |
| `realisations` | `tenant_id` (nullable) | Query-level filtering |
| `assignments` | *(none)* | Implicit via `derivation_id` FK --- no direct `tenant_id` column; tenant scope is derived by joining to `derivations` |

Query-level filtering ensures tenants can only see their own builds and metadata through the `AdminService` and `SchedulerService` RPCs. The gateway injects `tenant_id` from the SSH session into all requests.

> **Current state (Phase 3a):** all `tenant_id` columns are nullable and never written (always NULL). Query-level filtering is not implemented; `AdminService` returns all rows regardless of tenant.

## Shared Resources

### Workers

Workers are shared across tenants. Per-build overlay filesystem isolation ensures that one tenant's build cannot access another tenant's build artifacts or intermediate state. The Nix sandbox provides additional process-level isolation.

### Store Paths

Store paths can be shared across tenants. Since store paths are content-addressed (or input-addressed with deterministic hashes), identical paths built by different tenants are deduplicated at the chunk level. This is a significant storage efficiency benefit.

## Per-Tenant Features

### Signing Keys

> **Scheduled:** per-tenant signing keys → [P0256](../.claude/work/plan-0256-per-tenant-signing-output-hash.md). Until it lands: a single cluster-wide ed25519 key signs all narinfo.

Each tenant can have their own ed25519 signing key for narinfo signatures. This allows tenants to maintain independent trust chains for their binary caches.

### GC Policies

> **Scheduled:** per-tenant GC policy → [P0206](../.claude/work/plan-0206-path-tenants-migration-upsert.md) + [P0207](../.claude/work/plan-0207-mark-cte-tenant-retention.md). GC core (mark/sweep/drain) is implemented.

Garbage collection retention policies are configurable per tenant:

| Parameter | Description |
|-----------|-------------|
| `gc_grace_period` | Time between mark and sweep phases |
| `gc_retention_days` | Minimum retention for completed build outputs |
| `gc_max_store_size` | Maximum total store size for the tenant |

### Resource Quotas

> **Scheduled:** per-tenant quotas → [P0255](../.claude/work/plan-0255-quota-reject-submitbuild.md). Until it lands: limits are global compile-time constants; no per-tenant accounting.

| Parameter | Description |
|-----------|-------------|
| `max_concurrent_builds` | Maximum builds running simultaneously |
| `max_dag_size` | Maximum derivations in a single build DAG |
| `max_store_size` | Maximum total store usage |
| `max_nar_upload_size` | Maximum single NAR upload size |

## Security Considerations

### `FindMissingChunks` Scoping

The `FindMissingChunks` RPC can reveal whether another tenant has built a specific package (by probing for chunk existence). Two scoping options:

1. **Global scope** (default): All tenants share the chunk namespace. Maximum dedup savings, but tenants can infer each other's build activity.
2. **Per-tenant scope**: Each tenant has a separate chunk namespace. No cross-tenant information leakage, but reduced dedup (identical chunks stored per-tenant).

> **Current state:** scoping is not configurable. `FindMissingChunks` always uses global scope. Per-tenant scoping requires the chunk table to carry `tenant_id` (it doesn't) and is deferred to Phase 5 if a use case emerges.

### Build Activity Leakage

Beyond `FindMissingChunks`, a tenant can observe shared derivation scheduling (e.g., a shared derivation completes faster than expected, implying another tenant built it first). This is inherent to the DAG merging optimization and is documented as an accepted risk.

## Phase Mapping

| Phase | Multi-Tenancy Work |
|-------|-------------------|
| Phase 2a | `tenant_id` columns added to PostgreSQL schema (nullable, unused) --- **done** |
| Phase 4 | `tenant_id` propagated via gRPC metadata; query filtering implemented in `AdminService` |
| Phase 5 | Full enforcement: JWT issuance/verification, resource quotas, per-tenant signing keys, GC policies |

> **Correction:** earlier docs claimed Phase 3 would wire `tenant_id` propagation. Phase 3a landed with `tenant_id` still the empty string everywhere; no propagation or filtering exists. This work has been re-scoped to Phase 4.

> **Warning: Multi-tenant deployments are unsafe before Phase 5.** Prior to Phase 5, resource quotas are not enforced, per-tenant signing keys are not available, and data isolation relies on incomplete query-level filtering. Phases 2a--4 should only be deployed as single-tenant or in environments where all tenants are trusted. Do not expose a pre-Phase-5 deployment to untrusted tenants.
