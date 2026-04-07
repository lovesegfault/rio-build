# Deployment Guide

This guide covers deploying rio-build to a Kubernetes cluster. For development, see [Contributing](./contributing.md).

## Prerequisites

- Kubernetes 1.33+ (EKS, GKE, or self-managed) --- required for user namespace isolation (`hostUsers: false`), see [ADR-012](./decisions/012-privileged-builder-pods.md)
- PostgreSQL 15+ (managed service recommended: RDS, Cloud SQL, or CloudNativePG). Aurora/RDS PG 15+ have `rds.force_ssl=1` by default --- the connection string must include `?sslmode=require` (sqlx has `tls-rustls-aws-lc-rs` enabled for this)
- S3-compatible object storage (AWS S3, MinIO, GCS with S3 compatibility)
- `kubectl` configured for the target cluster

## Component Topology

| Component | K8s Resource | Replicas | Notes |
|-----------|-------------|----------|-------|
| rio-gateway | Deployment | 2+ | Stateless (per-connection ephemeral state only). Behind NLB. |
| rio-scheduler | Deployment | 2 (leader-elected) | Leader election via Kubernetes Lease. One leader, one hot standby. ~15s failover. |
| rio-store | Deployment | 1 | Stateless at runtime (PG + S3 hold everything), but concurrent startup migrations from multiple replicas would race. Scale horizontally later after adding a migration-lock mechanism. |
| rio-controller | Deployment | 1 | K8s operator. **Single replica, not leader-elected** --- two controllers would fight over SSA patches (conflicting fieldManager). Add leader election later if the ~30s pod-reschedule gap during restart becomes a problem. |
| rio-worker | StatefulSet | 2+ (autoscaled) | Managed by rio-controller via WorkerPool CRD. Requires dedicated node pool. |

## Deployment Order

1. **External dependencies** (PostgreSQL, S3 bucket, TLS certificates)
2. **rio-controller** (creates CRDs, starts watching for resources)
3. **rio-store** (needs PostgreSQL and S3)
4. **rio-scheduler** (needs PostgreSQL and rio-store)
5. **rio-gateway** (needs rio-scheduler and rio-store)
6. **WorkerPool CRD** (rio-controller creates and manages worker StatefulSets)

`helm upgrade --wait` blocks until all Deployments report Available — strict ordering isn't enforced, but no component is externally reachable until the release as a whole is Ready. Readiness probes on each component ensure this: store readiness requires PG migrations done, scheduler readiness requires store reachable, gateway readiness requires scheduler reachable.

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

r[infra.node.nixos-ami]

The EKS reference deployment builds its own NixOS worker-node AMI (`nix build .#node-ami-<arch>`) and selects it via a tag-matched `EC2NodeClass` with `amiFamily: AL2023` so Karpenter emits NodeConfig userData for the packaged `nodeadm` to consume. The AMI bakes in the `user.max_user_namespaces` sysctl, the Localhost seccomp profiles, `cgroup_writable=true` for containerd, the `EROFS_FS_ONDEMAND`/`CACHEFILES_ONDEMAND` kernel options, and `/dev/{fuse,kvm}` injection via containerd `base_runtime_spec` — the chart renders no `userData` at all. See [ADR-021](decisions/021-nixos-node-ami.md). The pipeline is `cargo xtask k8s -p eks up --ami` (writes `.rio-ami-tag`) → `cargo xtask k8s -p eks up --deploy` (sets `karpenter.amiTag` from that file). The tag is content-addressed (12 hex of `sha256(∑ drvPaths)`), so a no-op `up` skips the coldsnap upload entirely; `up --deploy` recomputes the same tag if `.rio-ami-tag` is absent.

r[infra.node.prebake-layer-warm]

