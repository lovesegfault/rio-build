# Helm chart on local k3s/kind. Invoked as `just dev <recipe>`.
# Direct `helm upgrade --install`. Rook-Ceph provides dev S3
# (heavyweight — ~3GB memory floor, multi-minute bootstrap, but
# real S3 semantics). PG is a bitnami subchart inside the rio chart.
#
# `set shell` does NOT inherit into modules (pipefail silently off without
# this line). dotenv DOES inherit — the root justfile loads .env.local.
# working-directory pins recipe CWD to repo root so infra/... paths resolve.
set working-directory := '..'
set shell := ["bash", "-euo", "pipefail", "-c"]

# Materialize authorized_keys from RIO_SSH_PUBKEY (.env.local).
# Shebang recipe: `source` + call must share a shell (just runs
# each indented line in a fresh shell otherwise).
[private]
_ssh-key:
    #!/usr/bin/env bash
    set -euo pipefail
    source infra/lib/ssh-key.sh
    mkdir -p infra/helm/ssh
    rio_authorized_keys > infra/helm/ssh/authorized_keys

# Rook operator + cluster bring-up. Two-phase: operator CRDs must
# establish before CephCluster/CephObjectStore CRs apply. Idempotent
# (helm upgrade --install). ~2-5min on first run (Ceph bootstrap).
rook:
    #!/usr/bin/env bash
    set -euo pipefail
    op=$(nix build --no-link --print-out-paths .#helm-rook-ceph)
    cl=$(nix build --no-link --print-out-paths .#helm-rook-ceph-cluster)
    echo "[rook] operator..."
    helm upgrade --install rook-ceph "$op" \
      --namespace rook-ceph --create-namespace \
      --set csi.enableRbdDriver=false --set csi.enableCephfsDriver=false
    kubectl -n rook-ceph wait --for=condition=Available deploy/rook-ceph-operator --timeout=120s
    echo "[rook] cluster..."
    helm upgrade --install rook-ceph-cluster "$cl" \
      --namespace rook-ceph \
      -f infra/helm/rook-dev-values.yaml
    # CephObjectStoreUser reconcile generates AccessKey/SecretKey in a
    # Secret. This takes a while — Ceph mons+OSDs+RGW all have to be up
    # first. 600s is generous; on fast hardware it's ~2-3min.
    echo "[rook] waiting for ObjectStoreUser secret (~2-5min)..."
    kubectl -n rook-ceph wait --for=jsonpath='{.data.AccessKey}' \
      secret/rook-ceph-object-user-rio-rio --timeout=600s

# Copy Rook's ObjectStoreUser secret → rio-system as rio-s3-creds.
# Cross-namespace secretKeyRef doesn't work; store reads from rio-s3-creds.
# Also creates the bucket (RGW doesn't auto-create).
[private]
_s3-bridge:
    #!/usr/bin/env bash
    set -euo pipefail
    kubectl create ns rio-system --dry-run=client -o yaml | kubectl apply -f -
    kubectl -n rook-ceph get secret rook-ceph-object-user-rio-rio -o json \
      | jq '.metadata = {name:"rio-s3-creds", namespace:"rio-system"}' \
      | kubectl apply -f -
    # Bucket creation via aws cli against RGW. Port-forward because the
    # RGW Service is in rook-ceph ns and we're outside the cluster.
    ak=$(kubectl -n rook-ceph get secret rook-ceph-object-user-rio-rio -o jsonpath='{.data.AccessKey}' | base64 -d)
    sk=$(kubectl -n rook-ceph get secret rook-ceph-object-user-rio-rio -o jsonpath='{.data.SecretKey}' | base64 -d)
    kubectl -n rook-ceph port-forward svc/rook-ceph-rgw-rio 18080:80 >/dev/null &
    pf=$!; trap "kill $pf 2>/dev/null" EXIT
    sleep 2
    AWS_ACCESS_KEY_ID=$ak AWS_SECRET_ACCESS_KEY=$sk \
      aws --endpoint-url http://localhost:18080 s3 mb s3://rio-chunks 2>/dev/null \
      || echo "[s3-bridge] bucket already exists"

# Subchart: helm validates charts/ against Chart.yaml BEFORE evaluating
# condition: postgresql.enabled. Gitignored; nix store symlink (same
# FOD the helm-lint flake check uses).
[private]
_chart-deps:
    mkdir -p infra/helm/rio-build/charts
    ln -sfn "$(nix build --no-link --print-out-paths .#helm-postgresql)" \
        infra/helm/rio-build/charts/postgresql

# Envoy Gateway operator (dashboard gRPC-Web → gRPC+mTLS translation).
# CRDs first (Gateway API + gateway.envoyproxy.io extension CRDs) →
# operator Deployment → wait Available. The rio chart's dashboard-
# gateway*.yaml templates (Gateway/GRPCRoute/EnvoyProxy/...) need
# these CRDs present. Idempotent (helm upgrade --install).
envoy-gateway:
    #!/usr/bin/env bash
    set -euo pipefail
    eg=$(nix build --no-link --print-out-paths .#helm-envoy-gateway)
    echo "[envoy-gateway] operator + CRDs..."
    helm upgrade --install envoy-gateway "$eg" \
      --namespace envoy-gateway-system --create-namespace
    kubectl -n envoy-gateway-system wait --for=condition=Available \
      deploy/envoy-gateway --timeout=120s

# CRDs + SSH secret + helm install. CRDs first (chart has no crds/ dir —
# Helm's install-once-never-upgrade crds/ semantics are wrong for dev).
# Depends on `rook` having run (store reads rio-s3-creds).
apply: _ssh-key _chart-deps rook _s3-bridge
    kubectl apply --server-side -f infra/helm/crds/
    kubectl -n rio-system create secret generic rio-gateway-ssh \
      --from-file=authorized_keys=infra/helm/ssh/authorized_keys \
      --dry-run=client -o yaml | kubectl apply -f -
    helm upgrade --install rio infra/helm/rio-build \
      -f infra/helm/rio-build/values/dev.yaml \
      --set global.logLevel="${RIO_LOG_LEVEL:-debug}"

delete:
    helm uninstall rio --ignore-not-found
    kubectl -n rio-system delete secret rio-gateway-ssh rio-s3-creds --ignore-not-found

# Full teardown including Rook. Deleting the cluster chart alone leaves
# the operator's cleanup finalizers hanging; delete in reverse order.
destroy: delete
    helm uninstall rook-ceph-cluster -n rook-ceph --ignore-not-found
    # Cluster finalizer waits for data deletion. If it hangs, the Rook
    # docs say to `kubectl patch cephcluster -p '{"metadata":{"finalizers":[]}}'`.
    kubectl -n rook-ceph wait --for=delete cephcluster/rook-ceph --timeout=300s || true
    helm uninstall rook-ceph -n rook-ceph --ignore-not-found
    kubectl delete ns rook-ceph --ignore-not-found
