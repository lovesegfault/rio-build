# Version pins that must agree between nix/tests/ (VM tests) and
# infra/eks/ (reference deploy). Drift = deploying an untested combination.
#
# Keys are snake_case (Terraform convention, not Nix) so
# `builtins.toJSON` in flake.nix passes straight through to
# generated.auto.tfvars.json — no mapping layer to forget to update.
# Add a key here → it lands in the tfvars automatically.
#
# Bump: edit here, then
#   nix build .#tfvars && jq -S . result > infra/eks/generated.auto.tfvars.json
# and commit both. checks.tfvars-fresh fails CI on drift.
{
  # EKS control-plane version. 1.33+ required (ADR-012: hostUsers: false).
  kubernetes_version = "1.33";

  # cert-manager: chart version == app version since v1.0. Keep the
  # `v` prefix — jetstack's chart repo publishes as `vX.Y.Z`.
  cert_manager_version = "v1.20.0";

  # aws-load-balancer-controller: v3.0+ aligns chart with app version.
  aws_lbc_version = "3.1.0";

  # Karpenter chart (AWS provider, OCI-published). Must stay within
  # the EKS module's karpenter submodule compat range (~> 21.0).
  karpenter_version = "1.10.0";

  # security-profiles-operator. NOT a tofu-managed helm release: SPO
  # stopped publishing chart tarballs after v0.7.1 (only the in-repo
  # deploy/helm/ exists). The static deploy/operator.yaml is vendored
  # at infra/k8s/security-profiles-operator.yaml; bump = re-download
  # from this tag. Requires cert-manager (above).
  spo_version = "v0.10.0";
}
