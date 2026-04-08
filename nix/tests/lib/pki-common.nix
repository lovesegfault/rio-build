# Shared openssl shell-snippet builders for the VM-test PKI.
#
# Both pki.nix (standalone fixture: flat $out/{server,client,gateway}.
# {crt,key}, hostname SANs) and pki-k8s.nix (k3s fixture: per-component
# $out/rio-<c>/tls.{crt,key} dirs, k8s Service-DNS SANs) emit the same
# root-CA + CSR + CA-sign openssl flow. This module owns the openssl
# invocations so a key-algo or key-size change happens in one place.
#
# RSA 2048 + PKCS#1: rustls accepts both RSA/PKCS#1 and ECDSA/PKCS#8;
# RSA is the simpler openssl-CLI path (no -pkeyopt dance). Production
# cert-manager uses ECDSA (templates/cert-manager.yaml) but the wire
# protocol is the same — VM tests only need internal consistency
# (server cert signed by the CA the client trusts).
{ lib }:
rec {
  # Self-signed root CA → $out/ca.key + $out/ca.crt. Hard-codes $out
  # as the destination: every caller is a runCommand body.
  mkCa = ''
    openssl req -x509 -newkey rsa:2048 -nodes \
      -keyout $out/ca.key -out $out/ca.crt \
      -days 3650 -subj "/CN=rio-test-ca"
  '';

  # Leaf cert signed by $out/ca.{crt,key}. CSR is a throwaway in the
  # build cwd named `<cn>.csr`.
  #   keyOut, crtOut — output paths (shell-expanded; `$out/…` works)
  #   cn             — Subject CN (also the CSR tempfile basename)
  #   sans           — bare SAN list (`DNS:foo,IP:127.0.0.1`) or null
  #                    for no subjectAltName extension
  mkLeaf =
    {
      keyOut,
      crtOut,
      cn,
      sans ? null,
    }:
    let
      ext = lib.optionalString (sans != null) "-extfile <(printf 'subjectAltName=${sans}')";
    in
    ''
      openssl req -newkey rsa:2048 -nodes \
        -keyout ${keyOut} -out ${cn}.csr \
        -subj "/CN=${cn}"
      openssl x509 -req -in ${cn}.csr \
        -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
        -out ${crtOut} -days 3650 ${ext}
    '';
}
