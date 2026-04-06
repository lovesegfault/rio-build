#!/usr/bin/env nix-shell
#!nix-shell -i bash -p awscli2 kubectl openssh
# shellcheck shell=bash
#
# NOTE: `nix` intentionally NOT in -p — nixpkgs' nix (2.31.x) hangs on
# ssh-ng remote stores after eval. The system's patched nix on PATH works.
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

# kubectl exec deploy/rio-scheduler picks an arbitrary pod. With
# 2 replicas + leader election, the standby responds "not leader"
# to admin RPCs. Find the leader from the Lease and exec there.
sched_leader() {
  kubectl -n "$NS" get lease rio-scheduler-leader \
    -o jsonpath='{.spec.holderIdentity}' 2>/dev/null
}
sched_exec() {
  local leader
  leader=$(sched_leader) || die "no scheduler lease found"
  [[ -n "$leader" ]] || die "scheduler lease has no holder"
  kubectl -n "$NS" exec "$leader" -- "$@"
}

# Scheduler container is a minimal image (no sh/wget/curl). For
# metrics, port-forward to the leader pod and curl from here.
sched_metric() {
  local leader pf_pid metric
  leader=$(sched_leader) || die "no scheduler lease found"
  kubectl -n "$NS" port-forward "$leader" 19091:9091 >/dev/null 2>&1 &
  pf_pid=$!
  # shellcheck disable=SC2064
  trap "kill $pf_pid 2>/dev/null; wait $pf_pid 2>/dev/null || true" RETURN
  sleep 1
  metric=$(curl -s localhost:19091/metrics 2>/dev/null | grep "^$1 " | awk '{print $NF}')
  echo "${metric:-0}"
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
if ! sched_exec rio-cli create-tenant "$TENANT" 2>&1 | tee /tmp/rio-cli-create.log; then
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

# Restart gateway to pick up the new Secret. authorized_keys is loaded
# once at startup into Arc<Vec<PublicKey>> (server.rs:96) — no hot-reload.
kubectl -n "$NS" rollout restart deployment/rio-gateway
kubectl -n "$NS" rollout status deployment/rio-gateway --timeout=120s

# rollout status returns when k8s readiness passes, but NLB target
# registration + health-checking is a separate ~30-90s cycle. During
# that window, old targets are draining (no NEW connections) and new
# targets are still `initial`. Starting the SSM tunnel there means the
# bastion's SSM agent hits "Connection to destination port failed" and
# session-manager-plugin leaves the local socket bound but useless.
#
# Wait for the new pod IPs to be NLB-healthy before starting the tunnel.
log "waiting for NLB target health (new gateway pods)"
TG_ARN=$(aws elbv2 describe-target-groups --region "$(tf region)" \
  --query "TargetGroups[?contains(TargetGroupName, 'rio')].TargetGroupArn" --output text)
# Filter out Terminating pods — status.phase stays Running until the
# container exits; deletion is indicated by metadata.deletionTimestamp.
# kubectl field-selectors can't match on deletionTimestamp, so use a
# go-template. Without this filter, the 90s retry times out waiting
# for draining old-rev pods that will never register healthy.
want_ips=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=rio-gateway \
  -o go-template='{{range .items}}{{if not .metadata.deletionTimestamp}}{{.status.podIP}} {{end}}{{end}}')
retry 30 3 bash -c '
  healthy_ips=$(aws elbv2 describe-target-health --region '"$(tf region)"' \
    --target-group-arn '"$TG_ARN"' \
    --query "TargetHealthDescriptions[?TargetHealth.State=='\''healthy'\''].Target.Id" --output text)
  for ip in '"$want_ips"'; do
    grep -qw "$ip" <<<"$healthy_ips" || exit 1
  done
' || die "new gateway pod IPs never became NLB-healthy: want=$want_ips"

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
# 5. Build a trivial derivation through the tunnel
# ----------------------------------------------------------------
# StrictHostKeyChecking=no: the gateway's host key is ephemeral
# (emptyDir in the eks overlay). accept-new would work on first
# run but fail on reruns after a gateway restart.
STORE_URL="ssh-ng://rio@localhost:$LOCAL_PORT?ssh-key=$SSH_KEY"
export NIX_SSHOPTS="-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null"

