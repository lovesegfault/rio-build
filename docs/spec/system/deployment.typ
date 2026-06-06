#import "/lib/rio.typ": *
#show: rio.with(domains: ("infra",))

This guide covers deploying rio-build to a Kubernetes cluster. For development, see #cross-link("/contributing.typ")[Contributing].

= Prerequisites

- Kubernetes 1.33+ (EKS, GKE, or self-managed) --- required for user namespace isolation (`hostUsers: false`), see #cross-link("/spec/system/security.typ")[Security §Rationale]
- PostgreSQL 15+ (managed service recommended: RDS, Cloud SQL, or CloudNativePG). Aurora/RDS PG 15+ have `rds.force_ssl=1` by default --- the connection string must include `?sslmode=require` (sqlx has `tls-rustls-aws-lc-rs` enabled for this)
- S3-compatible object storage (AWS S3, MinIO, GCS with S3 compatibility)
- `kubectl` configured for the target cluster

= Component Topology

#table(
  columns: 4,
  table.header([Component], [K8s Resource], [Replicas], [Notes]),
  [rio-gateway],
  [Deployment],
  [2+],
  [Stateless (per-connection ephemeral state only). Behind #gls("nlb").],

  [rio-scheduler],
  [Deployment],
  [2 (leader-elected)],
  [Leader election via Kubernetes Lease. One leader, one hot standby. Failover: next 5s poll after graceful step_down; \~20--25s on ungraceful death (`STEAL_AFTER` 19s + one 5s poll).],

  [rio-store],
  [Deployment],
  [2--14 (KEDA-autoscaled)],
  [Stateless at runtime (PG + S3 hold everything). Multi-replica is safe: startup migrations serialize via #rref("store.db.migrate-try-lock"). Replica count owned by a KEDA ScaledObject with one-per-node spread (see Store autoscaling below); k3s/dev profiles disable autoscaling and render a static count.],

  [rio-controller],
  [Deployment],
  [1],
  [K8s operator. *Single replica by chart default*; the `nodeclaim_pool` reconciler is leader-elected via `rio_lease`. Leader-elected components: #(refs.leased-components)().],

  [rio-builder],
  [Job (ephemeral)],
  [0+ (autoscaled)],
  [Managed by rio-controller via Pool @crd (`kind: Builder`). Requires dedicated node pool.],
)

= Deployment Order

+ *External dependencies* (PostgreSQL, S3 bucket; Cilium CNI must be installed first — nodes are NotReady until it lands)
+ *rio-controller* (creates CRDs, starts watching for resources)
+ *rio-store* (needs PostgreSQL and S3)
+ *rio-scheduler* (needs PostgreSQL and rio-store)
+ *rio-gateway* (needs rio-scheduler and rio-store)
+ *Pool CRD* (rio-controller creates and manages builder/fetcher Jobs)

`helm upgrade --wait` blocks until all Deployments report Available — strict ordering isn't enforced, but no component is externally reachable until the release as a whole is Ready. Readiness probes on each component ensure this: store readiness requires PG migrations done, scheduler readiness requires store reachable, gateway readiness requires scheduler reachable.

= Minimum Viable Deployment

For development or evaluation, a minimal deployment needs:

```
1x rio-gateway
1x rio-scheduler
1x rio-store
1x rio-controller
1x Pool (kind: Builder, maxConcurrent: 2)
1x PostgreSQL (single instance, e.g., via CloudNativePG)
1x MinIO (for S3-compatible storage)
```

This fits in a 4-node cluster (1 control plane + 1 general workload + 2 worker nodes with taints).

= Executor Node Pool

Executors require a dedicated node pool with:

#r("infra.node.nixos-ami")[
  The EKS reference deployment builds its own NixOS worker-node AMI (`nix build .#packages.<arch>-linux.ami`) and selects it via a tag-matched `EC2NodeClass` with `amiFamily: AL2023` so Karpenter emits NodeConfig userData for the packaged `nodeadm` to consume. The AMI bakes in the `user.max_user_namespaces` sysctl, the Localhost seccomp profiles, `cgroup_writable=true` for containerd, the `EROFS_FS_ONDEMAND`/`CACHEFILES_ONDEMAND` kernel options, and `/dev/{fuse,kvm}` injection via containerd `base_runtime_spec` — the chart renders no `userData` at all. See ADR-021. The pipeline is `cargo xtask k8s -p eks up --ami` (writes `.rio-ami-tag`) → `cargo xtask k8s -p eks up --deploy` (sets `karpenter.amiTag` from that file). The tag is content-addressed (12 hex of `sha256(∑ drvPaths)`), so a no-op `up` skips the coldsnap upload entirely; `up --deploy` recomputes the same tag if `.rio-ami-tag` is absent.
]

#r("infra.node.prebake-layer-warm")[
  The AMI also bakes a `rio-executor-seed.oci.tar` (builder+fetcher images, deduplicated layers, \~124 MB) and imports it into containerd's content store via a `containerd-seed-warm` oneshot that runs concurrently with kubelet registration (the import completes inside kubelet's \~5–15 s TLS-bootstrap window; if it loses the race, the first pod pull is cold-from-ECR — degraded, not broken). PodSpec image refs stay `<ECR>/rio-{builder,fetcher}:<git-sha>` — the seed is a layer-cache warm, not the pulled ref. On a fresh node's first pod, containerd resolves the ECR manifest and finds most layer blobs already local by digest; only layers that changed since the AMI was cut are fetched (typically the \~10 MB `rio-workspace` top layer, or zero if AMI and deploy are at the same commit). The seed's `seed.local/…:prebaked` image-store refs are pinned (`io.cri-containerd.pinned=pinned`) so kubelet image-GC can't reclaim the layer blobs before the first pod runs. Dev loop is unchanged (`up --push --deploy`); `up --ami` is an optional optimization to keep the delta near zero.
]

