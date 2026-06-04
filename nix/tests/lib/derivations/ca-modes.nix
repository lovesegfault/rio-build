# Floating-CA outputs in the non-default modes: flat hashing and a
# non-SHA-256 algorithm. The default-mode (recursive/sha256) floating-CA
# path is exercised end-to-end by ca-chain.nix; these two pin that the
# builder-side CA finalization AND the store-side CA verification agree
# on the declared method — a store gate that hardcoded recursive-sha256
# would reject both uploads even though the builds succeed.
#
# Built by ca-cutoff.nix via `nix-build -A flat` / `-A sha512` against
# the rio gateway store.
{ busybox }:
{
  # outputHashMode = "flat": the content hash is over the single output
  # file's bytes, not a NAR dump.
  flat = derivation {
    name = "rio-ca-flat";
    system = builtins.currentSystem;
    builder = "${busybox}/bin/sh";
    args = [
      "-c"
      "echo rio-ca-flat-payload > $out"
    ];
    __contentAddressed = true;
    outputHashMode = "flat";
    outputHashAlgo = "sha256";
  };

  # outputHashAlgo = "sha512": same recursive mode as the chain, but the
  # fingerprint/path derivation runs over a SHA-512 content hash.
  sha512 = derivation {
    name = "rio-ca-sha512";
    system = builtins.currentSystem;
    builder = "${busybox}/bin/sh";
    args = [
      "-c"
      "${busybox}/bin/mkdir -p $out && echo rio-ca-sha512-payload > $out/file"
    ];
    __contentAddressed = true;
    outputHashMode = "recursive";
    outputHashAlgo = "sha512";
  };
}
