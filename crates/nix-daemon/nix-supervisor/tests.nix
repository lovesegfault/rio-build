# SPDX-License-Identifier: MIT-0
# SPDX-FileCopyrightText: 2023 Alyssa Ross <hi@alyssa.is>
# SPDX-FileCopyrightText: 2024 embr <git@liclac.eu>

{ pkgs ? import ../nix/nixpkgs.default.nix
, lib ? pkgs.lib
, nix-supervisor ? pkgs.callPackage ./. { buildType = "debug"; }
, nix-daemon-tests ? pkgs.callPackage ../nix-daemon/tests.nix {}
, cargo ? pkgs.cargo
, hello ? pkgs.hello
}:

let
  isPackage = lib.types.package.check;
  nixPackages = lib.filterAttrs (_: isPackage) (pkgs.callPackage ../nix/nix-packages.nix {});

  inherit (nix-daemon-tests) test-nix;

  mkNixTest = attr: nix: pkgs.testers.nixosTest ({ lib, ... }: {
    name = "nix-supervisor-test-${attr}";

    nodes.machine = { modulesPath, ... }: {
      imports = [
        "${modulesPath}/installer/cd-dvd/channel.nix"
      ];

      environment.systemPackages = [ hello ];
      system.extraDependencies = [ hello.drvPath ];

      systemd.services.nix-supervisor = {
        after = [ "network.target" "nix-daemon.service" ];
        serviceConfig = {
          ExecStart = ''${nix-supervisor}/bin/nix-supervisor \
            --sock=/tmp/nix-supervisor.sock'';
          UMask = "0000";
        };
        wantedBy = [ "multi-user.target" ];
      };

      users.users.testuser = {
        isNormalUser = true;
      };

      nix.package = nix;
      nix.settings = {
        substituters = lib.mkForce [ "file://${test-nix.binary-cache}" ];
        trusted-substituters = [ "testuser" ];
        trusted-users = [ "testuser" ];
      };
    };

    testScript = { nodes, ... }: ''
        machine.wait_for_unit("nix-supervisor")

        # Check the current nix version.
        print(machine.succeed('nix --version'))
        print(machine.succeed('cat /etc/nix/nix.conf'))

        # Run the nix-daemon integration tests through nix-supervisor.
        print(machine.succeed('sudo -u testuser env -C $(sudo -u testuser mktemp -d) NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock ${test-nix}/bin/nix-integration'))

        # Regular, old-fashioned CLI.
        machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix-build '<nixpkgs>' -A hello >&2")

        # Newfangled, experimental CLI.
        machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' run -f '<nixpkgs>' hello >&2")
        machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' build -f '<nixpkgs>' hello >&2")

        machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' show-derivation -f '<nixpkgs>' hello >&2")
        machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' path-info -f '<nixpkgs>' hello >&2")

        # REPL commands.
        machine.succeed("echo 'hello' | sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' repl -f '<nixpkgs>' >&2")
        machine.succeed("echo ':b hello' | sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' repl -f '<nixpkgs>' >&2")
        machine.succeed("echo ':p hello' | sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' repl -f '<nixpkgs>' >&2")

        # Shells.
        machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' shell -f '<nixpkgs>' hello -c hello >&2")
        machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix-shell -p hello --command 'hello' >&2")

        # For some reason, these hang forever when ran in test cases.
        # machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' why-depends -f '<nixpkgs>' hello hello >&2")
        # machine.succeed("sudo -u testuser env NIX_DAEMON_SOCKET_PATH=/tmp/nix-supervisor.sock nix --extra-experimental-features 'nix-command' develop -f '<nixpkgs>' hello -c hello >&2")
    '';
  });
in
lib.mapAttrs mkNixTest nixPackages
