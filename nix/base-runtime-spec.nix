# OCI base runtime spec — /dev/{fuse,kvm} injection via containerd.
#
# Shared by both delivery paths:
#   - EKS (NixOS AMI): nix/nixos-node/containerd-config.nix sets
#     `base_runtime_spec` on the runc runtime directly.
#   - k3s/kind VM tests: nix/tests/fixtures/k3s-full.nix renders a
#     full `config.toml.tmpl` (services.k3s.containerdConfigTemplate)
#     for k3s's embedded containerd with `base_runtime_spec` set on
#     the runc runtime.
#
# r[impl sec.pod.fuse-device-plugin]
# runc mknods these inside the container's /dev (container-namespace
# uid/gid) — NOT a hostPath mount, so no hostUsers:false idmap-mount
# rejection. Every pod gets both; mounting fuse still needs
# CAP_SYS_ADMIN, and /dev/kvm ENXIOs on non-.metal. The NodeOverlay-
# declared `rio.build/{fuse,kvm}` capacity (EKS) / kubectl-patched
# node status (k3s) is scheduling-signal only — kubelet leaves
# extended resources it never saw via a plugin alone (k/k#64784).
#
# `base_runtime_spec` is the STARTING spec — CRI's spec opts layer on
# top (`oci.WithSpecFromFile` then `WithProcessCwd`/`WithNamespaces`/…)
# but ONLY for fields the container/image config sets. A spec missing
# `process.cwd` or `linux.namespaces` fails `runc create` for containers
# whose image lacks WorkingDir ("Cwd property must not be empty") or
# that set readOnlyRootFilesystem ("unable to restrict sys entries
# without a private MNT namespace"). Hence: start from `ctr oci spec`
# (containerd's full default) and merge our additions, instead of
# hand-writing a minimal JSON.
{ pkgs }:
let
  rioAdditions = builtins.toJSON {
    process.rlimits = [
      {
        type = "RLIMIT_NOFILE";
        hard = 1048576;
        soft = 65536;
      }
    ];
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
  };
in
# `ctr oci spec` emits containerd's compiled-in default (the same spec
# CRI would build with no base_runtime_spec set). jq `*` is recursive
# object merge: rioAdditions wins on leaf collisions, arrays REPLACE
# (not append) — fine here since the default spec has no linux.devices
# and process.rlimits we want to override anyway.
pkgs.runCommand "base-runtime-spec.json"
  {
    nativeBuildInputs = [
      pkgs.containerd
      pkgs.jq
    ];
    inherit rioAdditions;
    passAsFile = [ "rioAdditions" ];
  }
  ''
    ctr oci spec | jq -S --slurpfile add "$rioAdditionsPath" '. * $add[0]' > $out
    # Sanity: the merge produced what we expect AND kept the
    # load-bearing defaults runc needs.
    jq -e '
      .process.cwd != null and .process.cwd != ""
      and (.linux.namespaces | length) > 0
      and (.linux.devices | map(.path) | contains(["/dev/fuse","/dev/kvm"]))
      and (.process.rlimits[0].type == "RLIMIT_NOFILE")
    ' $out >/dev/null
  ''
