#!/usr/bin/env nix-shell
#!nix-shell -i bash -p awscli2 kubectl openssh nix
# shellcheck shell=bash
#
# smoke-test.sh — end-to-end build via SSM-tunneled gateway.
#
# Prerequisites:
#   - `just eks deploy` has run (cluster is up, control-plane Ready)
#   - AWS_PROFILE set
#   - Session Manager plugin installed (`aws ssm start-session`
#     needs it — it's a separate binary from awscli. The nix-shell
#     shebang doesn't pull it because nixpkgs's ssm-session-manager-
#     plugin has a spotty packaging history; install via the AWS
#     docs if missing.)
#
# Exit 0 on success, 1 on any failure.

set -euo pipefail

NS=rio-system
TENANT=smoke-test
SSH_KEY=/tmp/rio-smoke-key
LOCAL_PORT=2222
REPO_ROOT="$(git rev-parse --show-toplevel)"
TF_DIR="$REPO_ROOT/infra/eks"

log() { echo "[smoke] $*" >&2; }
die() { log "FAIL: $*"; exit 1; }

tf() { tofu -chdir="$TF_DIR" output -raw "$1" 2>/dev/null \
  || die "tofu output '$1' missing"; }

# Bounded retry: run a command up to N times with a sleep between.
# Returns 0 on first success, 1 after exhausting retries. Never
# sleeps more than the caller's interval (user rule: <=30s sleeps).
retry() {
  local n=$1 interval=$2; shift 2
  for _ in $(seq 1 "$n"); do
    "$@" && return 0
    sleep "$interval"
  done
  return 1
}

# ----------------------------------------------------------------
# 1. Bootstrap tenant via rio-cli in the scheduler pod
# ----------------------------------------------------------------
# rio-cli is bundled in the scheduler image (/bin/rio-cli). The
# pod's RIO_TLS__* env is already set (tls-mounts.yaml), so it
# talks mTLS to localhost:9001 with zero extra config.
#
# Idempotent: CreateTenant returns AlreadyExists on rerun, which
# we treat as success (the tenant's there, that's what we wanted).
log "bootstrapping tenant '$TENANT'"
if ! kubectl -n "$NS" exec deploy/rio-scheduler -- \
     rio-cli create-tenant "$TENANT" 2>&1 | tee /tmp/rio-cli-create.log; then
  grep -q 'AlreadyExists\|already exists' /tmp/rio-cli-create.log \
    || die "create-tenant failed (not an AlreadyExists): $(cat /tmp/rio-cli-create.log)"
  log "tenant already exists, continuing"
fi

# ----------------------------------------------------------------
# 2. SSH key with tenant-name comment → authorized_keys Secret
# ----------------------------------------------------------------
# The gateway maps the authorized_keys COMMENT field to tenant_name
# (rio-gateway/src/server.rs auth_publickey). The key comment MUST
# match the tenant we just created.
log "generating SSH key with comment '$TENANT'"
rm -f "$SSH_KEY" "$SSH_KEY.pub"
ssh-keygen -t ed25519 -C "$TENANT" -f "$SSH_KEY" -N '' -q

log "installing authorized_keys"
kubectl -n "$NS" create secret generic rio-gateway-ssh \
  --from-file=authorized_keys="$SSH_KEY.pub" \
  --dry-run=client -o yaml | kubectl apply -f -

# Restart gateway to pick up the new Secret. Secret updates don't
# propagate to already-mounted volumes without a restart (they do
# for projected volumes, but this is a plain secret mount).
kubectl -n "$NS" rollout restart deployment/rio-gateway
kubectl -n "$NS" rollout status deployment/rio-gateway --timeout=120s

# ----------------------------------------------------------------
# 3. SSM tunnel to the internal NLB
# ----------------------------------------------------------------
BASTION_ID=$(tf bastion_instance_id)
REGION=$(tf region)

# NLB DNS: fetch from the Service status. Wait up to 30×5s = 150s
# for aws-lbc to provision it (usually ~60s for a new NLB).
log "waiting for NLB provisioning"
NLB_DNS=""
retry 30 5 bash -c '
  dns=$(kubectl -n '"$NS"' get svc rio-gateway \
    -o jsonpath="{.status.loadBalancer.ingress[0].hostname}" 2>/dev/null)
  [[ -n "$dns" ]] && echo "$dns"
' > /tmp/nlb-dns || die "NLB never got a hostname (aws-lbc running?)"
NLB_DNS=$(cat /tmp/nlb-dns)
log "NLB: $NLB_DNS"

# Start SSM port-forward in background. The session stays alive
# until we kill it (or it times out at 20min idle, which this
# test won't hit).
log "starting SSM tunnel $BASTION_ID → $NLB_DNS:22 → localhost:$LOCAL_PORT"
aws ssm start-session \
  --region "$REGION" \
  --target "$BASTION_ID" \
  --document-name AWS-StartPortForwardingSessionToRemoteHost \
  --parameters "host=$NLB_DNS,portNumber=22,localPortNumber=$LOCAL_PORT" \
  >/tmp/ssm-session.log 2>&1 &
SSM_PID=$!
trap 'kill $SSM_PID 2>/dev/null || true' EXIT

