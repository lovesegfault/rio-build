# Flat-hash FOD whose origin URL is intentionally dead (404 on the
# fetcher-split scenario's TEST-NET-3 server). Build succeeds ONLY via
# the hashed-mirrors fallback: nix-daemon's builtin:fetchurl tries
# {mirror}/sha256/{hex} first (CppNix fetchurl.cc, gated on
# FileIngestionMethod::Flat), and the scenario serves the probe body
# there.
#
# outputHash is computed in-VM from the same literal the scenario
# writes server-side (derivations.nix `hashedMirrorProbeHex`), so the
# two stay in lockstep without a shared file.
#
# Evaluated IN THE VM via nix-build. Do not reference host-eval paths.
{
  url ? "http://203.0.113.1/dead-origin",
}:
builtins.derivation {
  name = "rio-mirror-probe";
  builder = "builtin:fetchurl";
  system = "builtin";
  inherit url;
  outputHashMode = "flat";
  outputHashAlgo = "sha256";
  outputHash = builtins.hashString "sha256" "rio-hashed-mirror-probe\n";
  unpack = false;
}
