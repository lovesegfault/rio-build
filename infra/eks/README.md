# EKS deployment for rio-build

One-shot bring-up: OpenTofu for infra, `push-images.sh` for images,
`just eks deploy` for the chart, `smoke-test.sh` for verification. Driven
by the justfile at the repo root — `just --list` for the menu.

## What gets created

| Component | Resource | Notes |
|---|---|---|
| EKS cluster | 1.33, 1 nodegroup | system (3× m5.large, untainted) |
| Karpenter | helm_release + Pod Identity + SQS | provisions worker nodes on-demand (c6a/c7a preferred, m/r fallback) |
| Aurora PG | Serverless v2, 0.5-2 ACU | shared by scheduler + store, password in Secrets Manager |
| S3 bucket | NAR chunk storage | name: `<cluster_name>-chunks-<random>` |
| ECR repos | 7 (gateway/scheduler/store/controller/worker/fod-proxy/bootstrap) | immutable tags, keep-last-10 lifecycle |
| cert-manager | helm_release | issues mTLS certs for intra-cluster gRPC |
| aws-load-balancer-controller | helm_release + IRSA | provisions the gateway NLB |
| SSM bastion | t3.micro, private subnet | tunnel to the internal NLB — no inbound SG rules |

## Prerequisites

- `AWS_PROFILE` with admin-ish permissions in the target account
  (EKS, RDS, EC2, IAM, S3, ECR, Secrets Manager — it's a lot)
- [Session Manager plugin](https://docs.aws.amazon.com/systems-manager/latest/userguide/session-manager-working-with-install-plugin.html)
  for `aws ssm start-session` (separate binary from awscli)
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
just eks bootstrap                # ~5s if already set up

# Full bring-up: apply (prompts) → kubeconfig → push → deploy
just eks up                       # ~25min (EKS ~12min, Aurora ~8min, push ~3min, deploy ~2min)

# Or piecewise:
# just eks apply                  # tofu apply (prompts)
# just eks kubeconfig             # aws eks update-kubeconfig
# just eks push                   # nix build + skopeo copy to ECR, zstd layers
# just eks deploy                 # render + kubectl apply

# Verify
kubectl get nodes                 # should show 3 system nodes Ready (workers scale from 0 on demand)
just eks smoke                    # ~5min — builds nixpkgs#hello, kills a worker, asserts reassign
```

`just eks up-auto` skips the tofu apply prompt (`-auto-approve`).

### State backend configuration

Both `infra/eks/bootstrap` and `infra/eks` store state in the same S3
bucket (bootstrap is self-referential — it manages the bucket it
stores its own state in). Bucket name and region are passed via
`-backend-config` by the justfile, so nothing account-specific is
committed. Defaults:

| Var | Default | Override in `.env.local` |
|---|---|---|
| bucket | `rio-tfstate-${account_id}` (from `aws sts`) | `RIO_TFSTATE_BUCKET` |
| region | `us-east-2` | `RIO_TFSTATE_REGION` |

Running in a fresh AWS account just works: `just eks bootstrap`
computes the bucket name, creates it, migrates state into it.
Everything downstream reads the same computed name.

## Iterating

The cluster stays up. `just eks deploy` runs `helm upgrade` from the
working tree — chart changes deploy without commit/push. Code changes
need a push (image tag is derived from git SHA + dirty-tree hash):

```bash
# Chart-only change (template/values): no push needed
just eks deploy

# Code change: push new image + deploy
just eks push eks deploy
```

Deploy history: `just eks history`. Rollback: `just eks rollback [REV]`
(omit REV for previous).

## Autoscaling

Two layers, chained:

1. **Pod layer** (`rio-controller/src/scaling.rs`): polls scheduler queue depth every 30s, SSA-patches the WorkerPool StatefulSet replica count between `spec.replicas.{min,max}`. Scale-up is immediate; scale-down requires 10 minutes of quiet (`SCALE_DOWN_WINDOW`).
2. **Node layer** (Karpenter): watches for Pending pods that can't schedule, provisions an EC2 instance that fits (~30-60s boot). When pods scale to zero, empty nodes are consolidated after `consolidateAfter` (30s for workers, 5m for general).

The chain: build submitted → queue depth > 0 → rio-controller scales STS to N → N pods Pending (no worker nodes exist) → Karpenter provisions node(s) → pods Running. Cold start from zero: ~50-80s. `consolidationPolicy: WhenEmpty` means Karpenter never evicts a worker mid-build — only consolidates after rio-controller has scaled the pods away.

Three NodePools (weighted priority): `rio-worker-preferred` (c6a/c7a, weight 100), `rio-worker-fallback` (m/r-category, weight 10), `rio-general` (untainted, for future gateway/scheduler HPA overflow). One shared EC2NodeClass. Configured in `infra/helm/rio-build/values.yaml` under `karpenter.nodePools`.

## Cost (us-east-2, on-demand)

| Item | ~USD/mo |
|---|---|
| EKS control plane | $73 (fixed) |
| 3× m5.large (system) | ~$210 |
| Karpenter worker nodes | $0 idle; ~$55/mo per c6a.large while building |
| Aurora Serverless v2 @ 0.5 ACU | ~$44 |
| NAT Gateway | ~$35 + data |
| t3.micro bastion | ~$8 |
| **Total (idle, no builds)** | **~$370/mo** |

Worker cost scales with build load. rio-controller's 10-min scale-down + Karpenter consolidation means an hour of intermittent builds ≈ 1h of node time. Aurora at 2 ACU adds ~$130/mo.

## Teardown

```bash
just eks destroy                  # ~15min
```

This deletes WorkerPools first (their finalizers hold pods → NLB
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
`just eks push` wasn't run (no image at that tag in ECR). Check the
current release values: `helm get values rio -n rio-system | grep tag`.

**Scheduler/store CrashLoopBackOff with PG connection errors:**
Check the rio-postgres Secret: `kubectl -n rio-system get secret
rio-postgres -o jsonpath='{.data.url}' | base64 -d`. If it's
missing `?sslmode=require`, Aurora (rds.force_ssl=1) rejects the
connection. Check the rio-postgres ExternalSecret status.

**smoke-test.sh SSM tunnel times out:**
Check `aws ssm describe-instance-information --region us-east-2`
— the bastion should show `PingStatus: Online`. If not, the SSM
agent isn't connecting (usually NAT gateway routing or instance
profile misconfiguration). `/tmp/ssm-session.log` has the client
side.
