# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>

{ pkgs ? import ./nixpkgs.default.nix
, src ? builtins.fetchGit ../.
}:

pkgs.callPackage ({ runCommand, reuse }: runCommand "gorgon-reuse" {
  inherit src;
  nativeBuildInputs = [ reuse ];
} ''
  reuse --root $src lint
  touch $out
''
) {}
