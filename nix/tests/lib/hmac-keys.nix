# HMAC key generator for VM tests. Emits two 32-byte keys:
#   hmac.key          — assignment-token signing (scheduler signs, store verifies)
#   service-hmac.key  — service-token signing (gateway signs, store verifies)
#
# Separate keys so a compromised assignment-token key cannot mint
# service tokens (and vice versa) — see r[sec.authz.service-token].
#
# DETERMINISTIC, not random. This is an input-addressed derivation: a
# non-deterministic build (`openssl rand`) produces DIFFERENT bytes at
# the SAME store path when the derivation is realized independently on
# more than one machine (local vs remote builder). The executor token
# (fixtures/standalone.nix executorTokenEnv) is signed in a SEPARATE
# derivation from the key file the scheduler loads — if the two ever
# reference copies built on different machines, the signature never
# verifies and every executor RPC fails with "HMAC verification failed
# (tampered or wrong key)". Same failure class as the IFD ×
# non-determinism cert mismatch in .claude/rules/ci-failure-patterns.md.
# Test keys do not need to be secret or random — they need to be 32
# bytes and byte-identical everywhere.
{ pkgs }:
_:
pkgs.runCommand "rio-hmac-keys" { } ''
  mkdir -p $out
  # Append a trailing LF to each key: every consumer MUST byte-trim
  # it (mirroring rio-auth load_key) or its HMAC diverges from every
  # other component. A deployed Secret created with `echo` (no -n)
  # or a YAML `|` block scalar has one, so this makes "all consumers
  # trim" CI-enforced instead of a comment. The njs consumer at
  # nix/docker.nix is the tripwire's first catch (bug 007).
  printf 'rio-vmtest-assignment-hmac-key32' > $out/hmac.key
  printf '\n' >> $out/hmac.key
  printf 'rio-vmtest-service-hmac-key-32by' > $out/service-hmac.key
  printf '\n' >> $out/service-hmac.key
  # 32 key bytes + 1 LF each. 32 matches SHA-256's output size (the
  # standard HMAC key length recommendation); guard the literals so an
  # edit cannot silently change the length.
  for f in $out/hmac.key $out/service-hmac.key; do
    test "$(stat -c%s "$f")" = 33 || { echo "key $f is not 32 bytes + LF" >&2; exit 1; }
  done
''
