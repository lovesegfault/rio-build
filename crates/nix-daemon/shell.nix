# SPDX-FileCopyrightText: 2023 embr <git@liclac.eu>
#
# SPDX-License-Identifier: EUPL-1.2

{ pkgs ? import ./nix/nixpkgs.default.nix }:
pkgs.mkShell {
  buildInputs = with pkgs; [
    direnv

    # rust toolchain
    cargo
    rustc
    rustfmt
    clippy

    # build dependencies
    stdenv.cc
    pkg-config
    openssl
    libgit2
    cacert

    # script dependencies
    bashInteractive
    fish

    # other tools
    jq
    cargo-expand

    # repo tools
    reuse # check license compliance
    commitlint # check commit messages
    woodpecker-cli

    # test dependencies
    hello

    # fetcher/builder dependencies
    git # gorgon-fetch-git
    nix # gorgon-build-nix

    # other processes
    process-compose
    postgresql
    nats-server
  ] ++ (lib.optionals stdenv.isDarwin [
    iconv
    darwin.apple_sdk.frameworks.SystemConfiguration
  ]);

  shellHook = ''
    source .env

    # Make sure `NIX_PATH` is set, or `gorgon-build-nix` gets sad.
    test -z "$NIX_PATH" && export NIX_PATH=nixpkgs=${toString <nixpkgs>}

    # Hack to make woodpecker pipelines work locally.
    export CI_HACK_WORK_DIR=$PWD
  '';

  LIBGIT2_NO_VENDOR = true;

  DIESEL = "${pkgs.diesel-cli}/bin/diesel";
}
