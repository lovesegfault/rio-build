# smarter-device-manager packaged from source.
#
# Upstream ships no go.mod (Dockerfile does `go mod init && go mod tidy`
# at build time, network-dependent). The checked-in go.mod/go.sum here
# pin k8s.io/kubelet@v0.28 + grpc@v1.58 — the last revisions before the
# DevicesIDs→DevicesIds protoc rename and the mustEmbedUnimplemented…
# forward-compat requirement, both of which break v1.20.12 against
# unpinned `go mod tidy`.
#
# Runs as a host systemd unit (eks-node.nix), NOT a static pod: no
# registry round-trip, no kubelet-manages-its-own-dependency loop. The
# binary registers on /var/lib/kubelet/device-plugins/kubelet.sock and
# advertises smarter-devices/{fuse,kvm} from /root/config/conf.yaml.
{
  lib,
  buildGoModule,
  fetchFromGitHub,
  pins,
}:
buildGoModule {
  pname = "smarter-device-manager";
  version = pins.smarter_device_manager_version;

  src = fetchFromGitHub {
    owner = "smarter-project";
    repo = "smarter-device-manager";
    rev = "v${pins.smarter_device_manager_version}";
    hash = pins.smarter_device_manager_src_hash;
  };

  # Upstream vendor/ is GOPATH-era (no modules.txt) and stale wrt the
  # source's import paths. Replace with our pinned go.mod/go.sum.
  postPatch = ''
    rm -rf vendor
    cp ${./go.mod} go.mod
    cp ${./go.sum} go.sum
  '';

  vendorHash = pins.smarter_device_manager_vendor_hash;

  env.CGO_ENABLED = 0;
  ldflags = [
    "-s"
    "-w"
  ];

  meta = {
    description = "Kubernetes device plugin exposing /dev/* nodes as extended resources";
    homepage = "https://github.com/smarter-project/smarter-device-manager";
    license = lib.licenses.asl20;
    mainProgram = "smarter-device-management";
  };
}
