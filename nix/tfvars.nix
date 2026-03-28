# Renders nix/pins.nix as Terraform *.auto.tfvars.json so version pins
# live in ONE place and flow to the EKS reference deploy. Terraform
# auto-loads *.auto.tfvars.json — no -var-file flag needed.
#
#   nix build .#tfvars -o infra/eks/generated.auto.tfvars.json
#   cd infra/eks && tofu plan
#
# What goes here vs. terraform.tfvars:
#   HERE:             versions that nix/tests/ and infra/eks/ must agree on
#   terraform.tfvars: operator knobs (region, cluster_name, instance types)
#
# Image tags do NOT go here — Terraform doesn't install the rio chart.
# xtask eks deploy does, reading .rio-image-tag (xtask/src/k8s/eks/).
{ writeText, pins }:
writeText "generated.auto.tfvars.json" (
  builtins.toJSON {
    kubernetes_version = pins.kubernetesVersion;
    cert_manager_version = pins.certManagerVersion;
    aws_lbc_version = pins.awsLbcVersion;
    karpenter_version = pins.karpenterVersion;
  }
)
