#!/usr/bin/env bash
#
# run-spike.sh — End-to-end FUSE+overlay+sandbox platform validation
#
# Usage:
#   ./rio-spike/scripts/run-spike.sh [command]
#
# Commands:
#   up         Provision EKS cluster and ECR repository (terraform apply)
#   deploy     Build image, push to ECR, deploy spike pod, collect results
#   down       Destroy all infrastructure (terraform destroy)
#   all        Run up + deploy + down (full lifecycle)
#   status     Show current infrastructure state
#
# Environment:
#   AWS_PROFILE   AWS profile to use (default: beme_sandbox)
#   AWS_REGION    AWS region (default: us-east-2)
#   SKIP_DESTROY  Set to 1 to keep infrastructure after 'all' (for debugging)
#

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SPIKE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
REPO_DIR="$(cd "$SPIKE_DIR/.." && pwd)"
TF_DIR="$SPIKE_DIR/terraform"
K8S_DIR="$SPIKE_DIR/k8s"

export AWS_PROFILE="${AWS_PROFILE:-beme_sandbox}"
export AWS_REGION="${AWS_REGION:-us-east-2}"
SKIP_DESTROY="${SKIP_DESTROY:-0}"

# Use the real kubectl with explicit namespace to avoid wrappers injecting other namespaces
KUBECTL="${KUBECTL:-/usr/bin/kubectl}"
K() { $KUBECTL -n default "$@"; }

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
BLUE='\033[0;34m'
NC='\033[0m'

log()  { echo -e "${BLUE}[spike]${NC} $*"; }
ok()   { echo -e "${GREEN}[spike]${NC} $*"; }
warn() { echo -e "${YELLOW}[spike]${NC} $*"; }
err()  { echo -e "${RED}[spike]${NC} $*" >&2; }

# --- Preflight checks ---

