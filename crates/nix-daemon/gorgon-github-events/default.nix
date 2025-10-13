# SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>
# SPDX-License-Identifier: MIT-0

{ pkgs ? import ../nix/nixpkgs.default.nix
, lib ? pkgs.lib
, srcOverride ? import ../nix/src.nix { inherit lib; }
}:

pkgs.rustPlatform.buildRustPackage {
  pname = "gorgon-github-events";
  inherit ((lib.importTOML ./Cargo.toml).package) version;

  src = srcOverride;
  cargoLock.lockFile = ../Cargo.lock;

  nativeBuildInputs = [ pkgs.buildPackages.pkg-config ];
  buildInputs = [ pkgs.openssl ];

  cargoBuildFlags = [ "-p" "gorgon-github-events" ];
  cargoTestFlags = [ "-p" "gorgon-github-events" ];

  postInstall = ''
    install -m 644 -D gorgon-github-events/gorgon-github-events.1 $out/share/man/man1/gorgon-github-events.1
    install -m 644 -D gorgon-github-events/gorgon-github-events@.service $out/lib/systemd/system/gorgon-github-events@.service
    install -m 644 -D gorgon-github-events/gorgon-github-events@.timer $out/lib/systemd/system/gorgon-github-events@.timer
  '';
}
