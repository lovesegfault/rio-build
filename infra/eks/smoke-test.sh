#!/usr/bin/env bash
# EKS smoke test: deploy rio-build, run a build, kill a worker, verify reassign.
#
# Prerequisites:
#   - terraform apply completed (cluster exists)
#   - kubectl configured (run `$(terraform output -raw kubeconfig_command)`)
#   - STORE_IAM_ROLE_ARN set (run `export STORE_IAM_ROLE_ARN=$(terraform output -raw store_iam_role_arn)`)
#   - SSH keypair for gateway access at ~/.ssh/rio_test_ed25519{,.pub}
#     (generate: `ssh-keygen -t ed25519 -f ~/.ssh/rio_test_ed25519 -N ''`)
#
# Usage:
#   ./infra/eks/smoke-test.sh
#
# Exit 0 on success, 1 on failure. Logs to stdout.

set -euo pipefail

NS=rio-system
SSH_KEY=${SSH_KEY:-~/.ssh/rio_test_ed25519}

log() { echo "[smoke] $*" >&2; }
fail() { log "FAIL: $*"; exit 1; }

# --- Deploy ---
log "applying rio-build manifests"
kubectl kustomize deploy/overlays/eks | envsubst | kubectl apply -f -

log "waiting for control-plane pods..."
kubectl -n "$NS" wait --for=condition=Ready pod \
  -l app.kubernetes.io/part-of=rio-build,app.kubernetes.io/component!=worker \
  --timeout=300s || fail "control-plane pods not ready"

# --- Gateway SSH setup ---
log "configuring gateway authorized_keys"
kubectl -n "$NS" create secret generic rio-gateway-ssh \
  --from-file=authorized_keys="${SSH_KEY}.pub" \
  --dry-run=client -o yaml | kubectl apply -f -

# Restart gateway to pick up key. `kubectl rollout restart` +
# wait for new pods Ready.
kubectl -n "$NS" rollout restart deployment/rio-gateway
kubectl -n "$NS" rollout status deployment/rio-gateway --timeout=120s

# --- Get gateway address ---
log "waiting for gateway LoadBalancer..."
for _ in $(seq 1 60); do
  GATEWAY_HOST=$(kubectl -n "$NS" get svc rio-gateway \
    -o jsonpath='{.status.loadBalancer.ingress[0].hostname}' 2>/dev/null || true)
  [[ -n "$GATEWAY_HOST" ]] && break
  sleep 5
done
[[ -n "$GATEWAY_HOST" ]] || fail "LoadBalancer did not get hostname"
log "gateway: $GATEWAY_HOST"

# Wait for DNS to resolve + SSH to accept.
log "waiting for gateway SSH..."
for _ in $(seq 1 60); do
  if ssh -i "$SSH_KEY" -o StrictHostKeyChecking=no \
     -o ConnectTimeout=5 "rio@${GATEWAY_HOST}" true 2>/dev/null; then
    break
  fi
  sleep 5
done

# --- Create WorkerPool ---
log "creating WorkerPool"
cat <<EOF | kubectl apply -f -
apiVersion: rio.build/v1alpha1
kind: WorkerPool
metadata:
  name: smoke-test
  namespace: $NS
spec:
  replicas: { min: 2, max: 4 }
  autoscaling: { metric: queueDepth, targetValue: 5 }
  maxConcurrentBuilds: 2
  fuseCacheSize: 20Gi
  systems: [x86_64-linux]
  features: []
  sizeClass: ""
  image: rio-worker:latest
  tolerations:
    - key: rio.build/worker
      operator: Equal
      value: "true"
      effect: NoSchedule
  nodeSelector:
    rio.build/node-role: worker
EOF

log "waiting for worker pods..."
kubectl -n "$NS" wait --for=condition=Ready pod \
  -l rio.build/pool=smoke-test --timeout=300s || fail "workers not ready"

# --- Build 1: hello (quick sanity) ---
log "building nixpkgs#hello via SSH gateway"
nix build nixpkgs#hello \
  --store "ssh-ng://rio@${GATEWAY_HOST}?ssh-key=${SSH_KEY}" \
  --no-link || fail "hello build failed"
log "hello build OK"

# --- Build 2: longer build + kill worker mid-build ---
# Use a derivation that takes ~30s so we have time to kill a worker.
# `nixpkgs#cowsay` rebuilt from source usually works; if it's cached,
# we need something else. `sleep` drv would be ideal but requires
# writing a local expr.
log "starting longer build + killing worker mid-stream..."

# Get scheduler disconnect baseline BEFORE kill (G6 lesson from
# phase3a: capture baseline, then assert delta).
DISCONNECTS_BEFORE=$(kubectl -n "$NS" exec deploy/rio-scheduler -- \
  curl -s localhost:9091/metrics | \
  grep '^rio_scheduler_worker_disconnects_total' | \
  awk '{print $NF}' || echo 0)
log "worker_disconnects_total baseline: $DISCONNECTS_BEFORE"

# Start a build in background. `nixpkgs#git` usually takes a
# minute+ if not cached.
nix build nixpkgs#git \
  --store "ssh-ng://rio@${GATEWAY_HOST}?ssh-key=${SSH_KEY}" \
  --no-link &
BUILD_PID=$!

# Wait a few seconds for dispatch, then kill a worker pod.
sleep 10
log "killing one worker pod (simulating node failure)"
kubectl -n "$NS" delete pod \
  -l rio.build/pool=smoke-test --wait=false --field-selector=status.phase=Running \
  | head -1 || true

# Wait for build to complete. Should succeed despite worker kill
# (scheduler reassigns).
wait "$BUILD_PID" || fail "build failed after worker kill (reassign didn't work?)"
log "build completed despite worker kill"

# Verify disconnect metric increased.
DISCONNECTS_AFTER=$(kubectl -n "$NS" exec deploy/rio-scheduler -- \
  curl -s localhost:9091/metrics | \
  grep '^rio_scheduler_worker_disconnects_total' | \
  awk '{print $NF}')
log "worker_disconnects_total after: $DISCONNECTS_AFTER"
[[ "$DISCONNECTS_AFTER" -gt "$DISCONNECTS_BEFORE" ]] || \
  fail "disconnect metric didn't increase (scheduler didn't see the disconnect?)"

log "SMOKE TEST PASSED"
log "cleanup: kubectl -n $NS delete workerpool smoke-test"
