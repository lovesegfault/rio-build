<!-- SPDX-License-Identifier: CC-BY-SA-4.0 -->
<!-- SPDX-FileCopyrightText: 2023 embr <git@liclac.eu> -->

# nix-supervisor

A monitoring proxy for the `nix-daemon`.

## Try it

```sh
nix --extra-experimental-features 'nix-command flakes' run 'git+https://codeberg.org/gorgon/gorgon#nix-supervisor'
```

## Install it

### NixOS

```nix
{ ... }:

{
  systemd.packages = [
    (import ((builtins.fetchTarball {
      url = "https://codeberg.org/gorgon/gorgon/archive/<commit>.tar.gz";
      sha256 = ...;
    }) + "/nix-supervisor") {})
  ];

  systemd.sockets.nix-supervisor = {
    socketConfig.ListenStream = [
      "/run/nix-supervisor.sock"
      "[::1]:9649"
    ];
    wantedBy = [ "sockets.target" ];
  };

  environment.variables = {
    NIX_DAEMON_SOCKET_PATH = "/run/nix-supervisor.sock";
  };
}
```

Or clone this repository and use the overlay:

```nix
{ pkgs, ... }:

{
  systemd.packages = with pkgs; [ nix-supervisor ];

  systemd.sockets.nix-supervisor = {
    socketConfig.ListenStream = [
      "/run/nix-supervisor.sock"
      "[::1]:9649"
    ];
    wantedBy = [ "sockets.target" ];
  };

  environment.variables = {
    NIX_DAEMON_SOCKET_PATH = "/run/nix-supervisor.sock";
  };

  nixpkgs.overlays = [
    (import /home/embr/src/gorgon/gorgon/nix/overlay.nix)
  ];
}
```

Contributing
------------

Please see the [main README](..#contributing).

---

[<img src="https://nlnet.nl/logo/banner.svg" width="200" alt="NLNet Foundation logo" />](https://nlnet.nl/)
[<img src="https://nlnet.nl/image/logos/NGI0Entrust_tag.svg" width="200" alt="NGI Zero Entrust logo" />](https://nlnet.nl/NGI0/)

This project was funded through the NGI0 Entrust Fund, a fund established by NLnet with financial support from the European Commission's Next Generation Internet programme, under the aegis of DG Communications Networks, Content and Technology under grant agreement Nº 101069594.
