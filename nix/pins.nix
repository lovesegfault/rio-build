# Single source of truth for version pins that must agree between
# nix/tests/ (VM tests) and infra/eks/ (reference deploy). Drift =
# deploying an untested combination.
#
# Consumed by:
#   - nix/tfvars.nix  → infra/eks/generated.auto.tfvars.json
#
# Bump policy: change here, regenerate with `nix build .#tfvars -o
# infra/eks/generated.auto.tfvars.json`, commit both. The tfvars-fresh
# check fails CI if the committed file drifts from what pins.nix says.
{
  # EKS control-plane version. 1.33+ required (ADR-012: hostUsers: false).
  kubernetesVersion = "1.33";

  # cert-manager: chart version == app version since v1.0. Keep the
  # `v` prefix — jetstack's chart repo publishes as `vX.Y.Z`.
  certManagerVersion = "v1.20.0";

  # aws-load-balancer-controller: v3.0+ aligns chart with app version.
  awsLbcVersion = "3.1.0";

  # Karpenter chart (AWS provider, OCI-published). Must stay within
  # the EKS module's karpenter submodule compat range (~> 21.0).
  karpenterVersion = "1.10.0";
}