The AMI also bakes a `rio-executor-seed.oci.tar` (builder+fetcher images, deduplicated layers, ~124 MB) and imports it into containerd's content store via a `containerd-seed-warm` oneshot that runs concurrently with kubelet registration (the import completes inside kubelet's ~5–15 s TLS-bootstrap window; if it loses the race, the first pod pull is cold-from-ECR — degraded, not broken). PodSpec image refs stay `<ECR>/rio-{builder,fetcher}:<git-sha>` — the seed is a layer-cache warm, not the pulled ref. On a fresh node's first pod, containerd resolves the ECR manifest and finds most layer blobs already local by digest; only layers that changed since the AMI was cut are fetched (typically the ~10 MB `rio-workspace` top layer, or zero if AMI and deploy are at the same commit). The seed's `seed.local/…:prebaked` image-store refs are pinned (`io.cri-containerd.pinned=pinned`) so kubelet image-GC can't reclaim the layer blobs before the first pod runs. Dev loop is unchanged (`up --push --deploy`); `up --ami` is an optional optimization to keep the delta near zero.

- **Taint:** `rio.build/worker=true:NoSchedule` (only worker pods scheduled here). Note: system pods (coredns) need at least one untainted node — use a separate system node group.
- **Instance type:** Compute-optimized (e.g., `c8a.xlarge` on AWS). Avoid instance types with instance store NVMe (`m6id`, `i3`) unless the EKS AMI supports them — AL2023 EKS AMIs may report `InvalidDiskCapacity` with instance store volumes.
- **AMI:** the NixOS node AMI (`.#node-ami-<arch>`, [ADR-021](decisions/021-nixos-node-ami.md)) or AL2023 with kernel 6.1+. Amazon Linux 2 (AL2, kernel 5.10) does **NOT** support overlayfs-over-FUSE and is not compatible with rio-build workers.
- **Kernel:** Linux 6.1+ (for overlayfs-over-FUSE support). Linux 6.9+ recommended for FUSE passthrough mode. Verify with `uname -r` on worker nodes.
- **IMDSv2:** Hop limit = 1 (defense-in-depth against metadata access from containers)
- **Pod spec:** `hostUsers: false` is incompatible with `/dev/fuse` hostPath volumes (kernel rejects idmap mounts on device nodes). containerd `base_runtime_spec` injects `/dev/{fuse,kvm}` directly (OCI `linux.devices` — runc `mknod`s inside the container's `/dev`); see `nix/base-runtime-spec.nix` (NixOS AMI: `nix/nixos-node/containerd-config.nix`; k3s VM fixture: `services.k3s.containerdConfigTemplate` in `nix/tests/fixtures/k3s-full.nix`).
- **`/dev/fuse` access:** Worker pods need access to `/dev/fuse`. A `hostPath` volume with `privileged: true` works for development but production should use `base_runtime_spec` device injection to avoid granting full privileges. `CAP_SYS_ADMIN` alone is not sufficient for `/dev/fuse` access — the container's device cgroup must also allow the FUSE character device.
- **EKS addons:** `vpc-cni` and `kube-proxy` must be installed before node groups are created (they are daemonsets). `coredns` requires schedulable (untainted) nodes and should be installed after the system node group is ready.

### Node autoscaling

Worker pod autoscaling (rio-controller) and node autoscaling (cluster autoscaler or Karpenter) are separate concerns that chain together. rio-controller scales the WorkerPool StatefulSet replica count based on scheduler queue depth; the node autoscaler provisions capacity for the resulting Pending pods. Without a node autoscaler, rio-controller scaling beyond the static node pool's capacity just produces permanently-Pending pods.

The EKS reference deployment (`infra/eks/`) uses Karpenter: the `workers` managed nodegroup is replaced entirely with three Karpenter NodePools (compute-optimized preferred, general-purpose fallback, untainted general). `consolidationPolicy: WhenEmpty` on worker NodePools means Karpenter never evicts a node with a worker pod on it --- the 10-minute `SCALE_DOWN_WINDOW` in rio-controller scales pods away first, then Karpenter consolidates the empty node. Scale-to-zero is the default (`WorkerPool.spec.replicas.min: 0`): cold start from zero is ~50-80s (node boot + pod start).

## Key Configuration

See [Configuration Reference](./configuration.md) for all parameters. The minimum required settings:

| Component | Required Config |
|-----------|----------------|
| Gateway | `host_key`, `authorized_keys`, `scheduler_addr`, `store_addr` |
| Scheduler | `database_url` |
| Store | `database_url`, `chunk_backend` (tagged enum: `inline` / `filesystem` / `s3`), `signing_key_path` |
| Controller | `scheduler_addr` |
| Workers | `scheduler_addr`, `store_addr` |

> **Store chunk backend config** uses a serde internally-tagged enum (`kind`). TOML example for S3: `[chunk_backend]` / `kind = "s3"` / `bucket = "..."` / `prefix = "..."`. Default is `inline` (NARs stored in PostgreSQL --- fine for dev, does not scale). There is no flat `s3_bucket` field.

## Secrets

See [Security: Secrets Management](./security.md#secrets-management) for recommended patterns (External Secrets Operator or Vault Agent Injector for production). At minimum, create Kubernetes Secrets for:

- SSH host key (gateway)
- Authorized SSH keys (gateway)
- NAR signing key (store)
- Database credentials (scheduler, store)
- HMAC signing key for assignment tokens (scheduler, store) --- set via `RIO_HMAC_KEY_PATH` on both. The scheduler signs Claims{worker_id, drv_hash, expected_outputs, expiry} at dispatch; the store verifies on `PutPath`. Same key file both sides (shared secret). Generate: `openssl rand -out /path/to/key 32`.

> **SSH key mounting:** The default chart values do **not** set `gateway.ssh.hostKeySecret` — the gateway generates an ephemeral host key on startup (fine for dev; breaks `known_hosts` on every restart). Production should set it to a Secret with a persistent key so all replicas present the same host key. `gateway.ssh.authorizedKeysSecret` defaults to `rio-gateway-ssh` — create that Secret before deploy or the gateway pod blocks on the missing mount.

## Verification

After deployment:

```bash
# 1. Tenant bootstrap + cluster status. `xtask k8s cli` port-forwards
#    scheduler:9001 + store:9002, fetches the mTLS client cert from
#    the rio-scheduler-tls Secret, and runs rio-cli LOCALLY. No need
#    for rio-cli (or jq, column, …) inside the scheduler image.
cargo xtask k8s cli -p k3s -- create-tenant my-team
cargo xtask k8s cli -p k3s -- status

# 2. SSH key with tenant-name comment. The gateway maps the
#    authorized_keys comment field to tenant_name (server-side
#    comment, not the client's key comment).
ssh-keygen -t ed25519 -C my-team -f ~/.ssh/rio_key -N ''
kubectl -n rio-system create secret generic rio-gateway-ssh \
  --from-file=authorized_keys=~/.ssh/rio_key.pub \
  --dry-run=client -o yaml | kubectl apply -f -
kubectl -n rio-system rollout restart deployment/rio-gateway

# 3. Build a simple package (Service maps external port 22 → container 2222)
nix build --store "ssh-ng://rio@rio-gateway.example.com?ssh-key=$HOME/.ssh/rio_key" nixpkgs#hello

# 4. Verify binary cache
curl -s https://rio-cache.example.com/nix-cache-info
```

For a complete scripted walkthrough against EKS, run `cargo xtask k8s smoke -p eks`.

<details>
<summary>Without xtask (kubectl exec fallback)</summary>

If rio-cli is bundled in the scheduler image (legacy — discouraged per
`r[sec.image.control-plane-minimal]`):

```bash
kubectl -n rio-system exec deploy/rio-scheduler -- rio-cli create-tenant my-team
kubectl -n rio-system exec deploy/rio-scheduler -- rio-cli status
```

The pod's `RIO_TLS__*` env gives rio-cli mTLS to `localhost:9001` automatically.

</details>

## Production Considerations

- **PostgreSQL HA:** Use RDS Multi-AZ, Cloud SQL HA, or Patroni. See [Configuration: PostgreSQL Operations](./configuration.md#postgresql-operations).
- **Monitoring:** Configure Prometheus scraping and Grafana dashboards. See [Integration: Monitoring](./integration.md#monitoring-integration).
- **TLS:** Application-level mTLS via `RIO_TLS__CERT_PATH`/`KEY_PATH`/`CA_PATH` env vars (see [Configuration: TLS/mTLS](./configuration.md#tls--mtls)). The prod overlay's `cert-manager.yaml` issues per-component certs from a self-signed CA. A service mesh (Istio/Linkerd) also works and may be preferred for large deployments.
- **Backups:** PostgreSQL backups are critical. S3 data is durable by default. No additional backup needed for chunk storage.

## Upgrades

- **Schema migrations:** Run via `sqlx::migrate!` (uses sqlx's built-in PG advisory lock internally). All migrations are forward-compatible; rollback is supported by deploying the previous binary version (it ignores unknown columns/tables). Note: sqlx's lock covers single-service migrations; multi-replica store deployment needs a migration-lock mechanism (hence `replicas: 1` today).
- **Rolling updates:** Worker StatefulSets (created by rio-controller) set `terminationGracePeriodSeconds: 7200` --- the worker's SIGTERM handler blocks on its build semaphore until in-flight builds complete, then exits 0. Gateway pods use the Kubernetes default (30s); no extended grace period is configured in the base manifests. Use `maxUnavailable: 1` in the StatefulSet update strategy.
- **Blue/green deployments:** Supported if separate PostgreSQL schemas and S3 key prefixes are used per deployment. The gateway can be switched atomically via NLB target group changes.
- **Version skew policy:** Gateway and worker binaries can be at most 1 minor version behind the scheduler and store. The scheduler and store must be upgraded first.

## Disaster Recovery

- **PostgreSQL:** Standard backup/restore via `pg_dump`, WAL archiving, or managed service snapshots (e.g., RDS automated backups). PostgreSQL is the authoritative source for all metadata (narinfo, chunk manifests, scheduling state, build history). **PG metadata cannot be reconstructed from S3 alone.**
- **S3:** Durable by default (11 nines). Chunk data in S3 is the source of truth for build artifacts. Enable S3 versioning as defense against accidental deletes.
- **Recovery procedure:** Restore PostgreSQL from backup, verify S3 bucket accessibility, restart all components. Workers reconnect and re-register.

    > **State recovery (Phase 3b):** On `LeaderAcquired` (lease acquisition), the scheduler calls `recover_from_pg` which rebuilds the in-memory DAG from PostgreSQL: loads non-terminal builds + derivations + edges + build_derivations, reconstructs `DerivationState` via `from_recovery_row`, recomputes critical-path priorities, repopulates the ready queue. The lease loop fire-and-forgets `LeaderAcquired` (non-blocking — keeps renewing during recovery); `recovery_complete` flag gates dispatch. If recovery fails (PG down), sets `recovery_complete=true` anyway with an empty DAG (degrade to pre-recovery behavior, don't block). Generation counter seeded from `MAX(assignments.generation) + 1` via `fetch_max` for defensive monotonicity. See `rio-scheduler/src/actor/recovery.rs`.
- **RPO:** Determined by PostgreSQL backup frequency. With WAL archiving, RPO can be near-zero. S3 data has effectively zero RPO.
- **RTO:** Determined by PostgreSQL restore time + component restart time. Typically 5-15 minutes for managed databases.

See [Multi-Tenancy](./multi-tenancy.md) for tenant isolation configuration
(resource quotas, per-tenant signing keys, narinfo visibility filtering).
