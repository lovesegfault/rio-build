# EKS deployment for rio-build

One-shot bring-up: OpenTofu for infra, `push-images.sh` for images,
`deploy.sh` for manifests, `smoke-test.sh` for verification. Driven
by the justfile at the repo root — `just --list` for the menu.

## What gets created

| Component | Resource | Notes |
|---|---|---|
| EKS cluster | 1.33, 2 nodegroups | system (3× m5.large) + workers (2-10× c6a.xlarge, tainted) |
| Aurora PG | Serverless v2, 0.5-2 ACU | shared by scheduler + store, password in Secrets Manager |
| S3 bucket | NAR chunk storage | name: `<cluster_name>-chunks-<random>` |
| ECR repos | 6 (gateway/scheduler/store/controller/worker/fod-proxy) | immutable tags, keep-last-10 lifecycle |
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
just eks-bootstrap                # ~5s if already set up

# Full bring-up: apply (prompts) → kubeconfig → push → deploy
just eks-up                       # ~25min (EKS ~12min, Aurora ~8min, push ~3min, deploy ~2min)

# Or piecewise:
# just eks-apply                  # tofu apply (prompts)
# just eks-kubeconfig             # aws eks update-kubeconfig
# just eks-push                   # nix build + skopeo copy to ECR, zstd layers
# just eks-deploy                 # render + kubectl apply

# Verify
kubectl get nodes                 # should show 5 nodes Ready
just eks-smoke                    # ~5min — builds nixpkgs#hello, kills a worker, asserts reassign
```

`just eks-up-auto` skips the tofu apply prompt (`-auto-approve`).

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

Running in a fresh AWS account just works: `just eks-bootstrap`
computes the bucket name, creates it, migrates state into it.
Everything downstream reads the same computed name.

## Iterating

The cluster stays up. To deploy a code change:

```bash
git commit -am "..."
just eks-push eks-deploy          # new SHA → new ECR tag → rollout
```

To just bounce a pod without new code:

```bash
kubectl -n rio-system rollout restart deployment/rio-scheduler
```

## Cost (us-east-2, on-demand)

| Item | ~USD/mo |
|---|---|
| EKS control plane | $73 (fixed) |
| 3× m5.large (system) | ~$210 |
| 2× c6a.xlarge (workers min) | ~$220 |
| Aurora Serverless v2 @ 0.5 ACU | ~$44 |
| NAT Gateway | ~$35 + data |
| t3.micro bastion | ~$8 |
| **Total (min workers, idle Aurora)** | **~$590/mo** |

Scaling to 10 workers adds ~$1100/mo. Aurora at 2 ACU adds ~$130/mo.

## Teardown

```bash
just eks-destroy                  # ~15min
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
Either `just eks-push` wasn't run, or `RIO_IMAGE_TAG` doesn't match
what's in ECR. `aws ecr list-images --repository-name rio-scheduler`
shows what's actually pushed.

**Scheduler/store CrashLoopBackOff with PG connection errors:**
Check the rio-postgres Secret: `kubectl -n rio-system get secret
rio-postgres -o jsonpath='{.data.url}' | base64 -d`. If it's
missing `?sslmode=require`, Aurora (rds.force_ssl=1) rejects the
connection. Re-run `deploy.sh`.

**smoke-test.sh SSM tunnel times out:**
Check `aws ssm describe-instance-information --region us-east-2`
— the bastion should show `PingStatus: Online`. If not, the SSM
agent isn't connecting (usually NAT gateway routing or instance
profile misconfiguration). `/tmp/ssm-session.log` has the client
side.
