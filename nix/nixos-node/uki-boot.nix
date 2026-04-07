# Single-generation UKI at the ESP fallback path — UEFI firmware boots
# it directly, no loader. Node is immutable+ephemeral so there's exactly
# one generation ever; GRUB's menu/os-prober/timeout are dead weight.
#
# The UKI is assembled at INSTALL time from bootspec ($1/boot.json),
# not as `config.system.build.uki`: the upstream uki derivation bakes
# `init=${toplevel}` into its cmdline, and `installBootLoader` is itself
# a build input of toplevel (top-level.nix systemBuilderArgs +
# switchable-system.nix wrapProgram), so referencing the uki drv from
# the install hook is a derivation cycle. Bootspec exists exactly to
# break that — it's emitted by the toplevel builder with the final
# store path injected via `placeholder "out"`.
{ lib, pkgs, ... }:
let
  inherit (pkgs.stdenv.hostPlatform) efiArch;
in
{
  boot.loader = {
    grub.enable = lib.mkForce false;
    systemd-boot.enable = lib.mkForce false;
    efi.canTouchEfiVariables = false;
    external = {
      enable = true;
      installHook = pkgs.writeShellScript "install-uki" ''
        set -eu
        toplevel=$1
        bootspec=$toplevel/boot.json
        esp=/boot
        mkdir -p "$esp/EFI/BOOT"
        read -r kernel initrd init < <(${pkgs.jq}/bin/jq -r \
          '."org.nixos.bootspec.v1" | "\(.kernel) \(.initrd) \(.init)"' \
          "$bootspec")
        params=$(${pkgs.jq}/bin/jq -r \
          '."org.nixos.bootspec.v1".kernelParams | join(" ")' "$bootspec")
        ${pkgs.systemdUkify}/lib/systemd/ukify build \
          --linux="$kernel" \
          --initrd="$initrd" \
          --cmdline="init=$init $params" \
          --os-release="@$toplevel/etc/os-release" \
          --stub=${pkgs.systemdUkify}/lib/systemd/boot/efi/linux${efiArch}.efi.stub \
          --efi-arch=${efiArch} \
          --output="$esp/EFI/BOOT/BOOT${lib.toUpper efiArch}.EFI"
      '';
    };
  };
}
