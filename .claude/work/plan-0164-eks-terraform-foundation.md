# Plan 0164: EKS Terraform foundation — Aurora + S3 + ECR + SSM bastion + justfile

## Design

Full AWS infrastructure for running rio-build in EKS. Three terraform modules: `eks-bootstrap` (self-referential S3 state backend), `eks` (EKS cluster, Aurora PostgreSQL, S3 chunk bucket, ECR repos, SSM bastion), plus kustomize overlays and a justfile-driven deploy pipeline. OpenTofu instead of Terraform — HashiCorp's BSL relicensing makes `pkgs.terraform` unfree in nixpkgs; tofu is drop-in compatible.

The iteration across 32 commits is standard "first contact with a cloud provider" friction: Aurora SG uses `node_security_group_id` not `cluster_primary` (`dd43284`); cert-manager's deprecated `installCRDs` helm key (`23ce515`); skopeo `--insecure-policy` for ECR push (`2b9b89e`); deploy.sh shadowing `TMPDIR` (`f51dd34`); deploy.sh throwaway SSH key → real key from `.env.local` (`c41c41d` → `9e3034c`); scheduler `maxUnavailable=1` to unblock rolling updates (`6250327`); scheduler Lease Role missing `patch` verb (`8026632`).

Tooling ergonomics: justfile module structure (`just eks <recipe>`), `.env.local` via direnv for per-user `AWS_PROFILE`, dirty-tree pushes with diff-hash tag suffix, parallel skopeo pushes, zstd image compression level 6. The `deploy/` directory was `git mv`'d to `infra/k8s/` for clarity (`7c06a32..8620ace`).

One rust-side fix landed mid-EKS: `9fb099d` updated `rio-store/fuzz/Cargo.lock` for sqlx's transitive `webpki-roots` dep change (independent lockfile).

## Files

```json files
[
  {"path": "infra/eks/main.tf", "action": "NEW", "note": "EKS cluster, terraform-aws-eks module, Aurora, SSM bastion"},
  {"path": "infra/eks/aurora.tf", "action": "NEW", "note": "Aurora PostgreSQL, node SG ingress"},
  {"path": "infra/eks/ecr.tf", "action": "NEW", "note": "per-component ECR repos"},
  {"path": "infra/eks/s3.tf", "action": "NEW", "note": "chunk storage bucket"},
  {"path": "infra/eks/variables.tf", "action": "NEW", "note": "worker_*, cluster_name, region"},
  {"path": "infra/eks/outputs.tf", "action": "NEW", "note": "cluster_endpoint, aurora_endpoint, ecr URLs"},
  {"path": "infra/eks/bootstrap/main.tf", "action": "NEW", "note": "self-referential S3 state backend"},
  {"path": "infra/eks/push-images.sh", "action": "NEW", "note": "parallel skopeo push, zstd, explicit policy"},
  {"path": "infra/eks/deploy.sh", "action": "NEW", "note": "kustomize apply, dry-run=server, scheduler readyReplicas=1 wait"},
  {"path": "infra/eks/smoke-test.sh", "action": "NEW", "note": "SSM tunnel, nix-build via gateway"},
  {"path": "infra/lib/ssh-key.sh", "action": "NEW", "note": "shared helper for .env.local SSH key"},
  {"path": "infra/k8s/base/kustomization.yaml", "action": "NEW", "note": "base layer (git mv from deploy/)"},
  {"path": "infra/k8s/overlays/prod/kustomization.yaml", "action": "NEW", "note": "tls-mounts patch"},
  {"path": "justfile", "action": "NEW", "note": "cloud-prefixed deploy recipes"},
  {"path": "infra/eks.just", "action": "NEW", "note": "module for `just eks <recipe>`"},
  {"path": "infra/dev.just", "action": "NEW", "note": "local dev recipes"},
  {"path": "flake.nix", "action": "MODIFY", "note": "devshell: awscli2, opentofu, kubectl, skopeo, helm, grpcurl, jq, openssl, just, git"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "nix-built fod-proxy image (replaces ubuntu/squid); zstd compression"}
]
```

## Tracey

No tracey markers — infrastructure, no spec behaviors.

## Entry

- Depends on P0148: phase 3b complete

## Exit

Merged as `9d669b6`, `7b9a8f1..7260b8d` + infra-related riders (32 commits total). `just eks deploy` → smoke test passes on cold cluster.
