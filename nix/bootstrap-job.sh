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

if aws secretsmanager describe-secret --secret-id rio/mountd-hmac >/dev/null 2>&1; then
  echo "[bootstrap] rio/mountd-hmac already exists, skipping"
else
  echo "[bootstrap] generating rio/mountd-hmac"
  # SEPARATE key from rio/hmac (ADR-022 §P0559) — the scheduler signs
  # per-build Mount-admission tokens with this; the rio-mountd
  # DaemonSet on every builder node verifies them. Keeping it distinct
  # means a builder-node compromise cannot forge store-valid
  # assignment tokens. Regenerating only invalidates in-flight mountd
  # tokens (builds re-dispatch), but the describe-secret guard keeps
  # it stable like the others.
  openssl rand 32 > /tmp/mountd-hmac
  aws secretsmanager create-secret --name rio/mountd-hmac \
    --secret-binary fileb:///tmp/mountd-hmac
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
  # which cluster signed them. Format: name:base64-seed.
  # --store dummy://: nix-store opens LocalStore on startup
  # (mkdir /nix/store/.links) → EROFS under readOnlyRootFilesystem.
  # The dummy backend skips all filesystem store init.
  nix-store --store dummy:// \
    --generate-binary-cache-key "rio-$CHUNK_BUCKET" \
    "$tmp/key.sec" "$tmp/key.pub"
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

# Mountd Mount-admission Ed25519 keypair (ADR-022 mount-admission
# credentials, §P0590). The PRIVATE half is mounted only into the
# scheduler (rio-system); builder nodes get the PUBLIC trust roots
# only, so a node compromise yields no minting ability. Same dual
# guard + pub-first create||put as the narinfo signing key above:
# converge from any partial state without ever leaving a mismatched
# pair behind. Regenerating would orphan in-flight mountd tokens
# (builds re-dispatch) — the guard keeps the pair stable.
if aws secretsmanager describe-secret --secret-id rio/mountd-signing-key >/dev/null 2>&1 \
  && aws secretsmanager describe-secret --secret-id rio/mountd-signing-pub >/dev/null 2>&1; then
  echo "[bootstrap] rio/mountd-signing-key{,-pub} already exist, skipping"
else
  echo "[bootstrap] generating rio/mountd-signing-key"
  tmp=$(mktemp -d)
  # File formats are rio-auth's mountd_token loaders (what
  # `spike_mountd_client keygen` writes for VM tests/standalone):
  #   private: rio-mountd-<n>:base64(seed[32] || pubkey[32])  (one line)
  #   public:  rio-mountd-<n>:base64(pubkey[32])              (one line per active key)
  # The rio-mountd- name prefix is load-bearing — the loaders hard-fail
  # on anything else to prevent cross-wiring with the narinfo keypair.
  # Raw key material via openssl: the last 32 bytes of the PKCS#8 DER
  # are the Ed25519 seed; the last 32 bytes of the SPKI DER are the
  # raw public key.
  openssl genpkey -algorithm ed25519 -out "$tmp/mountd-signing.pem"
  openssl pkey -in "$tmp/mountd-signing.pem" -outform DER \
    | tail -c 32 > "$tmp/mountd-seed.bin"
  openssl pkey -in "$tmp/mountd-signing.pem" -pubout -outform DER \
    | tail -c 32 > "$tmp/mountd-pub.bin"
  printf 'rio-mountd-1:%s\n' \
    "$(cat "$tmp/mountd-seed.bin" "$tmp/mountd-pub.bin" | base64 -w0)" \
    > "$tmp/mountd-signing.key"
  printf 'rio-mountd-1:%s\n' \
    "$(base64 -w0 < "$tmp/mountd-pub.bin")" \
    > "$tmp/mountd-signing.pub"
  aws secretsmanager create-secret --name rio/mountd-signing-pub \
    --secret-string "file://$tmp/mountd-signing.pub" 2>/dev/null \
    || aws secretsmanager put-secret-value --secret-id rio/mountd-signing-pub \
      --secret-string "file://$tmp/mountd-signing.pub"
  aws secretsmanager create-secret --name rio/mountd-signing-key \
    --secret-string "file://$tmp/mountd-signing.key" 2>/dev/null \
    || aws secretsmanager put-secret-value --secret-id rio/mountd-signing-key \
      --secret-string "file://$tmp/mountd-signing.key"
  echo "[bootstrap] mountd trust root (public; verifiers accept tokens signed by it):"
  cat "$tmp/mountd-signing.pub"
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
