# Fixed ed25519 test keypair for the k3s-full fixture's jwtEnabled mode.
#
# Seed = 32 bytes of 0x42 (ASCII 'B'). Pubkey derived deterministically
# via ed25519 (SigningKey::from_bytes → verifying_key → 32-byte point).
# The derivation is the same in ed25519-dalek (what rio uses) and
# RFC 8032 — regenerate with:
#
#   python3 -c 'from cryptography.hazmat.primitives.asymmetric import ed25519;
#     import base64; seed=bytes([0x42]*32);
#     sk=ed25519.Ed25519PrivateKey.from_private_bytes(seed);
#     print(base64.b64encode(seed).decode());
#     print(base64.b64encode(sk.public_key().public_bytes_raw()).decode())'
#
# Why hardcoded, not a runCommand: passing these through helm --set
# requires the VALUE at eval time. Generating via pkgs.runCommand +
# builtins.readFile is IFD (categorically bad — see deps-hygiene).
# The seed is TEST-ONLY (airgapped VM, never production); the pubkey
# is public by definition. Hardcoding is fine.
#
# Why not pki-k8s.nix's separate-manifest approach: that works for TLS
# because the Helm chart NEVER renders the TLS Secrets (cert-manager
# does, and cert-manager isn't in the airgap set). Here, jwt.enabled=
# true makes the Helm chart render BOTH the mount (which we want) AND
# the ConfigMap/Secret (which would overwrite a pre-applied manifest).
# Passing the keys via --set makes the Helm-rendered ConfigMap/Secret
# carry the real content — no race, no override dance.
{
  # 32 bytes of 0x42, STANDARD base64. Matches what gateway's main.rs
  # expects: the Secret mount gives this string (kubelet strips the
  # Secret.data outer b64 layer), gateway base64-decodes to 32 raw
  # bytes → SigningKey::from_bytes.
  seedB64 = "QkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkJCQkI=";

  # Derived public key, STANDARD base64. scheduler/store's
  # load_jwt_pubkey base64-decodes → 32 bytes → VerifyingKey::from_bytes.
  # Must be a valid curve point (not just any 32 bytes) — it is,
  # because it was derived from a real seed.
  #
  # ripsecrets allowlisted via .secretsignore (this file). The
  # pubkey is PUBLIC by definition; ripsecrets flags any high-entropy
  # 40+char base64 literal regardless.
  pubkeyB64 = "IVL40Zt5HSRFMkLhXy6rbLfP+ntqXtMAl5YOBpiB2xI=";
}
