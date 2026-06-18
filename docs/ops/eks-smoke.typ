#import "/lib/rio.typ": *

#show: rio.with(domains: none)


Manual walkthrough of `cargo xtask k8s qa --health -p eks`. Use this when the
automated run fails or for first-time setup validation.

= Prerequisites

- `terraform apply` complete (see `infra/eks/README.md`)
- `kubectl` configured: `$(cd infra/eks && terraform output -raw kubeconfig_command)`
- `STORE_IAM_ROLE_ARN` exported: `export STORE_IAM_ROLE_ARN=$(cd infra/eks && terraform output -raw store_iam_role_arn)`
- SSH keypair for gateway: `ssh-keygen -t ed25519 -f ~/.ssh/rio_test_ed25519 -N ''`

= Step 1: Deploy

```bash
cargo xtask k8s -p eks up --deploy    # helm upgrade --install from working tree
```

Wait for control-plane readiness:

```bash
kubectl -n rio-system wait --for=condition=Ready pod \
  -l 'app.kubernetes.io/part-of=rio-build,app.kubernetes.io/component!=worker' \
  --timeout=300s
```

*Troubleshooting if pods stuck Pending:*
- `kubectl describe pod <name>` — check Events for scheduling issues
- Common: no nodes matching `system` nodegroup (terraform nodegroup
  didn't create, or @az mismatch)
- Fix: `kubectl get nodes --show-labels | grep system`

*Troubleshooting if pods CrashLoopBackOff:*
- `kubectl logs <pod> -p` — previous container's logs
- Common: PG connection refused (RDS not ready or Secret wrong)
- Fix: verify `rio-postgres` Secret: `kubectl -n rio-system get secret rio-postgres -o jsonpath='{.data.url}' | base64 -d`

= Step 2: Gateway SSH Setup

```bash
kubectl -n rio-system create secret generic rio-gateway-ssh \
  --from-file=authorized_keys=~/.ssh/rio_test_ed25519.pub \
  --dry-run=client -o yaml | kubectl apply -f -

kubectl -n rio-system rollout restart deployment/rio-gateway
kubectl -n rio-system rollout status deployment/rio-gateway --timeout=120s
```

= Step 3: Get Gateway Address

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

*Troubleshooting if @nlb never provisions:*
- AWS Load Balancer Controller not installed: `kubectl get pods -n kube-system | grep aws-load-balancer`
- Check Events: `kubectl -n rio-system describe svc rio-gateway`
- Common: missing IAM permissions for the controller's SA

*NLB target health is N/M, not M/M — this is correct.* `externalTrafficPolicy: Local` means only the N nodes hosting a rio-gateway pod pass the `healthCheckNodePort` probe; the rest are intentionally unhealthy. The dualstack NLB forwards to an IPv6-only target group; xtask sets `preserve_client_ip.enabled=false` and `enable-prefix-for-ipv6-source-nat=on` so IPv4 clients on the dualstack listener can reach IPv6 targets. Nodes set their own primary IPv6 at boot (`primary-ipv6-init.service` in the NixOS AMI).

= Step 4: Create Pool

Pods land in `rio-builders`, *not* `rio-system` --- builder pods need
`CAP_SYS_ADMIN` (@overlayfs mount), and `rio-system` is PSA
#(refs.psa)("rio-system"). See #rref("sec.psa.control-plane-restricted").

```bash
cat <<EOF | kubectl apply -f -
apiVersion: rio.build/v1alpha1
kind: Pool
metadata:
  name: smoke-test
  namespace: rio-builders   # NOT rio-system (see above)
spec:
  kind: Builder
  image: rio-builder:latest
  systems: [x86_64-linux]
  features: []
  maxConcurrent: 4
  nodeSelector:
    rio.build/node-role: builder
  tolerations:
    - key: rio.build/builder
      operator: Equal
      value: "true"
      effect: NoSchedule
EOF

# Builder Jobs spawn on demand once a build is queued — no standing pods to wait for.
# Per-pod cpu/mem/disk come from the scheduler's per-drv SpawnIntent (ADR-023),
# NOT from Pool.spec — there is no resources field.
rio-cli pool describe smoke-test -n rio-builders    # or: kubectl -n rio-builders get pool smoke-test -o yaml
```

*Troubleshooting if workers stuck ContainerCreating:*
- Check `/dev/fuse` on worker node: `kubectl debug node/<worker-node> -it --image=busybox -- ls -la /dev/fuse`
- If missing: worker AMI doesn't have @fuse support (use Amazon
  Linux 2023 with fuse3 installed, or a custom AMI)

= Step 5: Build Test

```bash
nix build nixpkgs#hello \
  --store "ssh-ng://rio@${GATEWAY_HOST}?ssh-key=~/.ssh/rio_test_ed25519" \
  --no-link
```

