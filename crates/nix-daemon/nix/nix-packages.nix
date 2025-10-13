# SPDX-FileCopyrightText: 2025 embr <git@liclac.eu>
#
# SPDX-License-Identifier: EUPL-1.2

{ pkgs ? import ./nixpkgs.default.nix
, lib ? pkgs.lib
, nixVersions ? pkgs.nixVersions
, lixVersions ? pkgs.lixVersions
}:
let
  toVersion = lib.replaceStrings ["_"] ["."];

  # Don't access Nix versions that have been removed from nixpkgs.
  isNixSupported = attr: _: lib.versionAtLeast (toVersion (lib.removePrefix "nix_" attr)) "2.24";
  isLixSupported = attr: _: lib.versionAtLeast (toVersion (lib.removePrefix "lix_" attr)) "2.90";
in
lib.filterAttrs isNixSupported nixVersions //
lib.filterAttrs isLixSupported lixVersions