- *Taint:* `rio.build/executor=true:NoSchedule` (only executor pods scheduled here). Note: system pods (coredns) need at least one untainted node — use a separate system node group.
- *Instance type:* Compute-optimized (e.g., `c8a.xlarge` on AWS). For IO-heavy builds the `rio-builder-nvme-{x86,aarch64}` NodePools select instance-store-NVMe families via `karpenter.k8s.aws/instance-local-nvme > 0`; the AMI's `rio-nvme-mount.service` (early boot, before tmpfiles and nodeadm) stripes the instance-store devices into `/dev/md0`, formats XFS, and mounts at `/var/lib/kubelet` with `prjquota` so kubelet enforces per-pod ephemeral-storage via XFS project quotas. The `rio-nvme` EC2NodeClass sets `instanceStorePolicy: RAID0` so @karpenter's bin-pack simulation counts NVMe capacity toward ephemeral-storage; nodeadm does not act on it (the AMI's `nodeadm init --skip run` never executes the local-disk aspect), so `rio-nvme-mount.service` still owns the mdadm→mkfs.xfs+prjquota chain uncontested.
- *AMI:* the NixOS node AMI (`.#packages.<arch>-linux.ami`, ADR-021). Amazon Linux 2 (AL2, kernel 5.10) does *NOT* support #gls("overlayfs")-over-@fuse and is not compatible with rio-build executors.
- *Kernel:* Linux 6.1+ (for overlayfs-over-FUSE support). Linux 6.9+ recommended for FUSE passthrough mode. Verify with `uname -r` on worker nodes.
- *#gls("imdsv2"):* Hop limit = 1 (defense-in-depth against metadata access from containers)
- *Pod spec:* `hostUsers: false` is incompatible with `/dev/fuse` hostPath volumes (kernel rejects idmap mounts on device nodes). containerd `base_runtime_spec` injects `/dev/{fuse,kvm}` directly (OCI `linux.devices` — runc `mknod`s inside the container's `/dev`); see `nix/base-runtime-spec.nix` (NixOS AMI: `nix/nixos-node/containerd-config.nix`; k3s VM fixture: `services.k3s.containerdConfigTemplate` in `nix/tests/fixtures/k3s-full.nix`).
- *`/dev/fuse` access:* Executor pods need access to `/dev/fuse`. A `hostPath` volume with `privileged: true` works for development but production should use `base_runtime_spec` device injection to avoid granting full privileges. `CAP_SYS_ADMIN` alone is not sufficient for `/dev/fuse` access — the container's device cgroup must also allow the FUSE character device.
- *EKS addons:* `vpc-cni` and `kube-proxy` must be installed before node groups are created (they are daemonsets). `coredns` requires schedulable (untainted) nodes and should be installed after the system node group is ready.

== Node autoscaling

Builder pod autoscaling (rio-controller) and node autoscaling (cluster autoscaler or Karpenter) are separate concerns that chain together. rio-controller spawns Pool Jobs based on scheduler queue depth; the node autoscaler provisions capacity for the resulting Pending pods. Without a node autoscaler, rio-controller scaling beyond the static node pool's capacity just produces permanently-Pending pods.

The EKS reference deployment (`infra/eks/`) uses Karpenter: the `executors` managed nodegroup is replaced entirely with three Karpenter NodePools (compute-optimized preferred, general-purpose fallback, untainted general). `consolidationPolicy: WhenEmpty` on builder NodePools means Karpenter never evicts a node with a builder pod on it --- ephemeral Jobs terminate on completion, then Karpenter consolidates the empty node. Scale-to-zero is the default: cold start from zero is \~50-80s (node boot + pod start).

== Store autoscaling

#r("infra.store.autoscaling+3")[
  The rio-store Deployment's replica count MUST have exactly one writer ---
  the KEDA ScaledObject (`templates/store-scaledobject.yaml`): when
  `store.autoscaling.enabled` the chart MUST NOT render a static
  `spec.replicas` (on an existing release it echoes the live Deployment's
  count via `lookup` so an upgrade never null-patches the field), and the
  chart MUST define no ComponentScaler CR targeting the store. The
  ScaledObject scales on three triggers --- substitution backlog
  (#(refs.metric)("rio_scheduler_substituting_derivations") per replica,
  the leading signal, thresholded in JOB units by
  `targetBacklogJobsPerReplica`), builders-per-replica
  (#(refs.metric)("rio_scheduler_open_attempts") per replica), and CPU
  utilization (reactive corrective); every prometheus trigger MUST
  render through the unit-checking helper (`rio.promTrigger` ×
  `files/metric-units.json` --- a metric/knob unit mismatch MUST fail
  the render). Scale-up unstabilized, scale-down damped (1800 s window,
  max(25 %, 1 pod) / 600 s); floor 2 with a values-configurable ceiling
  (default 173, overridden on every EKS deploy by the pg preflight's
  derivation from the MEASURED Aurora `max_connections`) that MUST be
  the PG-connection backstop (`derive_store_ceiling`: 70 % of the
  budget minus non-store consumers, over `pgMaxConnections`), not a
  product cap --- the operative scale limit MUST be the Karpenter
  `rio-general` pool. Disruption rules key on NAMED axis predicates:
  required one-per-node `podAntiAffinity` (`kubernetes.io/hostname`)
  gated on the CEILING (`rio.mayRunMultiple`); the explicit
  `maxUnavailable: 1` rollout strategy gated on the FLOOR
  (`rio.alwaysRunsMultiple` --- at one live replica it would mark the
  Deployment Available at zero ready pods); the store
  PodDisruptionBudget (`maxUnavailable` 10 %) rendered UNCONDITIONALLY
  on the scale axes (a percentage budget rounds up --- harmless at one
  replica, protective at every scale).
]
The store carries three superimposed load classes --- substitution ingest
(upstream → store → S3; leading indicator: the scheduler's materialization
backlog, known at merge time), builder read-serving (S3 → store → builder),
and builder upload ingest (PutPath → S3; both keyed to busy builders) ---
all flowing through the same pod NIC, NAR/chunk memory, and PG pool. One
replica per node makes scale-out add NICs rather than re-partition one
node's bandwidth; the required rule never blocks scale-out, it delays it
one Karpenter node-mint --- a Pending store pod on the untainted
on-demand pool mints a node in tens of seconds, up to the pool's
`limits.cpu`, which is the intended operative bound (correctness never
depends on scaling). The KEDA ceiling is only the connection backstop:
`maxReplicas × pgMaxConnections` stays ~30 % under the provisioned
Aurora `max_connections` (runtime-constant at the configured
`max_capacity`, and capped at 2,000 when `min_capacity` ≤ 0.5 ACU ---
the model lives in `infra/eks/rds.tf`; `xtask deploy`'s pg preflight
measures the live value, asserts it matches the model, and deploys the
ceiling derived from the measurement); the trigger thresholds are
seeded (85 backlog jobs/replica --- the gauge counts derivations, not
paths; render-time unit-checked via `files/metric-units.json` ---
50 builders/replica, 70 % CPU) and re-derived from the post-wipe
warm-phase capture. The
no-KEDA profiles (`values/vmtest-full.yaml`, `values/dev.yaml`) set
`store.autoscaling.enabled=false` and render the static `store.replicas`.

= Key Configuration

See #cross-link("/ref/configuration.typ")[Configuration Reference] for all parameters. The minimum required settings:

#table(
  columns: 2,
  table.header([Component], [Required Config]),
  [Gateway], [`host_key`, `authorized_keys`, `scheduler_addr`, `store_addr`],
  [Scheduler], [`database_url`],
  [Store],
  [`database_url`, `chunk_backend` (tagged enum: `inline` / `filesystem` / `s3`), `signing_key_path`],

  [Controller], [`scheduler_addr`],
  [Builder], [`scheduler_addr`, `store_addr`],
)

#info[
  *Store chunk backend config* uses a serde internally-tagged enum (`kind`). TOML example for S3: `[chunk_backend]` / `kind = "s3"` / `bucket = "..."` / `prefix = "..."`. Default is `inline` (NARs stored in PostgreSQL --- fine for dev, does not scale). There is no flat `s3_bucket` field.
]

