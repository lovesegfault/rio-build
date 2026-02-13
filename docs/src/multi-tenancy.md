# Multi-Tenancy

rio-build supports multi-tenant operation where multiple teams or projects share a single cluster with data isolation and resource controls.

## Tenant Identity

Tenants are identified via SSH key mapping. The gateway's `authorized_keys` file includes tenant annotations:

```
# Format: <key-type> <public-key> <tenant-id>
ssh-ed25519 AAAA... team-infra
ssh-ed25519 BBBB... team-platform
```

Each SSH connection is associated with exactly one tenant.

### Signed Tenant Tokens

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
| `builds` | `tenant_id` | Query-level filtering |
| `derivations` | `tenant_id` | Query-level filtering |
| `narinfo` | `tenant_id` | Query-level filtering |
| `assignments` | Inherited from derivation | Implicit |

Query-level filtering ensures tenants can only see their own builds and metadata through the `AdminService` and `SchedulerService` RPCs. The gateway injects `tenant_id` from the SSH session into all requests.

## Shared Resources

### Workers

Workers are shared across tenants. Per-build overlay filesystem isolation ensures that one tenant's build cannot access another tenant's build artifacts or intermediate state. The Nix sandbox provides additional process-level isolation.

### Store Paths

Store paths can be shared across tenants. Since store paths are content-addressed (or input-addressed with deterministic hashes), identical paths built by different tenants are deduplicated at the chunk level. This is a significant storage efficiency benefit.

## Per-Tenant Features

### Signing Keys

Each tenant can have their own ed25519 signing key for narinfo signatures. This allows tenants to maintain independent trust chains for their binary caches.

### GC Policies

Garbage collection retention policies are configurable per tenant:

| Parameter | Description |
|-----------|-------------|
| `gc_grace_period` | Time between mark and sweep phases |
| `gc_retention_days` | Minimum retention for completed build outputs |
| `gc_max_store_size` | Maximum total store size for the tenant |

### Resource Quotas

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

The choice is a deployment-time configuration. Most deployments should use global scope and accept the minor information leakage risk.

### Build Activity Leakage

Beyond `FindMissingChunks`, a tenant can observe shared derivation scheduling (e.g., a shared derivation completes faster than expected, implying another tenant built it first). This is inherent to the DAG merging optimization and is documented as an accepted risk.

## Phase Mapping

| Phase | Multi-Tenancy Work |
|-------|-------------------|
| Phase 2a | `tenant_id` columns added to PostgreSQL schema (nullable, unused) |
| Phase 3 | `tenant_id` propagated via gRPC metadata; query filtering implemented |
| Phase 5 | Full enforcement: resource quotas, per-tenant signing keys, GC policies |

> **Warning: Multi-tenant deployments are unsafe before Phase 5.** Prior to Phase 5, resource quotas are not enforced, per-tenant signing keys are not available, and data isolation relies on incomplete query-level filtering. Phases 2a--4 should only be deployed as single-tenant or in environments where all tenants are trusted. Do not expose a pre-Phase-5 deployment to untrusted tenants.
