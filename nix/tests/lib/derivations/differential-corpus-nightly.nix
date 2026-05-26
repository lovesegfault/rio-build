# Nightly-tier differential corpus: 32-bit (i686) entries.
#
# Companion to differential-corpus.nix (the merge-gate corpus) — same
# in-VM evaluation model, same inline-script rules, but the builder and
# every PATH utility here is an *i686 busybox* (pkgsi686Linux — glibc,
# non-static; chosen because its toolchain is substitutable, unlike the
# from-source musl32 static variants), so every syscall the build makes
# goes through the 32-bit (i386) ABI. That is the whole point: the
# multi-ABI seccomp filter and 32-bit personality handling in rio-exec
# get no coverage from the x86_64 merge-gate corpus.
#
# These entries are NOT part of the per-PR merge gate. They run in the
# nightly tier together with the real-stdenv probe (which lives in
# differential-stdenv-probe.nix, not here, because it needs the full
# stdenv closure rather than a single busybox):
#
#   nix build .#nightly.vm-differential
#
# Evaluated IN THE VM via `nix-instantiate --impure --arg busybox32
# 'builtins.storePath "<i686 busybox>"'`. Keep every builder script
# inline (args = [ "-c" ... ]) for the same closure-size reasons as the
# merge-gate corpus.
{
  busybox32,
}:
let
  sh32 = "${busybox32}/bin/sh";

  mkDrv32 =
    name: script: extra:
    derivation (
      {
        inherit name;
        # The derivation still targets the host platform — only the
        # builder binary is 32-bit. Real Nix needs no extra-platforms
        # entry this way, and the native executor sees an ordinary
        # x86_64-linux job whose process happens to make i386-ABI
        # syscalls.
        system = builtins.currentSystem;
        builder = sh32;
        args = [
          "-c"
          script
        ];
        PATH = "${busybox32}/bin";
      }
      // extra
    );
in
rec {
  # Baseline 32-bit build: catches gross divergence in exec/personality
  # handling (a seccomp filter that kills i386-ABI syscalls shows up
  # here as SIGSYS on the native side only).
  i686-trivial = mkDrv32 "rio-diff-i686-trivial" ''
    {
      echo "hello from a 32-bit builder"
      uname -m
    } > $out
  '' { };

  # Setuid denial through the 32-bit ABI: the purity filter must EPERM
  # the i386 fchmodat/chmod exactly like the x86_64 one (the merge-gate
  # corpus only proves the 64-bit branch). The build itself still
  # succeeds; the recorded result must match the oracle byte-for-byte.
  i686-setuid-attempt = mkDrv32 "rio-diff-i686-setuid" ''
    touch $out
    if chmod 4755 $out 2>/dev/null; then
      echo "setuid-succeeded" > $out
    else
      echo "setuid-denied rc=$?" > $out
    fi
  '' { };

  # Multi-output + inter-output reference, 32-bit edition: output
  # registration order and reference scanning are ABI-independent, but
  # the path-scanning runs against NARs produced by 32-bit tools (mkdir,
  # echo, cp from the i686 busybox) — cheap to include, catches "works
  # on 64-bit coreutils only" assumptions in canonicalisation.
  i686-multi-output =
    mkDrv32 "rio-diff-i686-multi-output"
      ''
        mkdir -p $dev $out
        echo "development half (32-bit)" > $dev/marker
        echo "$dev" > $out/link-to-dev
      ''
      {
        outputs = [
          "out"
          "dev"
        ];
      };
}
