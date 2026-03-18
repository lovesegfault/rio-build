# Plan 0273: Envoy sidecar — gRPC-Web → gRPC+mTLS translate (THE GATEKEEPER)

**USER A1: Envoy as the DASHBOARD's sidecar. Scheduler COMPLETELY untouched.** No Rust. No `tonic-web` dep. No third listener. No `#[derive(Clone)]` on `AdminServiceImpl`. R5 (`.accept_http1`) **disappears** — Envoy handles HTTP/1.1 natively. R3 (streaming) is known-good territory — Envoy gRPC-Web streaming is battle-tested.

**Architecture:** dashboard pod = nginx (static SPA) + envoy sidecar. Envoy listens `:8080` gRPC-Web (HTTP/1.1), translates to gRPC/HTTP2 over mTLS, connects to `rio-scheduler-svc:8443`. Scheduler sees a normal mTLS gRPC client — it has no idea a browser is on the other end.

```
┌──────────────── rio-dashboard Pod ─────────────────┐
│ nginx:80 (SPA) ──/api──▶ envoy:8080 (gRPC-Web)     │
│                          grpc_web filter → router   │
│                          mTLS client cert ──────────┼──▶ scheduler-svc:8443 (UNCHANGED)
└────────────────────────────────────────────────────┘
```

**A1/A6 interaction:** with port-forward-only (A6), the "browsers can't mTLS" problem doesn't bite during dev — port-forward tunnels past Service TLS. Envoy is kept for future-proofing: if Ingress ever happens, translation is already in place. Present complexity is the price of not touching the scheduler.

## Entry criteria

none — Wave 0a root. Parallel with [P0274](plan-0274-dashboard-svelte-scaffold.md), [P0276](plan-0276-getbuildgraph-rpc-pg-backed.md).

## Tasks

### T1 — `feat(infra):` Envoy bootstrap config

NEW `infra/helm/rio-build/files/envoy-dashboard.yaml`:

```yaml
static_resources:
  listeners:
  - name: grpc_web
    address: { socket_address: { address: 0.0.0.0, port_value: 8080 } }
    filter_chains:
    - filters:
      - name: envoy.filters.network.http_connection_manager
        typed_config:
          "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
          codec_type: AUTO
          stat_prefix: grpc_web
          route_config:
            virtual_hosts:
            - name: scheduler
              domains: ["*"]
              routes:
              - match: { prefix: "/rio.admin.AdminService/" }
                route: { cluster: scheduler, timeout: 0s }  # 0 = no timeout (streaming)
              cors:
                allow_origin_string_match: [{ prefix: "" }]  # permissive MVP
                allow_methods: "GET, POST, OPTIONS"
                allow_headers: "content-type,x-grpc-web,x-user-agent"
                expose_headers: "grpc-status,grpc-message,grpc-status-details-bin"
          http_filters:
          - name: envoy.filters.http.grpc_web
            typed_config: { "@type": type.googleapis.com/envoy.extensions.filters.http.grpc_web.v3.GrpcWeb }
          - name: envoy.filters.http.cors
            typed_config: { "@type": type.googleapis.com/envoy.extensions.filters.http.cors.v3.Cors }
          - name: envoy.filters.http.router
            typed_config: { "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router }
  clusters:
  - name: scheduler
    type: STRICT_DNS
    http2_protocol_options: {}  # upstream is HTTP/2 (gRPC)
    load_assignment:
      cluster_name: scheduler
      endpoints:
      - lb_endpoints:
        - endpoint: { address: { socket_address: { address: rio-scheduler, port_value: 8443 } } }
    transport_socket:
      name: envoy.transport_sockets.tls
      typed_config:
        "@type": type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext
        common_tls_context:
          tls_certificates:
          - certificate_chain: { filename: "/etc/envoy/certs/tls.crt" }
            private_key: { filename: "/etc/envoy/certs/tls.key" }
          validation_context:
            trusted_ca: { filename: "/etc/envoy/certs/ca.crt" }
```

### T2 — `feat(infra):` mTLS client cert for Envoy

Check how [`rio-cli`](../../rio-cli/)/[`rio-controller`](../../rio-controller/) present mTLS certs to scheduler — likely a shared CA + cert-manager `Certificate` resource or a static Secret. Envoy gets the same treatment. At dispatch: `rg 'tls.crt\|ClientTlsConfig' rio-cli/src/ rio-controller/src/` + `ls infra/helm/rio-build/templates/ | grep -i cert`.

