# Single-generation UKI at the ESP fallback path — UEFI firmware boots
# it directly, no loader. Node is immutable+ephemeral so there's exactly
# one generation ever; GRUB's menu/os-prober/timeout are dead weight.
#
# `config.system.build.uki` is referenced from the install hook, which
# is itself a build input of toplevel (switchable-system.nix wrapProgram
# bakes INSTALL_BOOTLOADER into bin/switch-to-configuration). Upstream
# uki.nix's default Cmdline interpolates `init=${toplevel}` — that's a
# derivation cycle. Break it by pointing init at the profile symlink
# nixos-install creates; systemd-initrd's initrd-nixos-activation
# resolves it via `resolve-in-root /sysroot` before switch-root.
#
# Side effect: ukify (`pkgs.buildPackages.systemdUkify`, +170 MiB of
# python+pefile+binutils) stays a BUILD dep of the uki derivation; the
# .efi blob it emits has no store-path references, so the install hook
# pulls only the blob (and coreutils for cp) into toplevel's closure.
{
  config,
  lib,
  pkgs,
  ...
}:
let
  inherit (pkgs.stdenv.hostPlatform) efiArch;
  # amazon-image.nix injects vga=/nomodeset for the GRUB-era serial
  # console; both are noise on Nitro headless. They merge into
  # boot.kernelParams unconditionally, so filter here rather than
  # fight list-option priorities.
  cmdline = lib.concatStringsSep " " (
    [ "init=/nix/var/nix/profiles/system/init" ]
    ++ lib.filter (p: p != "nomodeset" && !(lib.hasPrefix "vga=" p)) config.boot.kernelParams
  );
in
{
  boot = {
    loader = {
      grub.enable = lib.mkForce false;
      systemd-boot.enable = lib.mkForce false;
      efi.canTouchEfiVariables = false;
      external = {
        enable = true;
        installHook = pkgs.writeShellScript "install-uki" ''
          set -eu
          esp=/boot
          mkdir -p "$esp/EFI/BOOT"
          cp ${config.system.build.uki}/${config.system.boot.loader.ukiFile} \
            "$esp/EFI/BOOT/BOOT${lib.toUpper efiArch}.EFI"
        '';
      };
    };
    uki.settings.UKI.Cmdline = cmdline;
  };
}
