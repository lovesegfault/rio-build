set -euo pipefail
: "${AWS_REGION:?}" "${CHUNK_BUCKET:?}"

if aws secretsmanager describe-secret --secret-id rio/hmac >/dev/null 2>&1; then
  echo "[bootstrap] rio/hmac already exists, skipping"
else
  echo "[bootstrap] generating rio/hmac"
  # 32 raw bytes. SecretBinary (not SecretString) — the HMAC key
  # isn't text. ESO's decodingStrategy: None preserves raw bytes.
  openssl rand 32 > /tmp/hmac
  aws secretsmanager create-secret --name rio/hmac \
    --secret-binary fileb:///tmp/hmac
fi

if aws secretsmanager describe-secret --secret-id rio/service-hmac >/dev/null 2>&1; then
  echo "[bootstrap] rio/service-hmac already exists, skipping"
else
  echo "[bootstrap] generating rio/service-hmac"
  # SEPARATE key from rio/hmac — gateway signs ServiceClaims with
  # this; store verifies. A leaked assignment key cannot mint
  # service tokens (different secret, different claims shape).
  openssl rand 32 > /tmp/service-hmac
  aws secretsmanager create-secret --name rio/service-hmac \
    --secret-binary fileb:///tmp/service-hmac
fi

# Fail-closed state probe for the signing-key pair. The signing key
# is the one secret here where a wrong "missing" verdict is
# DESTRUCTIVE: the recovery path mints a fresh keypair, and rotating
# the live key invalidates every narinfo `Sig:` made under the old
# one. So "missing" must mean the API SAID ResourceNotFoundException
# — never a throttle, an IAM hiccup, or a network blip (all of which
# also exit nonzero). Anything else aborts the Job; the Job retries.
secret_state() {
  _ss_id=$1
  if _ss_err=$(aws secretsmanager describe-secret --secret-id "$_ss_id" 2>&1 >/dev/null); then
    echo present
  elif printf '%s' "$_ss_err" | grep -q ResourceNotFoundException; then
    echo missing
  else
    printf '[bootstrap] describe-secret %s failed without ResourceNotFoundException; refusing to guess:\n%s\n' \
      "$_ss_id" "$_ss_err" >&2
    return 1
  fi
}

# Probe BOTH halves and dispatch on the pair state. With one guard
# and two creates, a Job retry after dying between the two creates
# (or a rotation by deleting only the private half) left a
# permanently mismatched pair while the Job reported success — every
# client signature check then fails.
#
# ALL key-byte work is delegated to rio-cli (the signing_keyfmt
# codec): the shell never decodes, slices, or re-encodes key
# material. The previous re-derive (`base64 -d | tail -c 32 |
# base64 -w0`) assumed the 64-byte expanded payload; for a 32-byte
# seed-only entry — a format the store's own Signer::parse accepts —
# it published the PRIVATE SEED verbatim onto rio/signing-key-pub
# and into this Job's log (round-16 bug_023, critical).
sec_state=$(secret_state rio/signing-key)
pub_state=$(secret_state rio/signing-key-pub)
tmp=$(mktemp -d)
if [ "$sec_state" = present ]; then
  # Private half is live: NEVER regenerate. Converge the pub half to
  # the seed-derived public entry. derive-pub reads the secret on
  # stdin (never argv) and refuses corrupt or internally inconsistent
  # entries with nothing published — operator intervention beats
  # minting or advertising a key that doesn't match the data.
  aws secretsmanager get-secret-value --secret-id rio/signing-key \
    --query SecretString --output text > "$tmp/sec.entry"
  rio-cli keygen derive-pub < "$tmp/sec.entry" > "$tmp/pub.derived"
  if [ "$pub_state" = present ]; then
    # Pair-consistency probe: every upgrade's Job log must show
    # either "pair consistent" or a heal. cmp (byte-exact) — the
    # canonical entries are newline-free; only the CLI transport
    # newline from --output text is stripped.
    aws secretsmanager get-secret-value --secret-id rio/signing-key-pub \
      --query SecretString --output text > "$tmp/pub.stored.raw"
    printf '%s' "$(cat "$tmp/pub.stored.raw")" > "$tmp/pub.stored"
    if cmp -s "$tmp/pub.stored" "$tmp/pub.derived"; then
      echo "[bootstrap] signing-key pair consistent"
    else
      echo "[bootstrap] rio/signing-key-pub does not match the private half; healing"
      aws secretsmanager put-secret-value --secret-id rio/signing-key-pub \
        --secret-string "file://$tmp/pub.derived"
      echo "[bootstrap] public key (add to nix.conf trusted-public-keys):"
      cat "$tmp/pub.derived"; echo
    fi
  else
    echo "[bootstrap] rio/signing-key-pub missing; re-deriving from rio/signing-key"
    aws secretsmanager create-secret --name rio/signing-key-pub \
      --secret-string "file://$tmp/pub.derived" 2>/dev/null \
      || aws secretsmanager put-secret-value --secret-id rio/signing-key-pub \
        --secret-string "file://$tmp/pub.derived"
    echo "[bootstrap] public key (add to nix.conf trusted-public-keys):"
    cat "$tmp/pub.derived"; echo
  fi
