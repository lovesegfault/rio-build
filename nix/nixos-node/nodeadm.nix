# Package awslabs/amazon-eks-ami/nodeadm — the AL2023 EKS bootstrap.
#
# nodeadm reads the Karpenter-generated NodeConfig (MIME-multipart
# userData via IMDSv2), writes kubelet config + bootstrap kubeconfig +
# CA + containerd drop-in, then exits. It does NOT manage kubelet/
# containerd lifecycle — those are NixOS systemd units (eks-node.nix).
#
# Pinned to the release tag matching pins.nix kubernetes_version so the
# KubeletConfiguration schema nodeadm emits matches the control plane.
# vendorHash: run with lib.fakeHash, copy the "got:" line.
{
  lib,
  buildGoModule,
  fetchFromGitHub,
  pins,
}:
buildGoModule rec {
  pname = "nodeadm";
  version = pins.nodeadm_rev;

  src = fetchFromGitHub {
    owner = "awslabs";
    repo = "amazon-eks-ami";
    rev = version;
    hash = pins.nodeadm_src_hash;
  };

  # nodeadm is a sub-module of the amazon-eks-ami repo.
  modRoot = "nodeadm";
  vendorHash = pins.nodeadm_vendor_hash;

  subPackages = [ "cmd/nodeadm" ];

  # Strip + embed version (mirrors upstream Makefile ldflags).
  ldflags = [
    "-s"
    "-w"
    "-X github.com/awslabs/amazon-eks-ami/nodeadm/internal/cli.version=${version}"
  ];

  # Upstream tests shell out to `imds` / assume AL2023 paths. The binary
  # is exercised end-to-end by the P1 spike (real instance) and P2's
  # nixos-node VM test with a mocked IMDS.
  doCheck = false;

  meta = {
    description = "EKS node bootstrap (AL2023 nodeadm) — packaged for the rio NixOS node AMI";
    homepage = "https://github.com/awslabs/amazon-eks-ami/tree/main/nodeadm";
    license = lib.licenses.mit0;
    mainProgram = "nodeadm";
  };
}
