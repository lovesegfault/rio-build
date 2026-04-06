# kubelet image-credential-provider for ECR (kubernetes/cloud-provider-aws).
#
# nodeadm hard-fails `init` if this binary isn't stat()able at
# /etc/eks/image-credential-provider/ecr-credential-provider (or wherever
# ECR_CREDENTIAL_PROVIDER_BIN_PATH points). On AL2023 install-worker.sh
# drops it from an S3 bucket; here we build from source. The kubelet later
# exec()s it (basename only — kubelet searches --image-credential-provider-
# bin-dir) to mint short-lived ECR pull tokens for vpc-cni / kube-proxy /
# any *.dkr.ecr.* image, so the node IAM role doesn't need a static
# imagePullSecret.
{
  lib,
  buildGoModule,
  fetchFromGitHub,
  pins,
}:
buildGoModule rec {
  pname = "ecr-credential-provider";
  version = pins.ecr_credential_provider_rev;

  src = fetchFromGitHub {
    owner = "kubernetes";
    repo = "cloud-provider-aws";
    rev = version;
    hash = pins.ecr_credential_provider_src_hash;
  };

  vendorHash = pins.ecr_credential_provider_vendor_hash;

  subPackages = [ "cmd/ecr-credential-provider" ];

  ldflags = [
    "-s"
    "-w"
    "-X k8s.io/component-base/version.gitVersion=${version}"
  ];

  # Upstream tests assume an EC2 environment / IMDS.
  doCheck = false;

  meta = {
    description = "kubelet credential-provider plugin for Amazon ECR";
    homepage = "https://github.com/kubernetes/cloud-provider-aws";
    license = lib.licenses.asl20;
    mainProgram = "ecr-credential-provider";
  };
}
