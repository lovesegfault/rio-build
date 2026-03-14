# Self-signed PKI for mTLS + HMAC VM tests.
#
# Generated at DERIVATION BUILD TIME (openssl runs once, output is a
# store path). Not hermetic across rebuilds (openssl uses /dev/urandom
# so every eval produces a fresh key set), but that's fine for a VM
# test — we only need internal consistency (server cert signed by the
# CA that the client trusts).
#
# Server cert SANs:
#   - control: worker connects to control:9001/9002; tonic derives
#     SNI from the URL authority → SNI=control. rustls verifies
#     against SANs. MUST be present.
#   - localhost: gateway (on control) connects to localhost:9001/
#     9002 for scheduler/store. Same SNI derivation → SNI=localhost.
#   - 127.0.0.1: belt + suspenders for IP-based addressing. Not
#     used currently but costs nothing.
#
# Client cert: presented by the worker for mTLS. CN=rio-worker is
# cosmetic (rio doesn't check CN, only that the cert chains to the
# CA). SANs not needed for outbound — servers only check chain.
#
# HMAC key: hex-encoded 32-byte random. rio-common/src/hmac.rs
# load_key reads RAW bytes (after stripping trailing \n). The hex
# string IS the key (64 bytes as-is, not decoded). HMAC accepts any
# key length so this is valid; hex avoids null bytes in the file.
{ pkgs }:
{
  # serverSans: DNS names for the server cert's subjectAltName. Defaults
  # match the standalone fixture (control VM hostname + localhost for
  # gateway→scheduler/store loopback).
  serverSans ? [
    "control"
    "localhost"
  ],
}:
let
  sanExt =
    "subjectAltName=" + pkgs.lib.concatMapStringsSep "," (s: "DNS:${s}") serverSans + ",IP:127.0.0.1";
in
pkgs.runCommand "rio-test-pki" { buildInputs = [ pkgs.openssl ]; } ''
  mkdir -p $out

  # ── Self-signed root CA ─────────────────────────────────────
  openssl req -x509 -newkey rsa:2048 -nodes \
    -keyout $out/ca.key -out $out/ca.crt \
    -days 3650 -subj "/CN=rio-test-ca"

  # ── Server cert (SANs parameterized) ────────────────────────
  # -addext works on openssl 1.1.1+. nixpkgs openssl is 3.x.
  openssl req -newkey rsa:2048 -nodes \
    -keyout $out/server.key -out server.csr \
    -subj "/CN=${builtins.head serverSans}"
  openssl x509 -req -in server.csr \
    -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
    -out $out/server.crt -days 3650 \
    -extfile <(printf '${sanExt}')

  # ── Client cert (worker mTLS identity) ──────────────────────
  openssl req -newkey rsa:2048 -nodes \
    -keyout $out/client.key -out client.csr \
    -subj "/CN=rio-worker"
  openssl x509 -req -in client.csr \
    -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
    -out $out/client.crt -days 3650

  # ── Gateway client cert (CN=rio-gateway for HMAC bypass) ────
  # Store's PutPath HMAC bypass requires CN=rio-gateway. The
  # gateway connects to the store as a CLIENT (store's
  # ServerTlsConfig with client_ca_root verifies this cert).
  # Without CN=rio-gateway, PutPath rejects with "assignment
  # token required (CN=... is not rio-gateway)".
  openssl req -newkey rsa:2048 -nodes \
    -keyout $out/gateway.key -out gateway.csr \
    -subj "/CN=rio-gateway"
  openssl x509 -req -in gateway.csr \
    -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
    -out $out/gateway.crt -days 3650

  # ── HMAC key (shared scheduler ↔ store secret) ──────────────
  # -hex emits lowercase hex + newline. load_key strips the
  # trailing \n (see rio-common/src/hmac.rs:105-111) so `echo`
  # vs `echo -n` doesn't matter here.
  openssl rand -hex 32 > $out/hmac.key
''