NEW `infra/helm/rio-build/templates/dashboard-envoy-cert.yaml` (cert-manager Certificate OR static Secret, matching existing pattern).

### T3 — `feat(nix):` envoy image pull

MODIFY [`nix/docker-pulled.nix`](../../nix/docker-pulled.nix) — add `envoyproxy/envoy:v1.x` image (for VM tests).

### T4 — `test(vm):` curl gate — unary + streaming through Envoy

Extend the lightest standalone-fixture scenario. Provision an envoy process between curl and scheduler:

```python
# r[verify dash.envoy.grpc-web-translate]  (col-0 header comment)
# R1 de-risk: gRPC-Web framing works (unary ClusterStatus via Envoy)
machine.succeed(
    "curl -sf -X POST http://envoy:8080/rio.admin.AdminService/ClusterStatus "
    "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
    r"--data-binary $'\x00\x00\x00\x00\x00' "
    "| xxd | head -1 | grep -q '^00000000: 00'"  # DATA frame, compression=0
)
# R3 de-risk: server-streaming through Envoy BEFORE any TS exists.
# 0x80 trailer-frame byte proves Envoy doesn't buffer the stream.
machine.succeed(
    "printf '\\x00\\x00\\x00\\x00\\x0a\\x0a\\x08nonexist' > /tmp/req.bin && "
    "curl -sf -X POST http://envoy:8080/rio.admin.AdminService/GetBuildLogs "
    "-H 'content-type: application/grpc-web+proto' -H 'x-grpc-web: 1' "
    "--data-binary @/tmp/req.bin | xxd | tail -5 | grep -q ' 80'"
)
```

**The `0x80` grep is the single most important assertion in Wave 0** — it proves server-streaming works end-to-end through Envoy before a single line of Svelte.

## Exit criteria

- `/nbr .#ci` green
- Both curl assertions pass: DATA frame `0x00` prefix + trailer frame `0x80` byte
- `git diff main -- rio-scheduler/` → EMPTY (USER A1: scheduler untouched)

## Tracey

References existing markers:
- `r[dash.envoy.grpc-web-translate]` — T1+T4 implement/verify (seeded by [P0284](plan-0284-dashboard-docs-sweep-markers.md) — see Spec additions there; this plan's `r[verify]` may land before the marker text, that's fine — `tracey validate` catches dangling in the closeout sweep)

## Files

```json files
[
  {"path": "infra/helm/rio-build/files/envoy-dashboard.yaml", "action": "NEW", "note": "T1: Envoy bootstrap (grpc_web filter + mTLS upstream)"},
  {"path": "infra/helm/rio-build/templates/dashboard-envoy-cert.yaml", "action": "NEW", "note": "T2: mTLS client cert for Envoy"},
  {"path": "nix/docker-pulled.nix", "action": "MODIFY", "note": "T3: envoy image"},
  {"path": "nix/tests/scenarios/cli.nix", "action": "MODIFY", "note": "T4: curl gate — unary + 0x80 trailer grep"}
]
```

```
infra/helm/rio-build/
├── files/envoy-dashboard.yaml    # T1: NEW Envoy config
└── templates/
    └── dashboard-envoy-cert.yaml # T2: NEW mTLS cert
nix/
├── docker-pulled.nix             # T3: envoy image
└── tests/scenarios/cli.nix       # T4: curl gate
```

## Dependencies

```json deps
{"deps": [245], "soft_deps": [], "note": "USER A1: Envoy sidecar — scheduler UNTOUCHED. Deps on P0245: T4's r[verify dash.envoy.grpc-web-translate] in cli.nix needs the marker seeded first (else .#ci tracey-validate dangling-ref). Parallel with P0274/P0276 after P0245 — zero file overlap. R3/R5 de-risked by Envoy. EXIT: git diff rio-scheduler/ = EMPTY."}
```

**Depends on:** none — Wave 0a root.
**Conflicts with:** none. All NEW files or low-collision (`docker-pulled.nix` shared with P0268's toxiproxy pull — both additive). Zero scheduler touch per USER A1.
