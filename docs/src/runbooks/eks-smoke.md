# EKS Smoke Test Runbook

Manual walkthrough of `infra/eks/smoke-test.sh`. Use this when the
automated script fails or for first-time setup validation.

## Prerequisites

- `terraform apply` complete (see `infra/eks/README.md`)
- `kubectl` configured: `$(cd infra/eks && terraform output -raw kubeconfig_command)`
- `STORE_IAM_ROLE_ARN` exported: `export STORE_IAM_ROLE_ARN=$(cd infra/eks && terraform output -raw store_iam_role_arn)`
- SSH keypair for gateway: `ssh-keygen -t ed25519 -f ~/.ssh/rio_test_ed25519 -N ''`
- cert-manager installed: `kubectl apply -f https://github.com/cert-manager/cert-manager/releases/download/v1.14.0/cert-manager.yaml`

## Step 1: Deploy

```bash
kubectl kustomize deploy/overlays/eks | envsubst | kubectl apply -f -
```

Wait for control-plane readiness:

```bash
kubectl -n rio-system wait --for=condition=Ready pod \
  -l 'app.kubernetes.io/part-of=rio-build,app.kubernetes.io/component!=worker' \
  --timeout=300s
```

**Troubleshooting if pods stuck Pending:**
- `kubectl describe pod <name>` — check Events for scheduling issues
- Common: no nodes matching `system` nodegroup (terraform nodegroup
  didn't create, or AZ mismatch)
- Fix: `kubectl get nodes --show-labels | grep system`

**Troubleshooting if pods CrashLoopBackOff:**
- `kubectl logs <pod> -p` — previous container's logs
- Common: PG connection refused (RDS not ready or Secret wrong)
- Fix: verify `rio-postgres` Secret: `kubectl -n rio-system get secret rio-postgres -o jsonpath='{.data.url}' | base64 -d`

## Step 2: Gateway SSH Setup

```bash
kubectl -n rio-system create secret generic rio-gateway-ssh \
  --from-file=authorized_keys=~/.ssh/rio_test_ed25519.pub \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl -n rio-system rollout restart deployment/rio-gateway
kubectl -n rio-system rollout status deployment/rio-gateway --timeout=120s
```

## Step 3: Get Gateway Address

```bash
# Poll until NLB provisions (takes 2-3 min)
while true; do
  GATEWAY_HOST=$(kubectl -n rio-system get svc rio-gateway \
    -o jsonpath='{.status.loadBalancer.ingress[0].hostname}')
  [[ -n "$GATEWAY_HOST" ]] && break
  echo "waiting for LoadBalancer..."
  sleep 5
done
echo "Gateway: $GATEWAY_HOST"
```

**Troubleshooting if NLB never provisions:**
- AWS Load Balancer Controller not installed: `kubectl get pods -n kube-system | grep aws-load-balancer`
- Check Events: `kubectl -n rio-system describe svc rio-gateway`
- Common: missing IAM permissions for the controller's SA

## Step 4: Create WorkerPool

```bash
cat <<EOF | kubectl apply -f -
apiVersion: rio.build/v1alpha1
kind: WorkerPool
metadata:
  name: smoke-test
  namespace: rio-system
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

kubectl -n rio-system wait --for=condition=Ready pod \
  -l rio.build/pool=smoke-test --timeout=300s
```

**Troubleshooting if workers stuck ContainerCreating:**
- Check `/dev/fuse` on worker node: `kubectl debug node/<worker-node> -it --image=busybox -- ls -la /dev/fuse`
- If missing: worker AMI doesn't have FUSE support (use Amazon
  Linux 2023 with fuse3 installed, or a custom AMI)

## Step 5: Build Test

```bash
nix build nixpkgs#hello \
  --store "ssh-ng://rio@${GATEWAY_HOST}?ssh-key=~/.ssh/rio_test_ed25519" \
  --no-link
```

**Expected:** completes in <1 minute (hello is small). Worker pod
logs show `build succeeded, uploading outputs`.

## Step 6: Resilience Test (Kill Worker)

```bash
# Baseline metric (G6 lesson: capture BEFORE action)
DISCONNECTS_BEFORE=$(kubectl -n rio-system exec deploy/rio-scheduler -- \
  curl -s localhost:9091/metrics | \
  grep '^rio_scheduler_worker_disconnects_total' | awk '{print $NF}')
echo "Baseline disconnects: $DISCONNECTS_BEFORE"

# Start a longer build in background
nix build nixpkgs#git \
  --store "ssh-ng://rio@${GATEWAY_HOST}?ssh-key=~/.ssh/rio_test_ed25519" \
  --no-link &
BUILD_PID=$!

# Wait for dispatch, then kill a worker
sleep 10
kubectl -n rio-system delete pod \
  -l rio.build/pool=smoke-test --wait=false \
  --field-selector=status.phase=Running | head -1

# Wait for build completion
wait $BUILD_PID && echo "Build completed despite worker kill ✓"

# Verify metric
DISCONNECTS_AFTER=$(kubectl -n rio-system exec deploy/rio-scheduler -- \
  curl -s localhost:9091/metrics | \
  grep '^rio_scheduler_worker_disconnects_total' | awk '{print $NF}')
echo "After disconnects: $DISCONNECTS_AFTER"
[[ $DISCONNECTS_AFTER -gt $DISCONNECTS_BEFORE ]] && echo "Reassign confirmed ✓"
```

## Step 7: GC Test (optional)

```bash
# Trigger GC via grpcurl (dry run first)
grpcurl -d '{"dry_run": true, "grace_period_hours": 2}' \
  -cacert /etc/rio/tls/ca.crt \
  -cert /etc/rio/tls/tls.crt \
  -key /etc/rio/tls/tls.key \
  rio-scheduler:9001 rio.admin.AdminService/TriggerGC
```

## Cleanup

```bash
kubectl -n rio-system delete workerpool smoke-test
kubectl -n rio-system delete -k deploy/overlays/eks
cd infra/eks && terraform destroy -var chunk_bucket=<your-bucket>
```

## Troubleshooting Matrix

| Symptom | Check | Fix |
|---|---|---|
| Scheduler pod NOT_SERVING | `kubectl logs` for "lease" | Standby replica — normal. Check both pods. |
| Worker `unable to mount overlay` | `kubectl describe pod` Events | privileged: true or SYS_ADMIN cap |
| Build hangs at "waiting for build" | Scheduler metrics `rio_scheduler_workers_active` | 0 → no worker registered. Check worker logs. |
| `nix copy` permission denied | `authorized_keys` secret | Secret mounted? Gateway restarted after creating secret? |
| Store PutPath PERMISSION_DENIED | `rio_store_hmac_rejected_total{reason}` | HMAC key mismatch scheduler↔store, or mTLS cert chain issue |
