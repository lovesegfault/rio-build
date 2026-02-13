# Deployment Guide

This guide covers deploying rio-build to a Kubernetes cluster. For development, see [Contributing](./contributing.md).

## Prerequisites

- Kubernetes 1.33+ (EKS, GKE, or self-managed) --- required for user namespace isolation (`hostUsers: false`), see [ADR-012](./decisions/012-privileged-worker-pods.md)
- PostgreSQL 15+ (managed service recommended: RDS, Cloud SQL, or CloudNativePG)
- S3-compatible object storage (AWS S3, MinIO, GCS with S3 compatibility)
- `kubectl` configured for the target cluster

## Component Topology

| Component | K8s Resource | Replicas | Notes |
|-----------|-------------|----------|-------|
| rio-gateway | Deployment | 2+ | Stateless (per-connection ephemeral state only). Behind NLB. |
| rio-scheduler | Deployment | 1 (leader-elected) | Leader election via Kubernetes Lease. Standby replicas for failover. |
| rio-store | Deployment | 2+ | Stateless. Handles gRPC + binary cache HTTP. |
| rio-controller | Deployment | 1 (leader-elected) | K8s operator. Leader election via Kubernetes Lease. |
| rio-worker | StatefulSet | 2+ (autoscaled) | Managed by rio-controller via WorkerPool CRD. Requires dedicated node pool. |

## Deployment Order

1. **External dependencies** (PostgreSQL, S3 bucket, TLS certificates)
2. **rio-controller** (creates CRDs, starts watching for resources)
3. **rio-store** (needs PostgreSQL and S3)
4. **rio-scheduler** (needs PostgreSQL and rio-store)
5. **rio-gateway** (needs rio-scheduler and rio-store)
6. **WorkerPool CRD** (rio-controller creates and manages worker StatefulSets)

## Minimum Viable Deployment

For development or evaluation, a minimal deployment needs:

```
1x rio-gateway
1x rio-scheduler
1x rio-store
1x rio-controller
1x WorkerPool (2 workers)
1x PostgreSQL (single instance, e.g., via CloudNativePG)
1x MinIO (for S3-compatible storage)
```

This fits in a 4-node cluster (1 control plane + 1 general workload + 2 worker nodes with taints).

## Worker Node Pool

Workers require a dedicated node pool with:

- **Taint:** `rio.build/worker=true:NoSchedule` (only worker pods scheduled here)
- **Instance type:** Compute-optimized with local SSD (e.g., `m6id.xlarge` on AWS)
- **AMI:** Amazon Linux 2023 (AL2023) or custom AMI with kernel 6.1+. Amazon Linux 2 (AL2, kernel 5.10) does **NOT** support overlayfs-over-FUSE and is not compatible with rio-build workers.
- **Kernel:** Linux 6.1+ (for overlayfs-over-FUSE support). Verify with `uname -r` on worker nodes.
- **IMDSv2:** Hop limit = 1 (defense-in-depth against metadata access from containers)
- **Pod spec:** `hostUsers: false` (required, enables user namespace isolation for CAP_SYS_ADMIN containment)

## Key Configuration

See [Configuration Reference](./configuration.md) for all parameters. The minimum required settings:

| Component | Required Config |
|-----------|----------------|
| Gateway | `host_key_path`, `authorized_keys_path` |
| Scheduler | `database_url` |
| Store | `database_url`, `s3_bucket`, `signing_key_path` |
| Controller | `scheduler_addr` |
| Workers | `scheduler_addr`, `store_addr` |

## Secrets

See [Security: Secrets Management](./security.md#secrets-management) for recommended patterns (External Secrets Operator or Vault Agent Injector for production). At minimum, create Kubernetes Secrets for:

- SSH host key (gateway)
- Authorized SSH keys (gateway)
- NAR signing key (store)
- Database credentials (scheduler, store)
- HMAC signing key for assignment tokens (scheduler, store)

## Verification

After deployment:

```bash
# 1. Verify gateway is reachable
ssh -i ~/.ssh/rio_key -p 2222 rio-gateway.example.com

# 2. Query a known store path
nix path-info --store ssh-ng://rio-gateway.example.com:2222 /nix/store/...-hello

# 3. Build a simple package
nix build --store ssh-ng://rio-gateway.example.com:2222 nixpkgs#hello

# 4. Verify binary cache
curl -s https://rio-cache.example.com/nix-cache-info
```

## Production Considerations

- **PostgreSQL HA:** Use RDS Multi-AZ, Cloud SQL HA, or Patroni. See [Configuration: PostgreSQL Operations](./configuration.md#postgresql-operations).
- **Monitoring:** Configure Prometheus scraping and Grafana dashboards. See [Integration: Monitoring](./integration.md#monitoring-integration).
- **TLS:** Deploy a service mesh (Istio/Linkerd) for transparent mTLS between components, or configure application-level TLS. See [Configuration: TLS](./configuration.md#tls--mtls).
- **Backups:** PostgreSQL backups are critical. S3 data is durable by default. No additional backup needed for chunk storage.

## Upgrades

- **Schema migrations:** Run via `sqlx migrate` with advisory locks. All migrations are forward-compatible; rollback is supported by deploying the previous binary version (it ignores unknown columns/tables).
- **Rolling updates:** Workers drain gracefully via `terminationGracePeriodSeconds` (7200s for workers, 600s for gateways). In-flight builds complete before the pod exits. Use `maxUnavailable: 1` in the StatefulSet update strategy.
- **Blue/green deployments:** Supported if separate PostgreSQL schemas and S3 key prefixes are used per deployment. The gateway can be switched atomically via NLB target group changes.
- **Version skew policy:** Gateway and worker binaries can be at most 1 minor version behind the scheduler and store. The scheduler and store must be upgraded first.

## Disaster Recovery

- **PostgreSQL:** Standard backup/restore via `pg_dump`, WAL archiving, or managed service snapshots (e.g., RDS automated backups). PostgreSQL is the authoritative source for all metadata (narinfo, chunk manifests, scheduling state, build history). **PG metadata cannot be reconstructed from S3 alone.**
- **S3:** Durable by default (11 nines). Chunk data in S3 is the source of truth for build artifacts. Enable S3 versioning as defense against accidental deletes.
- **Recovery procedure:** Restore PostgreSQL from backup, verify S3 bucket accessibility, restart all components. The scheduler reconstructs its in-memory DAG from PostgreSQL on startup. Workers reconnect and re-register.
- **RPO:** Determined by PostgreSQL backup frequency. With WAL archiving, RPO can be near-zero. S3 data has effectively zero RPO.
- **RTO:** Determined by PostgreSQL restore time + component restart time. Typically 5-15 minutes for managed databases.

> **Multi-tenancy warning:** Multi-tenant deployments with untrusted tenants are unsafe before Phase 5. See [Multi-Tenancy](./multi-tenancy.md) for details.