= Secrets

See #cross-link("/spec/system/security.typ")[Security: Secrets Management] for recommended patterns (External Secrets Operator or Vault Agent Injector for production). At minimum, create Kubernetes Secrets for:

- SSH host key (gateway)
- Authorized SSH keys (gateway)
- @nar signing key (store)
- Database credentials (scheduler, store)
- HMAC signing key for assignment tokens (scheduler, store) --- set via `RIO_HMAC_KEY_PATH` on both. The scheduler signs Claims{executor_id, drv_hash, expected_outputs, is_ca, expiry_unix} at dispatch; the store verifies on `PutPath`. Same key file both sides (shared secret). Generate: `openssl rand -out /path/to/key 32`.

#info[
  *SSH key mounting:* On EKS deploys (`xtask k8s -p eks up`), the bootstrap Job generates `rio/gateway-host-key` in AWS Secrets Manager and ESO syncs it to the `rio-gateway-host-key` Secret; deploy sets `gateway.ssh.hostKeySecret` to that name so all replicas present the same host key across restarts. On other deployments, the chart default leaves `hostKeySecret` empty — the gateway then generates an ephemeral key per pod (fine for dev; breaks `known_hosts` on reschedule and across replicas). `gateway.ssh.authorizedKeysSecret` defaults to `rio-gateway-ssh` — create that Secret before deploy or the gateway pod blocks on the missing mount.
]

