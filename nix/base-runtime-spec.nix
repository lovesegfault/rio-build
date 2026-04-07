# OCI base runtime spec — /dev/{fuse,kvm} injection via containerd.
#
# Shared by both delivery paths:
#   - EKS (NixOS AMI): nix/nixos-node/containerd-config.nix sets
#     `base_runtime_spec` on the runc runtime directly.
#   - k3s VM tests: nix/tests/fixtures/k3s-full.nix renders a
#     full `config.toml.tmpl` (services.k3s.containerdConfigTemplate)
#     for k3s's embedded containerd with `base_runtime_spec` set on
#     the runc runtime.
#
# r[impl sec.pod.fuse-device-plugin]
# runc mknods these inside the container's /dev (container-namespace
# uid/gid) — NOT a hostPath mount, so no hostUsers:false idmap-mount
# rejection. Every pod gets /dev/fuse; mounting fuse still needs
# CAP_SYS_ADMIN. /dev/kvm is included only when withKvm=true — both
# delivery paths build BOTH variants and pick at boot via `test -c
# /dev/kvm` → `ln -sfn … /run/base-runtime-spec.json`, so on
# non-.metal the node never appears in the container at all (instead
# of mknod-ing a dead 10:232 that fools `test -c /dev/kvm` probes
# then ENXIOs on open). kvm pods route to .metal via the
# `rio.build/kvm` nodeSelector (controller-derived from
# features:[kvm], r[ctrl.builderpool.kvm-device]).
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
{
  pkgs,
  withKvm ? false,
}:
let
  fuseDev = {
    path = "/dev/fuse";
    type = "c";
    major = 10;
    minor = 229;
    fileMode = 438; # 0666
    uid = 0;
    gid = 0;
  };
  kvmDev = {
    path = "/dev/kvm";
    type = "c";
    major = 10;
    minor = 232;
    fileMode = 432; # 0660
    uid = 0;
    gid = 0;
  };
  cgroupAllow = d: {
    allow = true;
    inherit (d) type major minor;
    access = "rwm";
  };
  devs = [ fuseDev ] ++ pkgs.lib.optional withKvm kvmDev;
  rioAdditions = builtins.toJSON {
    process.rlimits = [
      {
        type = "RLIMIT_NOFILE";
        hard = 1048576;
        soft = 65536;
      }
    ];
    linux = {
      devices = devs;
      # This is NOT the full cgroup allowlist. With base_runtime_spec
      # set, CRI does NOT append default devices — `crictl inspect`
      # shows exactly these entries in BOTH linux.devices and
      # linux.resources.devices (verified by the base-runtime-spec-
      # passthrough subtest in nix/tests/scenarios/security.nix,
      # vm-security-nonpriv-k3s). runc's libcontainer (specconv
      # AllowedDevices + CreateCgroupConfig) appends /dev/{null,zero,
      # full,tty,urandom,random} nodes AND the deny-all + standard
      # cgroup allows when converting OCI spec → libcontainer config
      # — below crictl-inspect visibility. The CI subtest gates that
      # our entries SURVIVE to the OCI spec; runc's append is proven
      # functionally by build-completes (would fail without /dev/null).
      resources.devices = map cgroupAllow devs;
    };
  };
in
# `ctr oci spec` emits containerd's compiled-in default (the same spec
# CRI would build with no base_runtime_spec set). jq `*` is recursive
# object merge: rioAdditions wins on leaf collisions, arrays REPLACE
# (not append) — fine here since the default spec has no linux.devices
# and process.rlimits we want to override anyway.
pkgs.runCommand "base-runtime-spec${pkgs.lib.optionalString withKvm "-kvm"}.json"
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
    # load-bearing defaults runc needs. kvm presence is asserted
    # to MATCH withKvm (present iff true).
    jq -e '
      .process.cwd != null and .process.cwd != ""
      and (.linux.namespaces | length) > 0
      and (.linux.devices | map(.path) | contains(["/dev/fuse"]))
      and ((.linux.devices | map(.path) | contains(["/dev/kvm"])) == ${pkgs.lib.boolToString withKvm})
      and (.process.rlimits[0].type == "RLIMIT_NOFILE")
    ' $out >/dev/null
  ''
