# Plan 0282: Docker image + Helm deploy — dashboard pod (nginx, Gateway API)

> **SCOPE CHANGE (2026-03-19, docs-959727):** Envoy Gateway via nixhelm supersedes the sidecar architecture. [P0273](plan-0273-envoy-sidecar-grpc-web.md) was rewritten (P0326 research: Outcome A — `GRPCRoute` natively injects `grpc_web` filter). The Envoy sidecar container in the original T3 below is **obsolete**: Envoy instances are operator-managed by the Gateway controller, not hand-configured in the dashboard pod spec. The dashboard pod becomes nginx-only; nginx proxies `/rio.*` to the Envoy Gateway Service's cluster DNS (not `localhost:8080`).
>
> **What survives:** docker image (nginx + SPA), Helm Deployment (single container), values.yaml, NetworkPolicy. **What's gone:** Envoy sidecar container, `envoy-config` ConfigMap, `envoy-certs` Secret volume — all replaced by P0273's Gateway API CRDs.

**USER A6:** no Ingress. Port-forward only (matches Grafana P0222 model).

**Architecture (post-P0273):**

```
browser ──HTTP──▶ kubectl port-forward svc/rio-dashboard:80
                  │
                  ▼
           nginx (SPA + proxy_pass)
                  │ /rio.admin.AdminService/* → envoy-gateway svc:8080
                  ▼
           Envoy Gateway (operator-managed, P0273)
                  │ grpc_web filter + mTLS client cert
                  ▼
           rio-scheduler:9001 (mTLS, UNCHANGED — USER A1)
```

nginx still fronts the SPA and proxies gRPC-Web POSTs — but the proxy target is the Envoy Gateway Service DNS (`envoy-rio-system-rio-dashboard-<hash>` by default, or a stable name via `EnvoyProxy.spec.provider.kubernetes.envoyService.name`). `proxy_buffering off` remains LOAD-BEARING.

## Entry criteria

- [P0274](plan-0274-dashboard-svelte-scaffold.md) merged (`nix/dashboard.nix` produces `dist/`)
- [P0273](plan-0273-envoy-sidecar-grpc-web.md) merged (Gateway API CRDs exist — Envoy Gateway operator + `GRPCRoute` + `EnvoyProxy` for the proxy target DNS)

## Tasks

### T1 — `feat(nix):` dashboard docker image (nginx-only)

MODIFY [`nix/docker.nix`](../../nix/docker.nix) — add dashboard image following existing `buildZstd` pattern:

```nix
# Proxy target: Envoy Gateway's generated Service name. The operator
# derives it as "envoy-{gateway-namespace}-{gateway-name}-{hash}" by
# default; EnvoyProxy.spec.provider.kubernetes.envoyService.name
# sets a stable name. P0273 T2's EnvoyProxy resource should set this
# to "rio-dashboard-gateway" so nginx can hardcode the DNS.
#
# If P0273 didn't set a stable name, this needs runtime envsubst or
# the nginx config moves to a ConfigMap that Helm templates — check
# at dispatch.
dashboardNginxConf = pkgs.writeText "nginx.conf" ''
  daemon off;
  error_log /dev/stderr info;
  events { worker_connections 1024; }
  http {
    include ${pkgs.nginx}/conf/mime.types;
    access_log /dev/stdout;
    # Single nginx upstream: the Envoy Gateway Service. Cluster DNS
    # resolves it. Port 8080 = Gateway listener (P0273 T1).
    upstream envoy_gateway {
      server rio-dashboard-gateway.rio-system.svc.cluster.local:8080;
    }
    server {
      listen 80;
      # SPA: all routes serve index.html, client-side router handles path.
      location / {
        root ${rioDashboard};
        try_files $uri /index.html;
      }
      # gRPC-Web is HTTP/1.1 POST — proxy to Envoy Gateway Service.
      # Envoy Gateway (P0273) handles grpc_web→gRPC translation +
      # mTLS client cert to scheduler. nginx just proxies HTTP/1.1.
      location ~ ^/rio\.(admin|scheduler)\./ {
        proxy_pass http://envoy_gateway;
        proxy_http_version 1.1;
        # LOAD-BEARING: without this, nginx buffers the entire
        # server-stream before flushing → defeats live log tailing
        # (P0279). Same constraint as the sidecar design.
        proxy_buffering off;
        # Pass grpc-web headers through (CORS preflight comes here).
        proxy_set_header Content-Type $content_type;
        proxy_set_header X-Grpc-Web $http_x_grpc_web;
      }
    }
  }
'';
dashboard = buildZstd {
  name = "rio-dashboard"; tag = "dev";
  contents = [ pkgs.nginx rioDashboard ];
  config = {
    Cmd = [ "${pkgs.nginx}/bin/nginx" "-c" "${dashboardNginxConf}" ];
    ExposedPorts."80/tcp" = {};
  };
};
```