= Verification

After deployment:

```bash
# 1. Tenant bootstrap + cluster status. `xtask k8s cli` port-forwards
#    scheduler:9001 + store:9002 and runs rio-cli LOCALLY against the
#    plaintext gRPC ports. No need for rio-cli (or jq, column, …)
#    inside the scheduler image.
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
```

For a complete scripted walkthrough against EKS, run `cargo xtask k8s qa --health -p eks`.

#info(title: [Without xtask (kubectl exec fallback)])[
  If rio-cli is bundled in the scheduler image (legacy — discouraged per `r[sec.image.control-plane-minimal]`):

  ```bash
  kubectl -n rio-system exec deploy/rio-scheduler -- rio-cli create-tenant my-team
  kubectl -n rio-system exec deploy/rio-scheduler -- rio-cli status
  ```

  In-pod, rio-cli connects to `localhost:9001` over plaintext gRPC (the loopback interface is not on the Cilium overlay; encryption is moot).
]

= Production Considerations

- *PostgreSQL HA:* Use RDS Multi-@az, Cloud SQL HA, or Patroni. See #cross-link("/ref/configuration.typ")[Configuration: PostgreSQL Operations].
- *Monitoring:* Configure Prometheus scraping and Grafana dashboards. See #cross-link("/spec/system/observability.typ")[Observability].
- *Transport encryption:* Cilium WireGuard transparent encryption is on by default (`encryption.type: wireguard`). rio components speak plaintext gRPC; the overlay encrypts node-to-node. There are no per-component TLS certificates and no cert-manager dependency. See #cross-link("/spec/system/security.typ")[Security: `r[sec.transport.cilium-wireguard]`].
- *NLB target health:* With `externalTrafficPolicy: Local` on the gateway Service, the NLB shows `N/M` targets healthy where `N` = number of nodes hosting a rio-gateway pod (the rest fail the `healthCheckNodePort` probe by design — they have no local backend). This is correct, not a bug.
- *Backups:* PostgreSQL backups are critical. S3 data is durable by default. No additional backup needed for chunk storage.

