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
sec_state=$(secret_state rio/signing-key)
pub_state=$(secret_state rio/signing-key-pub)
if [ "$sec_state" = present ] && [ "$pub_state" = present ]; then
  echo "[bootstrap] rio/signing-key{,-pub} already exist, skipping"
elif [ "$sec_state" = present ]; then
  # Pub half missing, private half alive: the pub is DERIVED data —
  # the tail 32 bytes of the 64-byte expanded secret (the
  # name:base64(seed++pubkey) format; pinned by rio-cli keygen's
  # round_trip_format test). Re-derive it; never regenerate the
  # private half here — that would be a silent key rotation. A
  # corrupt secret value fails the base64 pipeline and aborts
  # (set -o pipefail), which is the correct posture: operator
  # intervention beats minting a key that doesn't match the data.
  echo "[bootstrap] rio/signing-key-pub missing; re-deriving from rio/signing-key"
  sec_val=$(aws secretsmanager get-secret-value --secret-id rio/signing-key \
    --query SecretString --output text)
  key_name=${sec_val%%:*}
  pub_b64=$(printf '%s' "${sec_val#*:}" | base64 -d | tail -c 32 | base64 -w0)
  printf '%s:%s\n' "$key_name" "$pub_b64" > /tmp/signing-key-pub
  aws secretsmanager create-secret --name rio/signing-key-pub \
    --secret-string "file:///tmp/signing-key-pub"
  echo "[bootstrap] public key (add to nix.conf trusted-public-keys):"
  cat /tmp/signing-key-pub
else
  echo "[bootstrap] generating rio/signing-key"
  tmp=$(mktemp -d)
  # Key name includes the bucket so narinfo `Sig:` lines identify
  # which cluster signed them. rio-cli keygen emits the same
  # name:base64(seed++pubkey) / name:base64(pubkey) pair that
  # `nix-store --generate-binary-cache-key` did, without needing the
  # Nix closure (or its LocalStore-init-under-readOnlyRootFilesystem
  # workaround) in the bootstrap image.
  rio-cli keygen "rio-$CHUNK_BUCKET" "$tmp/key.sec" "$tmp/key.pub"
  # Pub FIRST, create||put: in this branch the private half is
  # MISSING, so a leftover pub from a half-done prior run is stale
  # and must be overwritten. The private half is CREATE-ONLY — if
  # it exists, the state probe was wrong, and letting create-secret
  # fail (ResourceExistsException → set -e) is exactly the no-
  # silent-rotation refusal rio-cli keygen applies to local files.
  # Public half stored separately so operators can `get-secret-
  # value` it for their nix.conf trusted-public-keys without
  # access to the private half.
  aws secretsmanager create-secret --name rio/signing-key-pub \
    --secret-string "file://$tmp/key.pub" 2>/dev/null \
    || aws secretsmanager put-secret-value --secret-id rio/signing-key-pub \
      --secret-string "file://$tmp/key.pub"
  aws secretsmanager create-secret --name rio/signing-key \
    --secret-string "file://$tmp/key.sec"
  echo "[bootstrap] public key (add to nix.conf trusted-public-keys):"
  cat "$tmp/key.pub"
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
