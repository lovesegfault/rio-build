# Spike: confirm a Nix-sandboxed build can use /dev/kvm when the device
# is exposed via plain bind-mount (extra-sandbox-paths) — i.e., what a
# k8s hostPath volume + nix.conf sandbox-paths gives you, with no
# smarter-device-manager in the loop.
#
# Rio-stack-independent: 1 VM, stock Nix daemon, local build. The VM
# gets nested KVM via `-cpu host` so /dev/kvm exists inside.
#
# Assertion: a derivation with requiredSystemFeatures=["kvm"] runs a
# static probe that open()s /dev/kvm RDWR + ioctl(KVM_GET_API_VERSION),
# and the build output contains the version (≥12).
#
# Supports the P0564 V2 decision to drop smarter-device-manager
# (hostPath + nodeSelector replaces the device plugin for /dev/kvm).
{ pkgs, common }:
let
  # Static probe — host-compiled, store path lands in VM closure via
  # testScript interpolation (9p store mount), so it's already in the
  # VM's /nix/store. `out` argv[1] so the derivation builder can be the
  # probe directly (no /bin/sh wrapper → no extra sandbox input).
  kvmProbe = pkgs.runCommandCC "kvm-probe" { } ''
    mkdir -p $out/bin
    cat > probe.c <<'EOF'
    #include <fcntl.h>
    #include <stdio.h>
    #include <stdlib.h>
    #include <sys/ioctl.h>
    #include <linux/kvm.h>
    int main(int argc, char **argv) {
        int fd = open("/dev/kvm", O_RDWR | O_CLOEXEC);
        if (fd < 0) { perror("open /dev/kvm"); return 1; }
        int ver = ioctl(fd, KVM_GET_API_VERSION, 0);
        if (ver < 0) { perror("ioctl KVM_GET_API_VERSION"); return 2; }
        FILE *out = (argc > 1) ? fopen(argv[1], "w") : stdout;
        if (!out) { perror("fopen out"); return 3; }
        fprintf(out, "KVM_API_VERSION=%d\n", ver);
        return 0;
    }
    EOF
    $CC -O2 -o $out/bin/kvm-probe probe.c
  '';

  # In-VM build target. builtins.storePath gives the literal path
  # string-context so Nix tracks kvmProbe as an input → it's bind-
  # mounted into the sandbox. requiredSystemFeatures=["kvm"] makes Nix
  # refuse to schedule unless system-features advertises kvm — proves
  # the nodeSelector-analogue (system-features ↔ node label).
  drvFile = pkgs.writeText "kvm-sandbox-probe.nix" ''
    let probe = builtins.storePath ${kvmProbe};
    in derivation {
      name = "kvm-sandbox-probe";
      system = builtins.currentSystem;
      builder = "''${probe}/bin/kvm-probe";
      args = [ (placeholder "out") ];
      requiredSystemFeatures = [ "kvm" ];
    }
  '';
in
pkgs.testers.runNixOSTest {
  name = "rio-kvm-hostpath-spike";
  skipTypeCheck = true;
  globalTimeout = 300;

  nodes.machine = {
    virtualisation = {
      # Nested KVM: expose host CPU so VMX/SVM is visible → guest kernel
      # creates /dev/kvm. Host has nested=1 (kvmCheck below asserts the
      # OUTER /dev/kvm; the inner check below asserts nested took).
      qemu.options = [ "-cpu host" ];
      writableStore = true;
      # Register kvmProbe + drvFile in the VM's Nix DB so storePath
      # resolves. testScript interpolation alone only puts them on the
      # 9p mount; additionalPaths adds the DB registration.
      additionalPaths = [
        kvmProbe
        drvFile
      ];
      memorySize = 1024;
    };

    nix = {
      enable = true;
      settings = {
        sandbox = true;
        # The mechanism under test: bind-mount /dev/kvm into every
        # sandbox. This is what hostPath-on-pod + nix.conf gives you.
        extra-sandbox-paths = [ "/dev/kvm" ];
        # Advertise kvm so requiredSystemFeatures=["kvm"] schedules
        # locally. In k8s, the kvm pool's nix.conf sets this; the
        # nodeSelector ensures only kvm-labeled nodes get such pods.
        system-features = [
          "kvm"
          "nixos-test"
          "benchmark"
          "big-parallel"
        ];
        experimental-features = [ "nix-command" ];
        # Airgapped VM — skip cache.nixos.org probe (saves ~5s of
        # connect retries before nix-build proceeds).
        substituters = pkgs.lib.mkForce [ ];
      };
    };
  };

  testScript = ''
    ${common.kvmCheck}
    start_all()
    machine.wait_for_unit("nix-daemon.socket")

    # Nested-KVM presence in the guest. If absent, the host's nested
    # param is off or -cpu host didn't take — fail here, not as a
    # confusing build error.
    machine.succeed("test -c /dev/kvm")
    machine.succeed("ls -la /dev/kvm >&2")

    # The actual assertion: sandboxed build with requiredSystemFeatures
    # =["kvm"] sees /dev/kvm via extra-sandbox-paths and can ioctl it.
    out = machine.succeed("nix-build --no-out-link ${drvFile}").strip()
    result = machine.succeed(f"cat {out}").strip()
    print(f"[kvm-hostpath-spike] probe output: {result}")
    assert result.startswith("KVM_API_VERSION="), f"unexpected: {result!r}"
    ver = int(result.split("=", 1)[1])
    assert ver >= 12, f"KVM API version {ver} < 12"
  '';
}