= Upgrades

- *Schema migrations:* Run via `sqlx::migrate!` with sqlx's built-in lock disabled (`set_locking(false)`); rio-store's own PG advisory `pg_try_advisory_lock` serializes concurrent replicas (#rref("store.db.migrate-try-lock")). All migrations are forward-compatible; rollback is supported by deploying the previous binary version (it ignores unknown columns/tables).
- *Rolling updates:* Builder Jobs (created by rio-controller) set `terminationGracePeriodSeconds: 7200` --- the builder's SIGTERM handler blocks until its single in-flight build completes, then exits 0. Gateway pods use the Kubernetes default (30s); no extended grace period is configured in the base manifests. Builder pods are one-shot, so a control-plane upgrade naturally rolls the fleet as Jobs complete and new ones spawn with the new image.
- *Blue/green deployments:* Supported if separate PostgreSQL schemas and S3 key prefixes are used per deployment. The gateway can be switched atomically via NLB target group changes.
- *Version skew policy:* Gateway and executor binaries can be at most 1 minor version behind the scheduler and store. The scheduler and store must be upgraded first.

= Disaster Recovery

- *PostgreSQL:* Standard backup/restore via `pg_dump`, WAL archiving, or managed service snapshots (e.g., RDS automated backups). PostgreSQL is the authoritative source for all metadata (@narinfo, chunk manifests, scheduling state, build history). *PG metadata cannot be reconstructed from S3 alone.*
- *S3:* Durable by default (11 nines). Chunk data in S3 is the source of truth for build artifacts. Enable S3 versioning as defense against accidental deletes.
- *Recovery procedure:* Restore PostgreSQL from backup, verify S3 bucket accessibility, restart all components. Executors reconnect and re-register.

  #info[
    *State recovery (Phase 3b):* On `LeaderAcquired` (lease acquisition), the scheduler calls `recover_from_pg` which rebuilds the in-memory @dag from PostgreSQL: loads non-terminal builds + derivations + edges + build_derivations, reconstructs `DerivationState` via `from_recovery_row`, recomputes critical-path priorities, restores `Ready`-state claimability. The lease loop fire-and-forgets `LeaderAcquired` (non-blocking — keeps renewing during recovery); `recovery_complete` flag gates dispatch. If the DAG load fails but the PG floor is still readable, the term continues with an empty DAG yet is still floored, claimed, and (when required) confirmed before dispatch is ungated; only in the floor-unreadable fallback (PG cannot answer even the floor query) does the scheduler complete at the recovery-entry generation — after the post-claim leadership confirmation, which needs no PG — (degrade to pre-recovery behavior, don't block). The recovery generation target starts from the generation at recovery entry — the Lease-derived generation (`leaseTransitions + 1`) unless an earlier recovery's PG-floor seed already raised it — and is raised to one past the durable PG floor `GREATEST(MAX(assignments.generation), MAX(leader_generation_claims.generation))` only when that floor exceeds the entry generation, or ties it without this holder's own claim row in the ledger (a same-epoch re-acquire retains its generation); the target is durably claimed in the `leader_generation_claims` ledger (#rref("sched.lease.generation-claim")) and applied via `fetch_max` (#rref("sched.recovery.fetch-max-seed")) before dispatch is ungated (a claim-write failure or claim-conflict exhaustion proceeds unclaimed — the same one-term claim-failure residual priced in the scheduler spec; see the recovery module doc). A PostgreSQL restore from backup regresses both arms of that floor; the Lease's transition count is the redundant epoch source in that case, and only the conjunction of a PG fault (skipped claim write or restore) with a Lease deletion — both epoch sources destroyed — reaches the documented residual collision. See `rio-scheduler/src/actor/recovery.rs`.
  ]
- *RPO:* Determined by PostgreSQL backup frequency. With WAL archiving, RPO can be near-zero. S3 data has effectively zero RPO.
- *RTO:* Determined by PostgreSQL restore time + component restart time. Typically 5-15 minutes for managed databases.

See #cross-link("/spec/system/tenancy.typ")[Multi-Tenancy] for tenant isolation configuration (resource quotas, per-tenant signing keys, narinfo visibility filtering).
