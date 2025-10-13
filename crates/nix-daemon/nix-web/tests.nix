# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>

{ pkgs ? import ../nix/nixpkgs.default.nix
, lib ? pkgs.lib
, nix-web ? pkgs.callPackage ./. {}
}:

let
  isPackage = lib.types.package.check;
  nixPackages = lib.filterAttrs (_: isPackage) (pkgs.callPackage ../nix/nix-packages.nix {});

  testDrv = derivation {
    name = "test";
    builder = "/bin/sh";
    args = [ "-c" ": > $out" ];
    inherit (pkgs.stdenv.hostPlatform) system;
  };

  mkNixTest = attr: nix: pkgs.testers.nixosTest ({ lib, ... }: {
    name = "nix-web-test-${attr}";

    nodes.machine = {
      system.extraDependencies = [
        testDrv.drvPath
      ];

      systemd.sockets.nix-web = {
        wantedBy = [ "sockets.target" ];
        socketConfig.ListenStream = "[::1]:1234";
      };

      systemd.packages = [ nix-web ];
      systemd.services.nix-web.path = [ nix ];

      nix.package = nix;
    };

    testScript = { nodes, ... }: ''
        machine.wait_for_unit("sockets.target")
        machine.succeed("curl --fail-with-body http://localhost:1234/ >&2")
        machine.succeed("curl --fail-with-body http://localhost:1234${testDrv.drvPath} >&2")
    '';
  });
in
lib.mapAttrs mkNixTest nixPackages
