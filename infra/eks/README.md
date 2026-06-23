# EKS deployment for rio-build

One-shot bring-up via `cargo xtask k8s -p eks up`: OpenTofu for infra,
nix-built images pushed to ECR, helm chart applied, smoke-test verified.
`cargo xtask k8s --help` for the full menu.

## What gets created

| Component | Resource | Notes |
|---|---|---|
| EKS cluster | 1.33, 1 nodegroup | system (3× m5.large, untainted) |
| Karpenter | helm_release + Pod Identity + SQS | provisions worker nodes on-demand (c6a/c7a preferred, m/r fallback) |
| Aurora PG | Serverless v2, 0.5-2 ACU | shared by scheduler + store, password in Secrets Manager |
| S3 bucket | NAR chunk storage | name: `<cluster_name>-chunks-<random>` |
| ECR repos | 8 (gateway/scheduler/store/controller/builder/fetcher/bootstrap/dashboard) | immutable tags, keep-last-30 lifecycle |
| cilium | helm_release | CNI + WireGuard encryption + Gateway API + kube-proxy replacement |
| aws-load-balancer-controller | helm_release + IRSA | provisions the gateway NLB |

## Prerequisites

- `AWS_PROFILE` with admin-ish permissions in the target account
  (EKS, RDS, EC2, IAM, S3, ECR, Secrets Manager — it's a lot)
- `nix develop` in the repo root gives you everything else
  (opentofu, kubectl, awscli2, skopeo, helm, jq)

## Bring-up

```bash
# ONE-TIME per user: per-user env (AWS_PROFILE). Gitignored.
cp .env.local.example .env.local  # edit AWS_PROFILE if not beme_sandbox
direnv allow

# ONE-TIME per AWS account: S3 state bucket. Idempotent — detects
# whether state already exists in S3; if not, does the first-time
# dance (local apply → create bucket → migrate state to S3).
cargo xtask k8s -p eks up --bootstrap   # ~5s if already set up

# Full bring-up: apply → kubeconfig → push → deploy
cargo xtask k8s -p eks up               # ~25min (EKS ~12min, Aurora ~8min, push ~3min, deploy ~2min)

# Or piecewise:
# cargo xtask k8s -p eks up --apply     # tofu apply
# cargo xtask k8s -p eks up --kubeconfig
# cargo xtask k8s -p eks up --push      # nix build + skopeo copy to ECR, zstd layers
# cargo xtask k8s -p eks up --deploy    # render + kubectl apply

# Verify
kubectl get nodes                       # should show 3 system nodes Ready (workers scale from 0 on demand)
cargo xtask k8s -p eks smoke            # ~5min — builds nixpkgs#hello, kills a worker, asserts reassign
```

### State backend configuration

Both `infra/eks/bootstrap` and `infra/eks` store state in the same S3
bucket (bootstrap is self-referential — it manages the bucket it
stores its own state in). Bucket name and region are passed via
`-backend-config` by xtask, so nothing account-specific is
committed. Defaults:

| Var | Default | Override in `.env.local` |
|---|---|---|
| bucket | `rio-tfstate-${account_id}` (from `aws sts`) | `RIO_TFSTATE_BUCKET` |
| region | `us-east-2` | `RIO_TFSTATE_REGION` |

Running in a fresh AWS account just works: `cargo xtask k8s -p eks up
--bootstrap` computes the bucket name, creates it, migrates state into
it. Everything downstream reads the same computed name.

### Multiple clusters in one account

Each cluster gets its own tofu workspace + `cluster_name`. Most
resources are `${cluster_name}-`-prefixed, but a few are
account-global and need handling on the second cluster:

```bash
# 1. Personal workspace (state isolated at env:/<name>/eks/).
#    Do NOT set TF_WORKSPACE in .env.local — it would also leak
#    into infra/eks/bootstrap, which is account-global.
tofu -chdir=infra/eks workspace new <name>

# 2. cluster_name in .env.local (default "rio-build" collides on IAM):
#    TF_VAR_cluster_name=rio-<name>

# 3. ECR repos are named rio-* (no cluster prefix) and already exist
#    in the default workspace's state. Import them so apply doesn't
#    try to recreate:
for img in gateway scheduler store controller builder fetcher bootstrap dashboard; do
  tofu -chdir=infra/eks import "aws_ecr_repository.rio[\"$img\"]" "rio-$img"
done
#    Note: every workspace's `tofu destroy` will then delete the
#    shared repos (force_delete=true) — destroy with care.

# 4. Each cluster's NAT gateway needs one Elastic IP. The default
#    EC2-VPC EIP quota is 5; a 5th+ cluster needs a quota bump
#    (Service Quotas → ec2 → L-0263D0A3). Without it, the NAT fails
#    to create and the system node group goes CREATE_FAILED with
#    "Instances failed to join the kubernetes cluster" (no internet
#    egress) — taint the node group + re-apply after the bump.
```

### Cross-arch builds

`up --push` builds docker images for both `x86_64-linux` and
`aarch64-linux`, and `up --ami` builds both arches' node AMIs (the
deploy preflight requires all three AMIs to exist). On a single-arch
host without binfmt emulation, set `RIO_REMOTE_STORE` in `.env.local`
to an `ssh-ng://` Nix store that can build both:

