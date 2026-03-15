# Self-signed PKI for the k3s-full fixture's mTLS.
#
# Replaces cert-manager for the airgapped VM test. Generates the same
# Secret layout (tls.crt/tls.key/ca.crt) and SAN set that
# templates/cert-manager.yaml would produce, so `tls.enabled: true`
# works without preloading cert-manager images (phase4c).
#
# Output: a store path with per-component cert dirs + a k8s manifest
# (tls-secrets.yaml) ready for services.k3s.manifests.
#
# Why not reuse pki.nix: that one is tuned for the standalone
# fixture (SANs = control/localhost hostnames). Here SANs must be k8s
# Service DNS names (rio-scheduler, rio-store, etc.) — tonic derives
# SNI from the URL :authority, and values/vmtest-full.yaml uses
# Service names in scheduler_addr/store_addr.
{ pkgs }:
{
  ns ? "rio-system",
}:
let
  # Components that get per-service certs. Matches the `range` in
  # cert-manager.yaml. Each gets SANs for the short Service name
  # (tonic's :authority derivation) + all FQDN forms + localhost
  # (for kubectl-exec in-pod tooling, see cert-manager.yaml comment).
  components = [
    "scheduler"
    "store"
    "gateway"
    "controller"
  ];

  mkSans =
    svc:
    pkgs.lib.concatMapStringsSep "," (s: "DNS:${s}") [
      svc
      "${svc}.${ns}"
      "${svc}.${ns}.svc"
      "${svc}.${ns}.svc.cluster.local"
      "localhost"
    ];

  # Worker SAN: client-cert only (workers connect OUT, don't serve
  # inbound gRPC). cert-manager.yaml uses a wildcard for belt-and-
  # suspenders; the SAN isn't actually verified on the client side
  # (scheduler only checks CA-signed).
  workerSans = "DNS:rio-worker,DNS:*.${ns}.svc.cluster.local";

  # RSA (not ECDSA): pki.nix uses RSA + PKCS#1, rustls accepts both.
  # cert-manager uses ECDSA+PKCS8 but RSA is simpler with openssl
  # CLI (no -pkeyopt dance). rustls parses either — the PKCS8 note
  # in cert-manager.yaml is about the EC-specific legacy header,
  # not RSA.
  pki =
    pkgs.runCommand "rio-k8s-pki"
      {
        buildInputs = [ pkgs.openssl ];
      }
      ''
        mkdir -p $out

        # ── Root CA ─────────────────────────────────────────────────
        openssl req -x509 -newkey rsa:2048 -nodes \
          -keyout $out/ca.key -out $out/ca.crt \
          -days 3650 -subj "/CN=rio-test-ca"

        # ── Per-component leaf certs ────────────────────────────────
        ${pkgs.lib.concatMapStringsSep "\n" (c: ''
          mkdir -p $out/rio-${c}
          openssl req -newkey rsa:2048 -nodes \
            -keyout $out/rio-${c}/tls.key -out /tmp/${c}.csr \
            -subj "/CN=rio-${c}"
          openssl x509 -req -in /tmp/${c}.csr \
            -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
            -out $out/rio-${c}/tls.crt -days 3650 \
            -extfile <(printf 'subjectAltName=${mkSans "rio-${c}"}')
          cp $out/ca.crt $out/rio-${c}/ca.crt
        '') components}

        # ── Worker cert ─────────────────────────────────────────────
        mkdir -p $out/rio-worker
        openssl req -newkey rsa:2048 -nodes \
          -keyout $out/rio-worker/tls.key -out /tmp/worker.csr \
          -subj "/CN=rio-worker"
        openssl x509 -req -in /tmp/worker.csr \
          -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
          -out $out/rio-worker/tls.crt -days 3650 \
          -extfile <(printf 'subjectAltName=${workerSans}')
        cp $out/ca.crt $out/rio-worker/ca.crt
      '';

  # Base64 helper: reads a PKI file, strips trailing newline,
  # base64-encodes for k8s Secret.data (which is base64, not
  # stringData). Nix builtins have no base64 so shell out.
  #
  # Two-stage: this runCommand takes the pki derivation as an
  # input, so Nix sequences it after pki builds. readFile then
  # pulls the result back into eval for the manifest interpolation.
  b64 =
    path:
    pkgs.lib.removeSuffix "\n" (
      builtins.readFile (
        pkgs.runCommand "b64" { } ''
          base64 -w0 < ${pki}/${path} > $out
        ''
      )
    );

  mkSecret = name: ''
    ---
    apiVersion: v1
    kind: Secret
    metadata:
      name: ${name}-tls
      namespace: ${ns}
    type: kubernetes.io/tls
    data:
      tls.crt: ${b64 "${name}/tls.crt"}
      tls.key: ${b64 "${name}/tls.key"}
      ca.crt: ${b64 "${name}/ca.crt"}
  '';
in
{
  inherit pki;

  # k8s manifest with all 5 TLS Secrets. Feed into
  # services.k3s.manifests so it's applied at boot, before the
  # rio-* pods start (same ordering slot as 03-gateway-ssh-empty).
  secretsManifest = pkgs.writeText "rio-tls-secrets.yaml" (
    pkgs.lib.concatMapStringsSep "\n" mkSecret (map (c: "rio-${c}") components ++ [ "rio-worker" ])
  );
}
