# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>

{ pkgs ? import ../nix/nixpkgs.default.nix
, lib ? pkgs.lib
, nixVersions ? pkgs.callPackage ../nix/nix-packages.nix {}
, nixVersion ? "nix_2_24"
, nix ? nixVersions."${nixVersion}"
, pkg-config ? pkgs.pkg-config
, openssl ? pkgs.openssl
, srcOverride ? import ../nix/src.nix { inherit lib; }
, buildType ? "release"
}:
let
  cargoPackageFlags = [ "-p" "nix-web" ];
in
pkgs.rustPlatform.buildRustPackage {
  pname = "nix-web";
  inherit ((lib.importTOML ./Cargo.toml).package) version;
  inherit buildType;

  src = srcOverride;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  postPatch = ''
    substituteInPlace nix-web/nix-web.service \
      --replace 'ExecStart=nix-web' "ExecStart=$out/bin/nix-web"
  '';
  postInstall = ''
    install -m 644 -D nix-web/nix-web.service $out/lib/systemd/system/nix-web.service
  '';

  cargoBuildFlags = cargoPackageFlags;
  cargoTestFlags = cargoPackageFlags;

  NIX_WEB_BUILD_NIX_CLI_PATH = "nix";
}
