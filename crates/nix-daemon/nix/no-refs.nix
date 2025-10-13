# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>

{ pkgs ? import ./nixpkgs.default.nix
, src ? builtins.fetchGit ../.
}:

pkgs.callPackage ({ runCommand, ripgrep }: runCommand "gorgon-no-refs" {
  inherit src;
  nativeBuildInputs = [ ripgrep ];
} ''
  rg --pcre2 '(?!ffffffffffffffffffffffffffffffff)[0-9abcdfghijklmnpqrsvwxyz]{32}-[0-9a-zA-Z+._?=-]+'  $src && exit 1
  touch $out
''
) {}