**Verify at dispatch:** P0273's `EnvoyProxy` resource should set `spec.provider.kubernetes.envoyService.name: rio-dashboard-gateway` so the upstream DNS is stable. If P0273 landed without this, either (a) add it to `dashboard-gateway-tls.yaml`'s `EnvoyProxy` spec as a prep commit here, or (b) move nginx config to a ConfigMap with Helm-templated upstream (less clean — baked-in config beats runtime templating).

### T2 — `feat(flake):` expose docker-dashboard package

MODIFY [`flake.nix`](../../flake.nix) — pass `rioDashboard` to `nix/docker.nix` import, expose `docker-dashboard` in `packages`. Second flake.nix touch (P0274 was first). Different section (packages vs checks) — low conflict.

### T3 — `feat(helm):` dashboard Deployment — nginx single-container

NEW `infra/helm/rio-build/templates/dashboard.yaml`:

```yaml
{{- if .Values.dashboard.enabled }}
apiVersion: apps/v1
kind: Deployment
metadata:
  name: rio-dashboard
  labels: {{- include "rio-build.labels" . | nindent 4 }}
spec:
  replicas: {{ .Values.dashboard.replicas }}
  selector: { matchLabels: { app.kubernetes.io/name: rio-dashboard } }
  template:
    metadata:
      labels: { app.kubernetes.io/name: rio-dashboard }
    spec:
      securityContext: { runAsNonRoot: true }
      containers:
      # nginx-only: SPA serve + gRPC-Web proxy to Envoy Gateway Service.
      # No sidecar — Envoy Gateway operator manages envoy instances
      # (P0273). nginx proxies to cluster-DNS upstream, not localhost.
      - name: nginx
        image: {{ .Values.dashboard.image.repository }}:{{ .Values.dashboard.image.tag }}
        ports: [{containerPort: 80, name: http}]
        securityContext: { readOnlyRootFilesystem: true, allowPrivilegeEscalation: false, runAsUser: 101 }
        volumeMounts:
        - { name: tmp, mountPath: /var/cache/nginx }
        - { name: tmp, mountPath: /var/run }
      volumes:
      - { name: tmp, emptyDir: {} }
---
apiVersion: v1
kind: Service
metadata: { name: rio-dashboard }
spec:
  selector: { app.kubernetes.io/name: rio-dashboard }
  ports: [{name: http, port: 80, targetPort: 80}]
{{- end }}
```

**No Ingress** (USER A6). **No envoy sidecar** (P0273 Gateway API). **No envoy-config ConfigMap** (operator-managed).

### T4 — `feat(helm):` values.yaml

MODIFY [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml):

```yaml
dashboard:
  enabled: false  # default off — existing deploys unchanged
  replicas: 1
  image: { repository: rio-dashboard, tag: dev }
  # No envoyImage — Envoy Gateway operator (P0273) pulls its own.
```

### T5 — `feat(helm):` NetworkPolicy — dashboard pod → Envoy Gateway Service