# Wait for the tunnel to actually forward. `nc -z` only checks that
# session-manager-plugin bound the local socket — the SSM agent on
# the bastion can still fail to reach the NLB (e.g. cross-zone LB off
# + empty-AZ ENI). Read the SSH banner to prove the full path works.
log "waiting for tunnel (reading SSH banner through full path)"
retry 10 3 bash -c '
  banner=$(timeout 3 bash -c "exec 3<>/dev/tcp/localhost/'"$LOCAL_PORT"' && head -c 12 <&3" 2>/dev/null)
  [[ "$banner" == SSH-2.0-* ]]
' || die "SSM tunnel not forwarding — check /tmp/ssm-session.log and NLB target health"

# ----------------------------------------------------------------
# 4. Wait for the chart's default WorkerPool to reconcile
# ----------------------------------------------------------------
# `just eks deploy` applies a default WorkerPool (templates/
# workerpool.yaml) with min=0 max=1000. At rest (no queue) it sits
# at 0 replicas, so we can't wait for Ready yet — instead confirm
# the CR exists and the controller reconciled it (status populated).
POOL=default
log "waiting for WorkerPool/$POOL reconcile"
retry 12 5 bash -c '
  kubectl -n '"$NS"' get workerpool '"$POOL"' \
    -o jsonpath="{.status.desiredReplicas}" 2>/dev/null | grep -q .
' || die "WorkerPool/$POOL not reconciled — check rio-controller logs"

# ----------------------------------------------------------------
# 5. Build nixpkgs#hello through the tunnel
# ----------------------------------------------------------------
# StrictHostKeyChecking=no: the gateway's host key is ephemeral
# (emptyDir in the eks overlay). accept-new would work on first
# run but fail on reruns after a gateway restart.
STORE_URL="ssh-ng://rio@localhost:$LOCAL_PORT?ssh-key=$SSH_KEY"

# First build from a cold cluster: queue goes 0→1 → rio-controller
# scales STS to 1 (next 30s poll) → pod Pending → Karpenter provisions
# a node (~30-60s) → pod Running → build dispatched. ~2min before any
# build output appears.
log "building nixpkgs#hello via $STORE_URL (cold-start: ~2min before first output)"
NIX_SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null" \
  nix build nixpkgs#hello --store "$STORE_URL" --no-link -L \
  || die "hello build failed"
log "hello build OK"

# ----------------------------------------------------------------
# 6. Verify via rio-cli status
# ----------------------------------------------------------------
log "checking cluster status"
kubectl -n "$NS" exec deploy/rio-scheduler -- rio-cli status | tee /tmp/rio-status.log

# Must show at least 1 worker and at least 1 build in the output.
# Exact format is the print_status() in rio-cli/src/main.rs.
grep -q 'worker ' /tmp/rio-status.log || die "no workers in status output"
grep -q 'build ' /tmp/rio-status.log || die "no builds in status output"

# ----------------------------------------------------------------
# 7. Worker-kill reassign test
# ----------------------------------------------------------------
# Baseline the disconnect counter BEFORE the kill (the phase3a G6
# lesson: capture-then-delta, not absolute).
log "capturing disconnect baseline"
DISCONNECTS_BEFORE=$(kubectl -n "$NS" exec deploy/rio-scheduler -- \
  sh -c 'wget -qO- localhost:9091/metrics 2>/dev/null || curl -s localhost:9091/metrics' \
  | grep '^rio_scheduler_worker_disconnects_total' | awk '{print $NF}')
: "${DISCONNECTS_BEFORE:=0}"
log "baseline: $DISCONNECTS_BEFORE"

# Start a longer build in the background. cowsay usually takes
# 30-60s from source.
log "starting background build + worker kill"
NIX_SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null" \
  nix build nixpkgs#cowsay --store "$STORE_URL" --no-link --rebuild &
BUILD_PID=$!

# Wait for the autoscaler to scale to >=2 before killing. With
# min=0, killing the only worker would leave nothing to reassign
# to until Karpenter cold-starts another node (~60s) — which the
# scheduler's reassign timeout might not tolerate. Autoscaler poll
# is 30s; give it 2 cycles.
log "waiting for scale-up to >=2 workers"
retry 12 10 bash -c '
  ready=$(kubectl -n '"$NS"' get workerpool '"$POOL"' \
    -o jsonpath="{.status.readyReplicas}" 2>/dev/null)
  [[ "${ready:-0}" -ge 2 ]]
' || die "WorkerPool never scaled to >=2 ready replicas"

log "killing one worker pod"
VICTIM=$(kubectl -n "$NS" get pod -l "rio.build/pool=$POOL" \
  -o jsonpath='{.items[0].metadata.name}')
kubectl -n "$NS" delete pod "$VICTIM" --wait=false

# Build should still succeed — scheduler reassigns.
wait "$BUILD_PID" || die "build failed after worker kill (no reassign?)"
log "build survived worker kill"

# Verify the disconnect registered.
DISCONNECTS_AFTER=$(kubectl -n "$NS" exec deploy/rio-scheduler -- \
  sh -c 'wget -qO- localhost:9091/metrics 2>/dev/null || curl -s localhost:9091/metrics' \
  | grep '^rio_scheduler_worker_disconnects_total' | awk '{print $NF}')
log "after: $DISCONNECTS_AFTER"
[[ "$DISCONNECTS_AFTER" -gt "$DISCONNECTS_BEFORE" ]] \
  || die "disconnect counter didn't increase (scheduler missed the kill?)"

# ----------------------------------------------------------------
log "SMOKE TEST PASSED"
