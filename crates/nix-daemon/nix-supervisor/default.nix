# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>

{ pkgs ? import ../nix/nixpkgs.default.nix
, lib ? pkgs.lib
, nix ? pkgs.nix
, pkg-config ? pkgs.pkg-config
, openssl ? pkgs.openssl
, srcOverride ? import ../nix/src.nix { inherit lib; }
, buildType ? "release"
}:
let
  cargoPackageFlags = [ "-p" "nix-supervisor" ];
in
pkgs.rustPlatform.buildRustPackage {
  pname = "nix-supervisor";
  inherit ((lib.importTOML ./Cargo.toml).package) version;
  inherit buildType;

  src = srcOverride;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkg-config ];
  buildInputs = [ openssl ];

  postPatch = ''
    substituteInPlace nix-supervisor/nix-supervisor.service \
      --replace 'ExecStart=nix-supervisor' "ExecStart=$out/bin/nix-supervisor"
  '';
  postInstall = ''
    install -m 644 -D nix-supervisor/nix-supervisor.service \
      $out/lib/systemd/system/nix-supervisor.service
  '';

  cargoBuildFlags = cargoPackageFlags;
  cargoTestFlags = cargoPackageFlags;
}