MODIFY [`infra/helm/rio-build/templates/networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml). The dashboard pod needs egress to the Envoy Gateway Service (`:8080`), NOT directly to `rio-scheduler:9001`. The Envoy Gateway → scheduler mTLS path is covered by P0273's NetPol (if P0273 adds one) or the existing scheduler-ingress rules (Envoy Gateway presents a CA-signed cert — scheduler accepts any CA-signed client, doesn't check WHICH).

```yaml
  # Dashboard pod → Envoy Gateway Service (gRPC-Web proxy target).
  # Envoy Gateway handles the mTLS hop to scheduler (P0273
  # BackendTLSPolicy + EnvoyProxy.backendTLS.clientCertificateRef).
  - from:
      - podSelector: { matchLabels: { app.kubernetes.io/name: rio-dashboard } }
    to:
      - podSelector:
          matchLabels:
            gateway.envoyproxy.io/owning-gateway-name: rio-dashboard
    ports:
      - { port: 8080, protocol: TCP }
```

**Verify at dispatch:** the exact label selector for Envoy Gateway-managed pods. `gateway.envoyproxy.io/owning-gateway-name` is the operator-set label per Envoy Gateway docs — confirm against the running VM cluster or the EG source.

## Exit criteria

- `/nixbuild .#docker-dashboard` → `.tar.zst` produced (single nginx image, no envoy layer)
- `helm template infra/helm/rio-build --set dashboard.enabled=true | grep -c 'kind: Deployment'` — renders cleanly; `grep -c 'name: envoy' <output>` → 0 (no sidecar)
- `helm template ... | grep envoyImage` → 0 (values.yaml doesn't reference removed key)
- `/nixbuild .#ci` green
- `grep 'proxy_buffering off' nix/docker.nix` → ≥1 hit (LOAD-BEARING constraint preserved)
- `grep 'localhost:8080\|127.0.0.1:8080' nix/docker.nix` → 0 hits (no sidecar-local proxy target)

## Tracey

none — deployment infra. `r[dash.envoy.grpc-web-translate]` is P0273's marker (Gateway API CRDs); this plan is the nginx pod that **proxies to** P0273's Gateway.

## Files

```json files
[
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "T1: dashboard image (nginx-only, cluster-DNS upstream, proxy_buffering off LOAD-BEARING)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: docker-dashboard package (packages section, disjoint from P0274's checks)"},
  {"path": "infra/helm/rio-build/templates/dashboard.yaml", "action": "NEW", "note": "T3: Deployment — single nginx container (Envoy sidecar removed, operator-managed now)"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T4: dashboard.enabled=false default (no envoyImage key)"},
  {"path": "infra/helm/rio-build/templates/networkpolicy.yaml", "action": "MODIFY", "note": "T5: dashboard pod → Envoy Gateway Service:8080 (not direct to scheduler)"}
]
```

```
nix/docker.nix                    # T1: dashboard image (nginx-only)
flake.nix                         # T2: packages
infra/helm/rio-build/
├── templates/
│   ├── dashboard.yaml            # T3: NEW (single nginx container)
│   └── networkpolicy.yaml        # T5: allow dashboard→Envoy Gateway
└── values.yaml                   # T4
```

## Dependencies

```json deps
{"deps": [274, 273], "soft_deps": [283], "note": "SCOPE CHANGE 2026-03-19 docs-959727: Envoy Gateway via nixhelm supersedes ff9c16e4 sidecar per threat-model. P0273 rewritten (P0326 research Outcome A — GRPCRoute native grpc_web). Sidecar container obsolete; nginx proxies to Envoy Gateway Service cluster-DNS (not localhost). P0274: dist/ exists. P0273: Gateway API CRDs + EnvoyProxy + BackendTLSPolicy exist — the proxy target. proxy_buffering off LOAD-BEARING (server-stream, same constraint). No Ingress (USER A6). Scheduler untouched (USER A1). Soft-dep P0283 (VM smoke curls THROUGH this nginx → Envoy Gateway → scheduler chain). flake.nix count=32, docker.nix low, networkpolicy.yaml low — all additive. discovered_from=coordinator (SCOPE-ROT detection)."}
```

**Depends on:** [P0274](plan-0274-dashboard-svelte-scaffold.md) — `nix/dashboard.nix` produces `dist/`. [P0273](plan-0273-envoy-sidecar-grpc-web.md) — Gateway API CRDs + Envoy Gateway operator + the Service DNS that nginx proxies to. Hard-dep now (was soft-dep in sidecar design).
**Conflicts with:** `flake.nix` count=32 — packages section, disjoint from P0274's checks + P0304-T29's tracey-validate fileset. [`docker.nix`](../../nix/docker.nix) low collision. [P0283](plan-0283-dashboard-vm-smoke-curl.md) tests the end-to-end chain — depends on BOTH P0273 + this plan.
