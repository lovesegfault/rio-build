# Plan 0282: Docker image + Helm deploy (nginx + Envoy sidecar)

**USER A1 architecture:** dashboard pod = nginx (static SPA) + Envoy sidecar. nginx proxies `/rio.admin.AdminService/*` → `localhost:8080` (Envoy, same pod). Envoy → `rio-scheduler:8443` (mTLS). **`proxy_buffering off` is load-bearing** — nginx must not buffer the server-stream.

**USER A6:** no Ingress. Port-forward only (matches Grafana P0222 model).

Can dispatch early — only deps on `nix/dashboard.nix` producing `dist/`.

## Entry criteria

- [P0274](plan-0274-dashboard-svelte-scaffold.md) merged (`nix/dashboard.nix` produces `dist/`)

## Tasks

### T1 — `feat(nix):` dashboard docker image

MODIFY [`nix/docker.nix`](../../nix/docker.nix) — add dashboard image following existing `buildZstd` pattern:

```nix
dashboardNginxConf = pkgs.writeText "nginx.conf" ''
  daemon off;
  error_log /dev/stderr info;
  events { worker_connections 1024; }
  http {
    include ${pkgs.nginx}/conf/mime.types;
    access_log /dev/stdout;
    server {
      listen 80;
      # SPA: all routes serve index.html, client-side router handles path
      location / {
        root ${rioDashboard};
        try_files $uri /index.html;
      }
      # gRPC-Web is HTTP/1.1 POST — proxy to Envoy sidecar (localhost).
      # USER A1: Envoy handles translation, nginx just proxies to it.
      location /rio.admin.AdminService/ {
        proxy_pass http://127.0.0.1:8080;
        proxy_http_version 1.1;
        # LOAD-BEARING: without this, nginx buffers the entire server-stream
        # before flushing → defeats live log tailing (P0279).
        proxy_buffering off;
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

### T2 — `feat(flake):` expose docker-dashboard package

MODIFY [`flake.nix`](../../flake.nix) — pass `rioDashboard` to `nix/docker.nix` import, expose `docker-dashboard` in `packages`. Second flake.nix touch (P0274 was first). Different section (packages vs checks) — low conflict.

### T3 — `feat(helm):` dashboard Deployment — nginx + Envoy sidecar

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
      # nginx: serves SPA + proxies /rio.admin.AdminService/* → localhost:8080 (Envoy)
      - name: nginx
        image: {{ .Values.dashboard.image.repository }}:{{ .Values.dashboard.image.tag }}
        ports: [{containerPort: 80, name: http}]
        securityContext: { readOnlyRootFilesystem: true, allowPrivilegeEscalation: false, runAsUser: 101 }
        volumeMounts:
        - { name: tmp, mountPath: /var/cache/nginx }
        - { name: tmp, mountPath: /var/run }
      # Envoy sidecar: gRPC-Web → gRPC+mTLS translation (USER A1)
      - name: envoy
        image: {{ .Values.dashboard.envoyImage }}
        args: ["-c", "/etc/envoy/envoy.yaml"]
        ports: [{containerPort: 8080, name: grpc-web}]
        volumeMounts:
        - { name: envoy-config, mountPath: /etc/envoy }
        - { name: envoy-certs, mountPath: /etc/envoy/certs, readOnly: true }
      volumes:
      - { name: tmp, emptyDir: {} }
      - { name: envoy-config, configMap: { name: rio-dashboard-envoy } }
      - { name: envoy-certs, secret: { secretName: rio-dashboard-envoy-cert } }  # from P0273 T2
---
apiVersion: v1
kind: ConfigMap
metadata: { name: rio-dashboard-envoy }
data:
  envoy.yaml: |
{{ .Files.Get "files/envoy-dashboard.yaml" | indent 4 }}
---
apiVersion: v1
kind: Service
metadata: { name: rio-dashboard }
spec:
  selector: { app.kubernetes.io/name: rio-dashboard }
  ports: [{name: http, port: 80, targetPort: 80}]
{{- end }}
```

**No Ingress** (USER A6).

### T4 — `feat(helm):` values.yaml

MODIFY [`infra/helm/rio-build/values.yaml`](../../infra/helm/rio-build/values.yaml):
```yaml
dashboard:
  enabled: false  # default off — existing deploys unchanged
  replicas: 1
  image: { repository: rio-dashboard, tag: dev }
  envoyImage: envoyproxy/envoy:v1.29-latest
```

### T5 — `feat(helm):` NetworkPolicy — dashboard pod → scheduler:8443

MODIFY [`infra/helm/rio-build/templates/networkpolicy.yaml`](../../infra/helm/rio-build/templates/networkpolicy.yaml) — allow `rio-dashboard` pod → `rio-scheduler:8443` (mTLS port, NOT a new plaintext port — USER A1: scheduler untouched).

## Exit criteria

- `/nbr .#docker-dashboard` → `.tar.zst` produced
- `helm template infra/helm/rio-build --set dashboard.enabled=true` renders cleanly (2 containers, ConfigMap, Service, no Ingress)
- `/nbr .#ci` green

## Tracey

none — deployment infra.

## Files

```json files
[
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "T1: dashboard image (nginx, proxy_buffering off LOAD-BEARING)"},
  {"path": "flake.nix", "action": "MODIFY", "note": "T2: docker-dashboard package (packages section, disjoint from P0274's checks)"},
  {"path": "infra/helm/rio-build/templates/dashboard.yaml", "action": "NEW", "note": "T3: Deployment — nginx + Envoy sidecar (USER A1)"},
  {"path": "infra/helm/rio-build/values.yaml", "action": "MODIFY", "note": "T4: dashboard.enabled=false default"},
  {"path": "infra/helm/rio-build/templates/networkpolicy.yaml", "action": "MODIFY", "note": "T5: dashboard→scheduler:8443 (mTLS port, scheduler untouched)"}
]
```

```
nix/docker.nix                    # T1: dashboard image
flake.nix                         # T2: packages (disjoint section)
infra/helm/rio-build/
├── templates/
│   ├── dashboard.yaml            # T3: NEW (nginx + Envoy sidecar)
│   └── networkpolicy.yaml        # T5: allow dashboard→scheduler mTLS
└── values.yaml                   # T4
```

## Dependencies

```json deps
{"deps": [274], "soft_deps": [273], "note": "USER A1: nginx + Envoy sidecar, NOT tonic-web. Scheduler UNTOUCHED — NetPol targets existing :8443 mTLS. proxy_buffering off LOAD-BEARING. No Ingress (A6). Parallel with P0275-P0281. Soft-dep P0273 for envoy config+cert reuse."}
```

**Depends on:** [P0274](plan-0274-dashboard-svelte-scaffold.md) — `nix/dashboard.nix` produces `dist/`. Soft: [P0273](plan-0273-envoy-sidecar-grpc-web.md) — Envoy config + cert template.
**Conflicts with:** `flake.nix` — packages section, disjoint from P0274's checks section. `docker.nix` medium collision.