check_deps() {
    local missing=()
    for cmd in aws terraform kubectl nix docker; do
        if ! command -v "$cmd" &>/dev/null; then
            missing+=("$cmd")
        fi
    done
    if [[ ${#missing[@]} -gt 0 ]]; then
        err "missing required tools: ${missing[*]}"
        exit 1
    fi

    # Verify AWS credentials
    if ! aws sts get-caller-identity --profile "$AWS_PROFILE" &>/dev/null; then
        err "AWS credentials not valid for profile '$AWS_PROFILE'"
        err "Run: aws sso login --profile $AWS_PROFILE"
        exit 1
    fi
    ok "AWS credentials valid (profile: $AWS_PROFILE)"
}

# --- Terraform operations ---

tf_init() {
    # Ensure the state bucket exists
    "$SCRIPT_DIR/bootstrap-backend.sh"
    log "initializing Terraform..."
    terraform -chdir="$TF_DIR" init -input=false
}

tf_up() {
    check_deps
    tf_init

    log "provisioning EKS cluster + ECR..."
    terraform -chdir="$TF_DIR" apply \
        -var="region=$AWS_REGION" \
        -auto-approve

    # Update kubeconfig
    local cluster_name
    cluster_name=$(terraform -chdir="$TF_DIR" output -raw cluster_name)
    log "updating kubeconfig for cluster '$cluster_name'..."
    aws eks update-kubeconfig \
        --name "$cluster_name" \
        --region "$AWS_REGION" \
        --profile "$AWS_PROFILE"

    ok "infrastructure provisioned"
    tf_status
}

tf_down() {
    check_deps
    tf_init

    warn "destroying all spike infrastructure..."
    terraform -chdir="$TF_DIR" destroy \
        -var="region=$AWS_REGION" \
        -auto-approve

    ok "infrastructure destroyed"
}

tf_status() {
    if terraform -chdir="$TF_DIR" output cluster_name &>/dev/null; then
        echo ""
        log "--- Infrastructure Status ---"
        echo "  Cluster:  $(terraform -chdir="$TF_DIR" output -raw cluster_name)"
        echo "  Endpoint: $(terraform -chdir="$TF_DIR" output -raw cluster_endpoint)"
        echo "  ECR:      $(terraform -chdir="$TF_DIR" output -raw ecr_repository_url)"
        echo "  Region:   $(terraform -chdir="$TF_DIR" output -raw region)"
        echo ""
    else
        warn "no infrastructure provisioned (run '$0 up' first)"
    fi
}

# --- Image build and push ---

build_and_push_image() {
    local ecr_url
    ecr_url=$(terraform -chdir="$TF_DIR" output -raw ecr_repository_url)
    local region
    region=$(terraform -chdir="$TF_DIR" output -raw region)

    # Build OCI image via Nix
    log "building spike OCI image..." >&2
    nix build "$REPO_DIR#spike-image" --out-link "$SPIKE_DIR/result-spike-image" >&2

    # Load into docker
    log "loading image into docker..." >&2
    docker load < "$SPIKE_DIR/result-spike-image" >&2

    # Tag and push to ECR
    log "authenticating to ECR..." >&2
    aws ecr get-login-password --region "$region" --profile "$AWS_PROFILE" \
        | docker login --username AWS --password-stdin "$ecr_url" >&2

    local image_tag="$ecr_url:latest"
    log "tagging and pushing image to $image_tag..." >&2
    docker tag rio-spike:latest "$image_tag" >&2
    docker push "$image_tag" >&2

    ok "image pushed: $image_tag" >&2
    echo "$image_tag"
}

# --- Deploy and collect results ---

install_seccomp_profile() {
    log "installing seccomp profile on worker nodes..."

    # Get worker node names
    local nodes
    nodes=$(K get nodes -l rio.build/role=spike-worker -o jsonpath='{.items[*].metadata.name}')

    if [[ -z "$nodes" ]]; then
        warn "no spike worker nodes found (are nodes ready?)"
        warn "waiting for nodes..."
        K wait --for=condition=Ready node -l rio.build/role=spike-worker --timeout=300s
        nodes=$(K get nodes -l rio.build/role=spike-worker -o jsonpath='{.items[*].metadata.name}')
    fi

    # The seccomp profile needs to be on the node filesystem.
    # For a spike, we use a ConfigMap + DaemonSet to copy it.

    # Create ConfigMap from the seccomp JSON file
    K create configmap rio-spike-seccomp \
        --from-file=seccomp-spike.json="$K8S_DIR/seccomp-spike.json" \
        --dry-run=client -o yaml | K apply -f -

    # DaemonSet that copies the profile to the node's seccomp directory
    K apply -f - <<'EOF'
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: seccomp-installer
  labels:
    app: seccomp-installer
spec:
  selector:
    matchLabels:
      app: seccomp-installer
  template:
    metadata:
      labels:
        app: seccomp-installer
    spec:
      tolerations:
        - key: rio.build/spike
          operator: Equal
          value: "true"
          effect: NoSchedule
      nodeSelector:
        rio.build/role: spike-worker
      containers:
        - name: installer
          image: busybox
          command: ["sh", "-c", "cp /seccomp/seccomp-spike.json /host-seccomp/ && sleep infinity"]
          volumeMounts:
            - name: seccomp-source
              mountPath: /seccomp
            - name: host-seccomp
              mountPath: /host-seccomp
      volumes:
        - name: seccomp-source
          configMap:
            name: rio-spike-seccomp
        - name: host-seccomp
          hostPath:
            path: /var/lib/kubelet/seccomp
            type: DirectoryOrCreate
EOF

    K rollout status daemonset/seccomp-installer --timeout=120s
    ok "seccomp profile installed"
}

deploy_spike_pod() {
    local image_tag="$1"

    log "deploying spike pod..."

    # Delete any previous run
    K delete pod fuse-overlay-spike --ignore-not-found=true

    # Substitute image placeholder and apply
    sed "s|SPIKE_IMAGE_PLACEHOLDER|$image_tag|" "$K8S_DIR/spike-pod.yaml" \
        | K apply -f -

    ok "spike pod deployed"
}

collect_results() {
    log "waiting for spike pod to complete (timeout: 30m)..."

    # Wait for the pod to finish (either succeed or fail)
    if K wait --for=condition=Ready pod/fuse-overlay-spike --timeout=60s 2>/dev/null; then
        log "pod is running..."
    fi

    # Stream logs while running
    K logs -f fuse-overlay-spike 2>&1 || warn "log streaming ended (pod may have finished or failed)"

    # Wait for completion
    local phase
    local api_failures=0
    for i in $(seq 1 360); do
        if ! phase=$(K get pod fuse-overlay-spike -o jsonpath='{.status.phase}' 2>&1); then
            api_failures=$((api_failures + 1))
            if [[ $api_failures -ge 5 ]]; then
                err "kubectl API unreachable after $api_failures consecutive failures: $phase"
                return 1
            fi
            warn "kubectl get pod failed (attempt $api_failures), retrying..."
            sleep 5
            continue
        fi
        api_failures=0
        case "$phase" in
            Succeeded)
                ok "spike pod completed successfully"
                echo ""
                log "--- Final Pod Logs ---"
                K logs fuse-overlay-spike
                return 0
                ;;
            Failed)
                err "spike pod failed"
                echo ""
                log "--- Pod Logs ---"
                K logs fuse-overlay-spike
                echo ""
                log "--- Pod Events ---"
                K describe pod fuse-overlay-spike | tail -20
                return 1
                ;;
            *)
                sleep 5
                ;;
        esac
    done

    err "spike pod timed out (phase: $phase)"
    K logs fuse-overlay-spike || true
    return 1
}

