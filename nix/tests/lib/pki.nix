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
# Client cert: presented by the worker for mTLS. CN=rio-builder is
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
  inherit (import ./pki-common.nix { inherit (pkgs) lib; }) mkCa mkLeaf;
  sanList = pkgs.lib.concatMapStringsSep "," (s: "DNS:${s}") serverSans + ",IP:127.0.0.1";
in
pkgs.runCommand "rio-test-pki" { buildInputs = [ pkgs.openssl ]; } ''
  mkdir -p $out

  # ── Self-signed root CA ─────────────────────────────────────
  ${mkCa}

  # ── Server cert (SANs parameterized) ────────────────────────
  ${mkLeaf {
    keyOut = "$out/server.key";
    crtOut = "$out/server.crt";
    cn = builtins.head serverSans;
    sans = sanList;
  }}

  # ── Client cert (worker mTLS identity) ──────────────────────
  ${mkLeaf {
    keyOut = "$out/client.key";
    crtOut = "$out/client.crt";
    cn = "rio-builder";
  }}

  # ── Gateway client cert (CN=rio-gateway for HMAC bypass) ────
  # Store's PutPath HMAC bypass requires CN=rio-gateway. The
  # gateway connects to the store as a CLIENT (store's
  # ServerTlsConfig with client_ca_root verifies this cert).
  # Without CN=rio-gateway, PutPath rejects with "assignment
  # token required (CN=... is not rio-gateway)".
  ${mkLeaf {
    keyOut = "$out/gateway.key";
    crtOut = "$out/gateway.crt";
    cn = "rio-gateway";
  }}

  # ── HMAC key (shared scheduler ↔ store secret) ──────────────
  # -hex emits lowercase hex + newline. load_key strips the
  # trailing \n (see rio-common/src/hmac.rs:105-111) so `echo`
  # vs `echo -n` doesn't matter here.
  openssl rand -hex 32 > $out/hmac.key
  openssl rand -hex 32 > $out/service-hmac.key
''
