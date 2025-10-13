# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>

{ lib ? (import <nixpkgs> {}).lib
, src ? if builtins.pathExists ../.git
        then builtins.fetchGit { url = ../.; submodules = true; }
        else ../.
}:

with lib;

cleanSourceWith {
  filter = path: type: (type == "regular" -> (
    (!hasSuffix ".nix" path) && (!((hasPrefix "." path) && (hasSuffix ".yaml" path)))
  ));
  src = cleanSource src;
}
