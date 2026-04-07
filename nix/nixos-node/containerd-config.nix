# Static containerd config — replaces nodeadm's runtime-generated
# /etc/containerd/config.toml. Every template variable nodeadm fills is a
# build-time constant for us (we control the AMI), so generating this at
# boot serializes containerd start behind an IMDS round-trip for nothing.
#
# Upstream template (v3 schema): awslabs/amazon-eks-ami
#   nodeadm/internal/containerd/config2.template.toml
{
  lib,
  pkgs,
  pauseRef,
}:
let
  baseRuntimeSpec = pkgs.writeText "base-runtime-spec.json" (
    builtins.toJSON {
      ociVersion = "1.1.0";
      process.rlimits = [
        {
          type = "RLIMIT_NOFILE";
          hard = 1048576;
          soft = 65536;
        }
      ];
    }
  );
in
pkgs.writeText "containerd-config.toml" ''
  version = 3
  root = "/var/lib/containerd"
  state = "/run/containerd"

  [grpc]
  address = "/run/containerd/containerd.sock"

  [plugins.'io.containerd.cri.v1.images']
  discard_unpacked_layers = true

  [plugins.'io.containerd.cri.v1.images'.pinned_images]
  sandbox = "${pauseRef}"

  [plugins.'io.containerd.cri.v1.runtime'.containerd]
  default_runtime_name = "runc"

  [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc]
  runtime_type = "io.containerd.runc.v2"
  base_runtime_spec = "${baseRuntimeSpec}"
  # ADR-012 §3 — hostUsers:false unblock. Was the only reason for the
  # config.d/10-rio.toml drop-in; folded in now that we own the file.
  cgroup_writable = true

  [plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.runc.options]
  SystemdCgroup = true
  BinaryName = "${lib.getExe pkgs.runc}"

  [plugins.'io.containerd.cri.v1.runtime'.cni]
  bin_dir = "/opt/cni/bin"
  conf_dir = "/etc/cni/net.d"
''