```
RIO_REMOTE_STORE=ssh-ng://builder.example
```

xtask then offloads the multi-arch `nix build` there and copies
results back. Alternatively, pre-build
`.#packages.{x86_64,aarch64}-linux.{dockerImages,ami}` (and
`.#packages.x86_64-linux.ami-bios`) into the local store via your own
remote-build mechanism before running `up`; xtask will find them
cached.

`--ami-arch x86-64` skips building the arm64 AMI, but does **not**
skip the arm64 docker images or the deploy-time arm64 AMI check — it
is not a single-arch escape hatch.

## Iterating

The cluster stays up. `cargo xtask k8s -p eks up --deploy` runs `helm
upgrade` from the working tree — chart changes deploy without
commit/push. Code changes need a push (image tag is derived from git
SHA + dirty-tree hash):

```bash
# Chart-only change (template/values): no push needed
cargo xtask k8s -p eks up --deploy

# Code change: push new image + deploy
cargo xtask k8s -p eks up --push --deploy
```

## Autoscaling

Two layers, chained:

1. **Pod layer** (`rio-controller`): builders are ephemeral one-shot Jobs — one pod per derivation, spawned on dispatch, deleted on completion. The controller gates spawn rate against each Pool's `spec.maxConcurrent`; there is no replica count to scale.
2. **Node layer**: for builder/fetcher cells, `rio-controller`'s `nodeclaim_pool` reconciler mints NodeClaims directly against `rio-nodeclaim-shim` (ADR-023 §13b) and reaps idle ones via the NA consolidation model, floored at `karpenter.nodeclaimPool.minConsolidationTime` (300s for builders, 600s for fetchers). For `rio-general`, Karpenter watches Pending pods and consolidates `WhenEmpty` after `consolidateAfter` (5m). EC2 boot is ~30-60s.

The chain: build submitted → scheduler dispatches → controller creates a Job + a NodeClaim if no node fits → pod Pending → node boots → pod Running. Cold start from zero: ~50-80s. `karpenter.sh/do-not-disrupt` on builder pods means a node is never evicted mid-build.

Two static NodePools: `rio-nodeclaim-shim` (`limits.cpu:0` — Karpenter sees it but never provisions; `rio-controller`'s nodeclaim_pool reconciler creates NodeClaims directly per ADR-023 §13b/§13c — for both builders AND fetchers since §13e), `rio-general` (untainted, for future gateway/scheduler HPA overflow). Three EC2NodeClasses: `rio-default` (UEFI/UKI), `rio-nvme` (UEFI/UKI + RAID0 instance store), `rio-metal` (BIOS for x86 `.metal`, UEFI for arm64 `.metal` — KVM builds). Builder and fetcher hardware classes (incl. metal) live in `infra/helm/rio-build/values.yaml` under `scheduler.sla.hwClasses`; static NodePools under `karpenter.nodePools`.

## Cost (us-east-2, on-demand)

| Item | ~USD/mo |
|---|---|
| EKS control plane | $73 (fixed) |
| 3× m5.large (system) | ~$210 |
| Karpenter worker nodes | $0 idle; ~$55/mo per c6a.large while building |
| Aurora Serverless v2 @ 0.5 ACU | ~$44 |
| NAT Gateway | ~$35 + data |
| **Total (idle, no builds)** | **~$362/mo** |

Worker cost scales with build load. Ephemeral Jobs exit on completion + Karpenter consolidation means an hour of intermittent builds ≈ 1h of node time. Aurora at 2 ACU adds ~$130/mo.

## Teardown

```bash
cargo xtask k8s -p eks destroy    # ~15min
```

This deletes Pool CRs first (their finalizers hold pods → NLB
→ tofu destroy blocks), then `tofu -chdir=infra/eks destroy`.

The S3 bucket has `force_destroy = true` so it deletes even with
chunks in it. Aurora has `skip_final_snapshot = true`. Both are
dev/test settings — flip them for anything you care about keeping.

The state bucket (`infra/eks/bootstrap`) is NOT destroyed by this —
it's a per-account fixture. Destroy it separately (and manually
empty it first — no `force_destroy` on state buckets, losing
state orphans resources).

## Troubleshooting

**`tofu plan` fails with "connection refused" before first apply:**
The helm/kubernetes providers try to contact the cluster during plan.
Run `tofu apply -target=module.eks` first, then full `tofu apply`.

**Pods stuck ImagePullBackOff:**
images weren't pushed (no image at that tag in ECR). Re-run `cargo
xtask k8s -p eks up --push`. Check the current release values:
`helm get values rio -n rio-system | grep tag`.

**Scheduler/store CrashLoopBackOff with "database schema" errors:**
Most common cause: the rio-migrate Job hasn't completed (or failed) —
app pods verify the schema at startup and crash-restart until the
Job lands it. Check `kubectl -n rio-system get jobs -l
app.kubernetes.io/name=rio-migrate` and the newest Job's pod logs.

**Scheduler/store CrashLoopBackOff with PG connection errors:**
Check the rio-postgres Secret: `kubectl -n rio-system get secret
rio-postgres -o jsonpath='{.data.url}' | base64 -d`. If it's
missing `?sslmode=require`, Aurora (rds.force_ssl=1) rejects the
connection. Check the rio-postgres ExternalSecret status.
