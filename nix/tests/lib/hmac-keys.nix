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
    openssl rand -out $out/hmac.key 32
    openssl rand -out $out/service-hmac.key 32
  ''
