# EKS Terraform for rio-build

## Prerequisites

- Terraform >= 1.5
- AWS CLI configured with `AWS_PROFILE=beme_sandbox` (or adjust)
- S3 bucket for chunk storage (pre-create: `aws s3 mb s3://my-rio-chunks`)
- cert-manager installed in cluster (after EKS is up)

## Usage

```bash
cd infra/eks
export AWS_PROFILE=beme_sandbox

# First time
terraform init

# Review plan
terraform plan -var chunk_bucket=my-rio-chunks

# Apply (~15 minutes for EKS control plane)
terraform apply -var chunk_bucket=my-rio-chunks

# Configure kubectl
$(terraform output -raw kubeconfig_command)

# Verify
kubectl get nodes
```

## Deploy rio-build

```bash
# Get IRSA role ARN for rio-store
export STORE_IAM_ROLE_ARN=$(terraform output -raw store_iam_role_arn)

# Apply EKS overlay (envsubst for IRSA patch)
cd ../../
kubectl kustomize deploy/overlays/eks | envsubst | kubectl apply -f -

# Wait for cert-manager to issue certs (if not already installed)
kubectl -n cert-manager wait --for=condition=Available deployment/cert-manager

# Wait for rio components
kubectl -n rio-system wait --for=condition=Ready pod -l app.kubernetes.io/part-of=rio-build --timeout=300s
```

## Smoke test

```bash
./infra/eks/smoke-test.sh
```

## Teardown

```bash
# Delete rio resources first (finalizers)
kubectl delete workerpools --all -n rio-system
kubectl -n rio-system delete -k deploy/overlays/eks

# Then terraform destroy
cd infra/eks
terraform destroy -var chunk_bucket=my-rio-chunks
```

## Cost estimate (us-west-2)

| Item | Approx monthly |
|---|---|
| EKS control plane | $73 (fixed) |
| 3× m5.large (system) | ~$210 |
| 2× c6a.xlarge (workers min) | ~$220 |
| NAT Gateway | ~$35 + data |
| **Total (min workers)** | **~$540/mo** |

Scaling to 10 workers adds ~$1100/mo. Consider spot instances for
the worker nodegroup (builds are interruptible — the scheduler
reassigns).
