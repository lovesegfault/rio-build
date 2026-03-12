# EKS deployment for rio-build

One-shot bring-up: OpenTofu for infra, `nix run .#push-images` for
images, `deploy.sh` for manifests, `smoke-test.sh` for verification.

## What gets created

| Component | Resource | Notes |
|---|---|---|
| EKS cluster | 1.33, 2 nodegroups | system (3× m5.large) + workers (2-10× c6a.xlarge, tainted) |
| Aurora PG | Serverless v2, 0.5-2 ACU | shared by scheduler + store, password in Secrets Manager |
| S3 bucket | NAR chunk storage | name: `<cluster_name>-chunks-<random>` |
| ECR repos | 5 (gateway/scheduler/store/controller/worker) | immutable tags, keep-last-10 lifecycle |
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
export AWS_PROFILE=beme_sandbox
cd infra/eks

# Optional: cp terraform.tfvars.example terraform.tfvars && edit

nix develop -c tofu init
nix develop -c tofu apply         # ~20min (EKS ~12min, Aurora ~8min)

# Configure kubectl
$(nix develop -c tofu output -raw kubeconfig_command)
kubectl get nodes                 # should show 5 nodes Ready

# Push images
cd ../..
export ECR_REGISTRY=$(nix develop -c tofu -chdir=infra/eks output -raw ecr_registry)
nix run .#push-images             # ~3min, prints RIO_IMAGE_TAG=<sha>

# Deploy rio
./infra/eks/deploy.sh             # ~2min

# Smoke test
./infra/eks/smoke-test.sh         # ~5min — builds nixpkgs#hello, kills a worker, asserts reassign
```

## Iterating

The cluster stays up. To deploy a code change:

```bash
git commit -am "..."
nix run .#push-images             # new SHA → new ECR tag
./infra/eks/deploy.sh             # rolls out the new tag
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
# Delete rio first — WorkerPool finalizers block terraform destroy
# if pods are still running (the NLB won't delete with targets).
kubectl -n rio-system delete workerpool --all --wait=true
kubectl delete -k /tmp/rio-rendered-overlay  # or rerun deploy.sh's render step

cd infra/eks
nix develop -c tofu destroy       # ~15min
```

The S3 bucket has `force_destroy = true` so it deletes even with
chunks in it. Aurora has `skip_final_snapshot = true`. Both are
dev/test settings — flip them for anything you care about keeping.

## Troubleshooting

**`tofu plan` fails with "connection refused" before first apply:**
The helm/kubernetes providers try to contact the cluster during plan.
Run `tofu apply -target=module.eks` first, then full `tofu apply`.

**Pods stuck ImagePullBackOff:**
Either `nix run .#push-images` wasn't run, or `RIO_IMAGE_TAG`
doesn't match what's in ECR. `aws ecr list-images --repository-name
rio-scheduler` shows what's actually pushed.

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