*Expected:* completes in \<1 minute (hello is small). Worker pod
logs show `build succeeded, uploading outputs`.

= Step 6: Resilience Test (Kill Worker)

```bash
# Baseline metric (G6 lesson: capture BEFORE action). A killed pod's
# attempt is requeued at the report fold / establishment sweep — the
# requeue histogram count is the observable (summed across causes).
REQUEUES_BEFORE=$(kubectl -n rio-system exec deploy/rio-scheduler -- \
  curl -s localhost:9091/metrics | \
  grep '^rio_scheduler_attempt_requeue_seconds_count' | awk '{s+=$NF} END {print s+0}')
echo "Baseline requeues: $REQUEUES_BEFORE"

# Start a longer build in background
nix build nixpkgs#git \
  --store "ssh-ng://rio@${GATEWAY_HOST}?ssh-key=~/.ssh/rio_test_ed25519" \
  --no-link &
BUILD_PID=$!

# Wait for dispatch, then kill a worker
sleep 10
kubectl -n rio-builders delete pod \
  -l rio.build/pool=smoke-test --wait=false \
  --field-selector=status.phase=Running | head -1

# Wait for build completion
wait $BUILD_PID && echo "Build completed despite worker kill ✓"

# Verify metric
REQUEUES_AFTER=$(kubectl -n rio-system exec deploy/rio-scheduler -- \
  curl -s localhost:9091/metrics | \
  grep '^rio_scheduler_attempt_requeue_seconds_count' | awk '{s+=$NF} END {print s+0}')
echo "After requeues: $REQUEUES_AFTER"
[[ $REQUEUES_AFTER -gt $REQUEUES_BEFORE ]] && echo "Requeue confirmed ✓"
```

= Step 7: GC Test (optional)

Trigger GC via `rio-cli `#(refs.cli-sub)("gc") over a port-forward (dry run first):
```bash
cargo xtask k8s cli -p eks -- gc --dry-run --grace-hours 2
```

= Cleanup

```bash
kubectl -n rio-builders delete pool smoke-test
helm uninstall rio -n rio-system    # or: cargo xtask k8s destroy -p eks for full teardown
```

= Aurora major-version upgrade (17→18, 18→19, …)

Bumping `engine_version` across a major in `infra/eks/rds.tf` and
running `terraform apply` will *attempt* an in-place major upgrade
(the cluster has `allow_major_version_upgrade = true` and
`apply_immediately = true`). On a dev cluster that is fine — Aurora
takes the writer offline for the upgrade window (minutes), and
`skip_final_snapshot = true` so there is no pre-upgrade snapshot.

For a cluster with data you care about, do NOT let terraform drive
the upgrade. Instead:

+ Snapshot first: `aws rds create-db-cluster-snapshot
  --db-cluster-identifier <name>-pg --db-cluster-snapshot-identifier
  <name>-pg-preNN`.
+ Prefer Blue/Green: `aws rds create-blue-green-deployment
  --source arn:aws:rds:...:cluster:<name>-pg --target-engine-version
  NN.x` → validate the green cluster → switchover. Downtime is the
  switchover window (\~1min), not the upgrade window.
+ In-place fallback: `aws rds modify-db-cluster --db-cluster-identifier
  <name>-pg --engine-version NN.x --allow-major-version-upgrade
  --apply-immediately`. Then `terraform apply` to reconcile state.
+ Post-upgrade: re-run the `xtask deploy` PG preflight (it asserts
  `max_connections` against the modeled `pg_max_connections` output —
  the value is keyed on ACU range, not engine major, so it should
  match unchanged).

No custom parameter group is defined (rds.tf), so Aurora uses
`default.aurora-postgresqlNN` automatically — nothing to re-create.

= Troubleshooting Matrix

#table(
  columns: 3,
  table.header([Symptom], [Check], [Fix]),
  [Scheduler pod NOT_SERVING],
  [`kubectl logs` for "lease"],
  [Standby replica — normal. Check both pods.],

  [Worker `unable to mount overlay`],
  [`kubectl describe pod` Events],
  [privileged: true or SYS_ADMIN cap],

  [Build hangs at "waiting for build"],
  [Scheduler metrics #(refs.metric)("rio_scheduler_open_attempts")],
  [0 → no executor pod has pulled its assignment yet. Check the pool's
    Jobs/pods and the builder logs.],

  [`nix copy` permission denied],
  [`authorized_keys` secret],
  [Secret mounted? Gateway restarted after creating secret?],

  [Store PutPath PERMISSION_DENIED],
  [#(refs.metric)("rio_store_hmac_rejected_total")`{reason}`],
  [HMAC key mismatch (assignment: scheduler↔store; service: gateway↔store)],
)
