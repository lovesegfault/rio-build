# shellcheck shell=bash
#
# rio_authorized_keys — validate RIO_SSH_PUBKEY and emit an
# authorized_keys file with the comment stripped (or replaced).
#
# Sourced by dev.just and infra/eks/deploy.sh. Do not execute directly.
#
# The comment strip matters: the gateway maps the authorized_keys
# comment field to tenant_name (rio-gateway/src/server.rs:211-223);
# the scheduler resolves it against the `tenants` table
# (rio-scheduler/src/grpc/mod.rs:149-156). Empty → single-tenant
# mode (no PG lookup, builds just work). A user's default key
# comment (user@hostname) would need a matching tenant row.
#
# Env:
#   RIO_SSH_PUBKEY   path to pubkey file; default ~/.ssh/id_ed25519.pub
#   RIO_SSH_TENANT   if set, use as the comment instead of stripping
#
# Output: authorized_keys content on stdout. Return 1 on any
# validation failure (message on stderr).

rio_authorized_keys() {
  local pubkey="${RIO_SSH_PUBKEY:-$HOME/.ssh/id_ed25519.pub}"

  if [[ ! -r "$pubkey" ]]; then
    echo "error: SSH pubkey not found at $pubkey" >&2
    echo "  set RIO_SSH_PUBKEY in .env.local (see .env.local.example)" >&2
    return 1
  fi

  # ssh-keygen -l exits 0 on both public AND private keys (it prints
  # the fingerprint either way). Catch the "pointed at id_ed25519
  # instead of id_ed25519.pub" footgun with a format check: public
  # keys start with `ssh-`; private keys start with `-----BEGIN`.
  if ! grep -q '^ssh-' "$pubkey"; then
    echo "error: $pubkey does not look like a public key" >&2
    echo "  (first line should start with 'ssh-'; did you point at the private key?)" >&2
    return 1
  fi

  if ! ssh-keygen -l -f "$pubkey" >/dev/null 2>&1; then
    echo "error: ssh-keygen cannot parse $pubkey" >&2
    return 1
  fi

  if [[ -n "${RIO_SSH_TENANT:-}" ]]; then
    awk -v t="$RIO_SSH_TENANT" '{print $1, $2, t}' "$pubkey"
  else
    awk '{print $1, $2}' "$pubkey"
  fi
}
