set dotenv-load := true
set dotenv-filename := ".env.local"
set shell := ["bash", "-euo", "pipefail", "-c"]

# `just` alone = menu, not a surprise build
default:
    @just --list

# ── AWS bootstrap (S3 state bucket — once per account) ──────────────

aws-bootstrap:
    tofu -chdir=infra/bootstrap init
    tofu -chdir=infra/bootstrap apply

# ── EKS ─────────────────────────────────────────────────────────────

# tofu's own "No outputs found" sounds like a config problem, not
# a "run apply first" problem. Wrap with a useful error.

[private]
_eks-out NAME:
    @tofu -chdir=infra/eks output -raw {{NAME}} 2>/dev/null || \
      { echo "error: tofu output '{{NAME}}' missing — run 'just eks-apply' first?" >&2; exit 1; }

# -reconfigure: tofu can't tell the dynamic -backend-config is the
# same as last time, prompts "migrate?" even though nothing changed.

# Init infra/eks backend (reads bucket from infra/bootstrap output)
eks-init:
    tofu -chdir=infra/eks init -reconfigure \
      -backend-config="bucket=$(tofu -chdir=infra/bootstrap output -raw bucket)"

eks-apply: eks-init
    tofu -chdir=infra/eks apply

eks-apply-auto: eks-init
    tofu -chdir=infra/eks apply -auto-approve

eks-kubeconfig:
    $(just _eks-out kubeconfig_command)

eks-push:
    ./infra/eks/push-images.sh

eks-deploy:
    ./infra/eks/deploy.sh

eks-smoke:
    ./infra/eks/smoke-test.sh

[private]
_eks-up APPLY:
    just {{APPLY}}
    just eks-kubeconfig
    just eks-push
    just eks-deploy

# Full bring-up: apply (prompts) → kubeconfig → push → deploy
eks-up: (_eks-up "eks-apply")

# Full bring-up, auto-approves apply
eks-up-auto: (_eks-up "eks-apply-auto")

# WorkerPool finalizers hold pods → NLB → tofu destroy blocks.
eks-destroy:
    kubectl -n rio-system delete workerpool --all --wait=true --ignore-not-found
    tofu -chdir=infra/eks destroy
