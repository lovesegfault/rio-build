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

# Guard on BOTH halves. With one guard and two creates, a Job
# retry after dying between the two creates (or a rotation by
# deleting only the private half) left a permanently mismatched
# pair while the Job reported success — every client signature
# check then fails. Guarding both + create||put converges from
# any partial state.
if aws secretsmanager describe-secret --secret-id rio/signing-key >/dev/null 2>&1 \
  && aws secretsmanager describe-secret --secret-id rio/signing-key-pub >/dev/null 2>&1; then
  echo "[bootstrap] rio/signing-key{,-pub} already exist, skipping"
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
  # Pub FIRST, create||put: a half-done prior run or a delete-
  # private-only rotation converges instead of leaving a stale
  # pub. If we die after pub-create, retry's guard fails (private
  # missing) → regenerate both → pub overwritten via put.
  # Public half stored separately so operators can `get-secret-
  # value` it for their nix.conf trusted-public-keys without
  # access to the private half.
  aws secretsmanager create-secret --name rio/signing-key-pub \
    --secret-string "file://$tmp/key.pub" 2>/dev/null \
    || aws secretsmanager put-secret-value --secret-id rio/signing-key-pub \
      --secret-string "file://$tmp/key.pub"
  aws secretsmanager create-secret --name rio/signing-key \
    --secret-string "file://$tmp/key.sec" 2>/dev/null \
    || aws secretsmanager put-secret-value --secret-id rio/signing-key \
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