# Minimal self-contained derivation: busybox FOD (builtin:fetchurl)
# + a raw derivation using it as builder. 2-path closure total — no
# stdenv, no shared DAG nodes with other builds (avoids poison cascade
# from stale scheduler state). The busybox FOD is deterministic
# (content-addressed) so it's a one-time build; subsequent runs only
# copy the timestamped smoke .drv.
#
# `${toString builtins.currentTime}` in the name → unique .drv each
# run → never cache-hit, always exercises the full build chain.
#
# Cold-start: queue 0→1 → rio-controller scales STS to 1 (next 30s
# poll) → pod Pending → Karpenter provisions node (~30-60s) → pod
# Running → build dispatched. ~2-3min total on a fresh cluster.
# Args: tag sleep_secs. `sleep_secs` controls build duration — the
# worker-kill test needs a build that outlasts the scale-up wait.
SMOKE_EXPR='
let
  busybox = builtins.derivation {
    name = "busybox";
    builder = "builtin:fetchurl";
    system = "builtin";
    url = "http://tarballs.nixos.org/stdenv/x86_64-unknown-linux-gnu/82b583ba2ba2e5706b35dbe23f31362e62be2a9d/busybox";
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-QrTEnQTBM1Y/qV9odq8irZkQSD9uOMbs2Q5NgCvKCNQ=";
    executable = true;
    unpack = false;
  };
in builtins.derivation {
  name = "rio-smoke-@TAG@-${toString builtins.currentTime}";
  system = "x86_64-linux";
  builder = "${busybox}";
  # Bootstrap busybox is ultra-minimal (sh/ash/mkdir only, no sleep,
  # no touch, no $((arith))). `read -t N < /dev/zero` times out after
  # N seconds (Nix sandbox provides /dev/zero). `echo > $out` creates
  # the output via shell redirect.
  args = ["sh" "-c" "echo @TAG@; read -t @SECS@ x < /dev/zero || true; echo ok > $out"];
}'

smoke_build() {
  local tag="$1" secs="$2"
  local expr="${SMOKE_EXPR//@TAG@/$tag}"
  expr="${expr//@SECS@/$secs}"
  local drv
  drv=$(nix-instantiate --expr "$expr" 2>/dev/null)
  [[ -n "$drv" ]] || return 1
  log "  copying closure for $(basename "$drv")"
  nix copy --to "$STORE_URL" --derivation "$drv" || return 1
  log "  building"
  nix build --store "$STORE_URL" --no-link --print-out-paths "$drv^*"
}

log "building trivial derivation via $STORE_URL (cold-start: ~2-3min)"
smoke_build fast 5 || die "trivial build failed"
log "trivial build OK"

# ----------------------------------------------------------------
# 6. Verify via rio-cli status
# ----------------------------------------------------------------
log "checking cluster status"
sched_exec rio-cli status | tee /tmp/rio-status.log

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
DISCONNECTS_BEFORE=$(sched_metric rio_scheduler_worker_disconnects_total)
log "baseline: $DISCONNECTS_BEFORE"

# Start a longer build in the background. 180s sleep — the scale-up
# wait below can take up to 120s (retry 12 10), plus time for the
# kill + reassign dispatch cycle.
log "starting background build + worker kill"
smoke_build slow 180 &
BUILD_PID=$!

# Wait for >=2 ready workers before killing. WorkerPool min=2 in
# values.yaml guarantees this; the wait is just for Karpenter to
# finish provisioning the second node if we're cold.
log "waiting for >=2 ready workers"
retry 18 10 bash -c '
  ready=$(kubectl -n '"$NS"' get workerpool '"$POOL"' \
    -o jsonpath="{.status.readyReplicas}" 2>/dev/null)
  [[ "${ready:-0}" -ge 2 ]]
' || die "WorkerPool never reached >=2 ready replicas (min=2 not applied?)"

log "killing one worker pod"
VICTIM=$(kubectl -n "$NS" get pod -l "rio.build/pool=$POOL" \
  -o jsonpath='{.items[0].metadata.name}')
kubectl -n "$NS" delete pod "$VICTIM" --wait=false

# Build should still succeed — scheduler reassigns.
wait "$BUILD_PID" || die "build failed after worker kill (no reassign?)"
log "build survived worker kill"

# Verify the disconnect registered.
DISCONNECTS_AFTER=$(sched_metric rio_scheduler_worker_disconnects_total)
log "after: $DISCONNECTS_AFTER"
[[ "$DISCONNECTS_AFTER" -gt "$DISCONNECTS_BEFORE" ]] \
  || die "disconnect counter didn't increase (scheduler missed the kill?)"

# ----------------------------------------------------------------
log "SMOKE TEST PASSED"