else
  echo "[bootstrap] generating rio/signing-key"
  # Key name includes the bucket so narinfo `Sig:` lines identify
  # which cluster signed them. rio-cli keygen emits the same
  # name:base64(seed++pubkey) / name:base64(pubkey) pair that
  # `nix-store --generate-binary-cache-key` did, without needing the
  # Nix closure in the bootstrap image.
  rio-cli keygen new "rio-$CHUNK_BUCKET" "$tmp/key.sec" "$tmp/key.pub"
  # PRIVATE half FIRST, create-only: this create IS the concurrency
  # guard. Two overlapping Jobs racing this branch both call
  # create-secret; the loser gets ResourceExistsException → set -e
  # aborts having written NOTHING (the pub write is sequenced after
  # the private CAS), and its retry converges through the re-derive/
  # heal branch above. The previous order (pub overwrite first) let
  # the loser clobber the winner's pub with a key that was about to
  # be discarded (round-16 merged_bug_015).
  aws secretsmanager create-secret --name rio/signing-key \
    --secret-string "file://$tmp/key.sec"
  # Pub second, create||put: any pre-existing pub here is stale by
  # definition (its private half did not exist) and must be
  # overwritten. Stored separately so operators can get-secret-value
  # it for nix.conf trusted-public-keys without access to the
  # private half.
  aws secretsmanager create-secret --name rio/signing-key-pub \
    --secret-string "file://$tmp/key.pub" 2>/dev/null \
    || aws secretsmanager put-secret-value --secret-id rio/signing-key-pub \
      --secret-string "file://$tmp/key.pub"
  echo "[bootstrap] public key (add to nix.conf trusted-public-keys):"
  cat "$tmp/key.pub"; echo
fi

if aws secretsmanager describe-secret --secret-id rio/gateway-host-key >/dev/null 2>&1; then
  echo "[bootstrap] rio/gateway-host-key already exists, skipping"
else
  echo "[bootstrap] generating rio/gateway-host-key"
  tmp=$(mktemp -d)
  # OpenSSH-format ed25519 private key. -N "" (no passphrase),
  # -C "" (no comment — the comment field in a host key is unused
  # and would otherwise leak the build-time hostname). -f writes
  # to $tmp (the /tmp emptyDir, writable under
  # readOnlyRootFilesystem). russh::keys::load_secret_key reads
  # the OpenSSH PEM format ssh-keygen emits.
  ssh-keygen -t ed25519 -N "" -C "" -f "$tmp/host_key" </dev/null
  aws secretsmanager create-secret --name rio/gateway-host-key \
    --secret-string "file://$tmp/host_key"
  echo "[bootstrap] gateway host key fingerprint (for known_hosts pinning):"
  ssh-keygen -l -f "$tmp/host_key.pub"
fi
