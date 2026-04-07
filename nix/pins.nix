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
  # k8s-openapi (Cargo.toml) tracks this via the v1_NN feature.
  kubernetes_version = "1.35";

  # cert-manager: chart version == app version since v1.0. Keep the
  # `v` prefix — jetstack's chart repo publishes as `vX.Y.Z`.
  cert_manager_version = "v1.20.0";

  # aws-load-balancer-controller: v3.0+ aligns chart with app version.
  aws_lbc_version = "3.1.0";

  # Karpenter chart (AWS provider, OCI-published). Must stay within
  # the EKS module's karpenter submodule compat range (~> 21.0).
  karpenter_version = "1.10.0";

  # NixOS node AMI kernel minor (ADR-021). String form ("6_18") so
  # minimal.nix can do `pkgs."linuxPackages_${node_kernel_minor}"`.
  # Pinned (not linuxPackages_latest) so a nixpkgs flake-input bump
  # can't surprise-rebuild the ~40min kernel derivation.
  node_kernel_minor = "6_18";

  # awslabs/amazon-eks-ami release tag for the packaged `nodeadm`
  # (nix/nixos-node/nodeadm.nix). Track kubernetes_version's minor —
  # nodeadm emits a KubeletConfiguration matching the control plane.
  # Hashes: build once with lib.fakeHash, copy "got:" lines.
  nodeadm_rev = "v20260318";
  nodeadm_src_hash = "sha256-lrkifYFc9XXBienp15gZ2gJkeFqcJH21cGl7SWyj+Qw=";

  # kubernetes/cloud-provider-aws → ecr-credential-provider binary.
  # nodeadm REQUIRES this on disk before it will finish kubelet config
  # (stat()s the path; no skip flag). Tracks kubernetes_version's minor.
  ecr_credential_provider_rev = "v1.35.1";
  ecr_credential_provider_src_hash = "sha256-kCDhkwcxYNDAmYrrk+dnHkVG2Qzcw8USPcaxHKZwxzs=";
  ecr_credential_provider_vendor_hash = "sha256-eW9vsuhDaudnq34onV5LH1hY9S7Zt2jkzhL5UhbUlHY=";

  # security-profiles-operator. NOT a tofu-managed helm release: SPO
  # stopped publishing chart tarballs after v0.7.1 (only the in-repo
  # deploy/helm/ exists). The static deploy/operator.yaml is vendored
  # at infra/k8s/security-profiles-operator.yaml; bump = re-download
  # from this tag. Requires cert-manager (above).
  spo_version = "v0.10.0";
}
