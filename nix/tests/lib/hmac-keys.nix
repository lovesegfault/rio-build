# HMAC key generator for VM tests. Emits two 32-byte random keys:
#   hmac.key          — assignment-token signing (scheduler signs, store verifies)
#   service-hmac.key  — service-token signing (gateway signs, store verifies)
#
# Separate keys so a compromised assignment-token key cannot mint
# service tokens (and vice versa) — see r[sec.authz.service-token].
{ pkgs }:
_:
pkgs.runCommand "rio-hmac-keys"
  {
    nativeBuildInputs = [ pkgs.openssl ];
  }
  ''
    mkdir -p $out
    # Append a trailing LF to each key: every consumer MUST byte-trim
    # it (mirroring rio-auth load_key) or its HMAC diverges from every
    # other component. A deployed Secret created with `echo` (no -n)
    # or a YAML `|` block scalar has one, so this makes "all consumers
    # trim" CI-enforced instead of a comment. The njs consumer at
    # nix/docker.nix is the tripwire's first catch (bug 007).
    openssl rand -out $out/hmac.key 32
    printf '\n' >> $out/hmac.key
    openssl rand -out $out/service-hmac.key 32
    printf '\n' >> $out/service-hmac.key
  ''
