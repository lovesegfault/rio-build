# Package awslabs/amazon-eks-ami/nodeadm — the AL2023 EKS bootstrap.
#
# nodeadm reads the Karpenter-generated NodeConfig (MIME-multipart
# userData via IMDSv2), writes kubelet config + bootstrap kubeconfig +
# CA, then exits. It does NOT manage kubelet/containerd lifecycle —
# those are NixOS systemd units (eks-node.nix).
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

  # nodeadm is a sub-module of the amazon-eks-ami repo. It vendors its
  # Go deps (vendor/ checked in upstream), so vendorHash = null.
  modRoot = "nodeadm";
  vendorHash = null;

  subPackages = [ "cmd/nodeadm" ];

  # One NixOS-side adjustment to the embedded templates:
  #
  # • kubeconfig: upstream hard-codes `command: aws` (`eks get-token`).
  #   awscli2 is ~500 MB Python with ~1-2 s cold start, paid on every
  #   kubelet credential refresh. aws-iam-authenticator is a ~20 MB Go
  #   static binary, sub-100 ms — and is what the AL2 path used before
  #   nodeadm. The token format is identical (STS presigned URL).
  #
  # (containerd templates are unpatched: nodeadm runs with `--daemon
  # kubelet` so its containerd config path never executes — see
  # eks-node.nix.)
  postPatch = ''
    cat > nodeadm/internal/kubelet/kubeconfig.template.yaml <<'EOF'
    ---
    apiVersion: v1
    kind: Config
    clusters:
      - name: kubernetes
        cluster:
          certificate-authority: {{.CaCertPath}}
          server: {{.APIServerEndpoint}}
    current-context: kubelet
    contexts:
      - name: kubelet
        context:
          cluster: kubernetes
          user: kubelet
    users:
      - name: kubelet
        user:
          exec:
            apiVersion: client.authentication.k8s.io/v1beta1
            command: aws-iam-authenticator
            args:
              - "token"
              - "-i"
              - "{{.Cluster}}"
              - "--region"
              - "{{.Region}}"
    EOF
  '';

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
