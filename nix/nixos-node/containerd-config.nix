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
      # r[impl sec.pod.fuse-device-plugin]
      # runc mknods these inside the container's /dev (container-namespace
      # uid/gid) — NOT a hostPath mount, so no hostUsers:false idmap-mount
      # rejection. Every pod gets both; mounting fuse still needs
      # CAP_SYS_ADMIN, and /dev/kvm ENXIOs on non-.metal. Replaces the
      # smarter-device-manager device plugin: the NodeOverlay-declared
      # smarter-devices/{fuse,kvm} capacity stays as a scheduling signal
      # only — kubelet leaves extended resources it never saw via a plugin
      # alone (k/k#64784).
      linux = {
        devices = [
          {
            path = "/dev/fuse";
            type = "c";
            major = 10;
            minor = 229;
            fileMode = 438; # 0666
            uid = 0;
            gid = 0;
          }
          {
            path = "/dev/kvm";
            type = "c";
            major = 10;
            minor = 232;
            fileMode = 432; # 0660
            uid = 0;
            gid = 0;
          }
        ];
        # CRI appends its default device-cgroup rules (deny-all + the
        # /dev/{null,zero,random,tty,…} allows) AFTER loading this spec
        # (oci.WithSpecFromFile → CRI spec opts), so this list is
        # additive, not the full allowlist. Live check on a booted node:
        #   crictl inspect <cid> | jq '.info.runtimeSpec.linux.resources.devices | length'
        # — must be >2. If a containerd bump changes merge semantics,
        # that's where it surfaces.
        resources.devices = [
          {
            allow = true;
            type = "c";
            major = 10;
            minor = 229;
            access = "rwm";
          }
          {
            allow = true;
            type = "c";
            major = 10;
            minor = 232;
            access = "rwm";
          }
        ];
      };
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
