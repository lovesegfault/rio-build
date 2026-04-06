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
  nsStore ? "rio-store",
  nsBuilders ? "rio-builders",
  # Additional component names to generate leaf certs for. The
  # default set matches cert-manager.yaml's `range`; callers that
  # need more (e.g. k3s-full.nix with envoyGatewayEnabled → adds
  # "dashboard-envoy" so EnvoyProxy.spec.backendTLS.
  # clientCertificateRef resolves) extend here. Each component `c`
  # gets a `rio-<c>-tls` Secret with tls.crt/tls.key/ca.crt.
  extraComponents ? [ ],
}:
let
  # Per-component target namespace. ADR-019: store moved to rio-store,
  # builder/fetcher pods live in rio-builders/rio-fetchers. Secrets
  # are ns-scoped — a store pod in rio-store can't mount a Secret from
  # rio-system. SANs include BOTH the short Service name (tonic's
  # :authority from `rio-scheduler:9001`) and the FQDN form (cross-ns
  # addressing like `rio-store.rio-store:9002`).
  nsFor =
    c:
    if c == "store" then
      nsStore
    else if c == "builder" then
      nsBuilders
    else
      ns;

  # Components that get per-service certs. Matches the `range` in
  # cert-manager.yaml. Each gets SANs for the short Service name
  # (tonic's :authority derivation) + all FQDN forms + localhost
  # (for kubectl-exec in-pod tooling, see cert-manager.yaml comment).
  components = [
    "scheduler"
    "store"
    "gateway"
    "controller"
  ]
  ++ extraComponents;

  mkSans =
    c:
    let
      svc = "rio-${c}";
      cns = nsFor c;
    in
    pkgs.lib.concatMapStringsSep "," (s: "DNS:${s}") [
      svc
      "${svc}.${cns}"
      "${svc}.${cns}.svc"
      "${svc}.${cns}.svc.cluster.local"
      "localhost"
    ];

  # Worker SAN: client-cert only (workers connect OUT, don't serve
  # inbound gRPC). cert-manager.yaml uses a wildcard for belt-and-
  # suspenders; the SAN isn't actually verified on the client side
  # (scheduler only checks CA-signed).
  workerSans = "DNS:rio-builder,DNS:*.${ns}.svc.cluster.local";

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
            -extfile <(printf 'subjectAltName=${mkSans c}')
          cp $out/ca.crt $out/rio-${c}/ca.crt
        '') components}

        # ── Worker cert ─────────────────────────────────────────────
        mkdir -p $out/rio-builder
        openssl req -newkey rsa:2048 -nodes \
          -keyout $out/rio-builder/tls.key -out /tmp/worker.csr \
          -subj "/CN=rio-builder"
        openssl x509 -req -in /tmp/worker.csr \
          -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
          -out $out/rio-builder/tls.crt -days 3650 \
          -extfile <(printf 'subjectAltName=${workerSans}')
        cp $out/ca.crt $out/rio-builder/ca.crt
      '';

  # Secret-name → target-namespace pairs. ADR-019: store/builder
  # Secrets land in their own namespaces so pods can mount them.
  secretTargets =
    map (c: {
      name = "rio-${c}";
      namespace = nsFor c;
    }) components
    ++ [
      {
        name = "rio-builder";
        namespace = nsBuilders;
      }
    ];
in
{
  inherit pki;

  # k8s manifest with all TLS Secrets. Feed into services.k3s.manifests
  # so it's applied at boot, before rio-* pods start.
  #
  # Generated inside a runCommand with ${pki} as a regular build input
  # — NO IFD (builtins.readFile). The pki derivation is non-deterministic
  # (openssl genrsa = random keys). With IFD, the Secret content came
  # from the LOCAL eval-time build; the test's ${pki} store path might
  # be REBUILT on the remote builder with different keys. Same path, different
  # contents → grpcurl's -cacert doesn't match the server cert's CA →
  # "crypto/rsa: verification error". Observed v23.
  #
  # As a regular input, Nix guarantees the SAME pki build feeds both
  # this manifest and the VM closure's ${pki} reference.
  secretsManifest = pkgs.runCommand "rio-tls-secrets.yaml" { } ''
    ${pkgs.lib.concatMapStringsSep "\n" (t: ''
      cat >> $out <<EOF
      ---
      apiVersion: v1
      kind: Secret
      metadata:
        name: ${t.name}-tls
        namespace: ${t.namespace}
      type: kubernetes.io/tls
      data:
        tls.crt: $(base64 -w0 < ${pki}/${t.name}/tls.crt)
        tls.key: $(base64 -w0 < ${pki}/${t.name}/tls.key)
        ca.crt: $(base64 -w0 < ${pki}/${t.name}/ca.crt)
      EOF
    '') secretTargets}
  '';
}