# --- Main commands ---

cmd_deploy() {
    check_deps

    # Verify infra exists
    if ! terraform -chdir="$TF_DIR" output cluster_name &>/dev/null; then
        err "no infrastructure found. Run '$0 up' first."
        exit 1
    fi

    local image_tag
    image_tag=$(build_and_push_image)
    install_seccomp_profile
    deploy_spike_pod "$image_tag"

    echo ""
    log "============================================"
    log "  FUSE+Overlay+Sandbox Validation Results"
    log "============================================"
    echo ""

    if collect_results; then
        ok "GO: Full validation chain passed"
        echo ""
        ok "Decision: FUSE+overlay architecture is viable on EKS"
    else
        err "NO-GO: Validation failed"
        echo ""
        err "Decision: Investigate failures or activate fallback plan"
        err "  Fallback: bind-mount with nix-store --realise pre-materialization"
        err "  See: docs/src/phases/phase1a.md#fuseoverlay-fallback-plan"
    fi
}

cmd_all() {
    tf_up
    echo ""
    cmd_deploy

    if [[ "$SKIP_DESTROY" == "1" ]]; then
        warn "SKIP_DESTROY=1: infrastructure kept alive for debugging"
        warn "Run '$0 down' when done"
    else
        echo ""
        tf_down
    fi
}

# --- Entry point ---

case "${1:-help}" in
    up)
        tf_up
        ;;
    deploy)
        cmd_deploy
        ;;
    down)
        tf_down
        ;;
    all)
        cmd_all
        ;;
    status)
        tf_status
        ;;
    help|--help|-h)
        echo "Usage: $0 {up|deploy|down|all|status}"
        echo ""
        echo "Commands:"
        echo "  up       Provision EKS cluster + ECR (terraform apply)"
        echo "  deploy   Build image, push, run spike pod, collect results"
        echo "  down     Destroy all infrastructure (terraform destroy)"
        echo "  all      Full lifecycle: up + deploy + down"
        echo "  status   Show infrastructure state"
        echo ""
        echo "Environment:"
        echo "  AWS_PROFILE=$AWS_PROFILE"
        echo "  AWS_REGION=$AWS_REGION"
        echo "  SKIP_DESTROY=$SKIP_DESTROY"
        ;;
    *)
        err "unknown command: $1"
        echo "Run '$0 help' for usage"
        exit 1
        ;;
esac
