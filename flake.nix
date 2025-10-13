# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>

{
  outputs =
    { self, nixpkgs }:
    nixpkgs.lib.foldAttrs nixpkgs.lib.mergeAttrs {} (map (system:
      let
        pkgs = import nixpkgs {
          inherit system;
          config.allowAliases = false;
        };

        filteredSrc = import nix/src.nix {
          inherit (pkgs) lib;
          src = self;
        };
      in {
        packages.${system} = {
          nix-web = import ./nix-web {
            inherit pkgs;
            srcOverride = filteredSrc;
          };

          nix-supervisor = import ./nix-supervisor {
            inherit pkgs;
            srcOverride = filteredSrc;
          };
        };

        checks.${system} = {
          nix-web = import nix-web/tests.nix {
            inherit pkgs;
            inherit (self.packages.${system}) nix-web;
          };

          nix-supervisor = import nix-supervisor/tests.nix {
            inherit pkgs;
            inherit (self.packages.${system}) nix-supervisor;
          };

          reuse = import nix/reuse.nix {
            inherit pkgs;
            src = self;
          };
          no-refs = import nix/no-refs.nix {
            inherit pkgs;
            src = self;
          };
        };
      }
    ) [
      "aarch64-linux"
      "armv7l-linux"
      "i686-linux"
      "powerpc64le-linux"
      "riscv64-linux"
      "x86_64-linux"
    ]);
}
