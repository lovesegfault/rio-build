# rio-migrate Job (migrate-job.yaml) is the ONLY migration runner —
# app pods just verify the schema at startup. It is a PLAIN Job (the
# hook lifecycle was removed deliberately; see the template header).
# Guard the contract:
# - the Job renders in every profile (a profile without it deploys a
#   cluster that can never converge), selected by LABEL — the name
#   carries a per-render pod-template hash;
# - name scheme: rio-migrate-<8 hex of sha256(rendered pod template)>;
# - helm.sh/resource-policy: keep (without it helm prunes the
#   old-named Job mid-run on upgrade) + ttlSecondsAfterFinished;
# - dedicated SA with automountServiceAccountToken: false;
# - NO helm.sh/hook annotations anywhere on it;
# - non-EKS (k3s/dev) renders carry ZERO ssl/cert material;
# - the EKS render mounts the vendored RDS CA bundle and carries the
#   rio-migrate-egress CNP;
# - the vendored bundle matches its own header sha256 pin.
#
# postgresql.enabled=false for the k3s-shaped renders: the bitnami
# subchart's own rendered config mentions ssl and would false-positive
# the grep; the constraint is about THIS chart's templates.

render_dev=$(helm template rio . -f values/dev.yaml --set postgresql.enabled=false)
render_vm=$(helm template rio . -f values/vmtest-full.yaml --set postgresql.enabled=false)
render_eks=$(helm template rio . --set global.image.tag=test \
  --set externalSecrets.enabled=true \
  --set externalSecrets.auroraEndpoint=db.example.invalid \
  --set externalSecrets.auroraSecretArn=arn:aws:secretsmanager:eu:1:secret:x \
  --set postgres.authMode=iam)

migrate_job() {
  printf '%s\n' "$1" \
    | yq 'select(.kind=="Job" and .metadata.labels."app.kubernetes.io/name"=="rio-migrate")'
}

job_present() {
  local name
  name=$(migrate_job "$2" | yq '.metadata.name')
  case "$name" in
    rio-migrate-[0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f][0-9a-f]) ;;
    *)
      echo "FAIL: $1 render: rio-migrate Job missing or bad name scheme (got '$name')" >&2
      exit 1
      ;;
  esac
}
job_present dev "$render_dev"
job_present vmtest "$render_vm"
job_present eks "$render_eks"

# Lifecycle contract on the EKS render (same template in all
# profiles): resource-policy keep, TTL, SA + automount off.
job=$(migrate_job "$render_eks")
[ "$(printf '%s\n' "$job" | yq '.metadata.annotations."helm.sh/resource-policy"')" = keep ] || {
  echo "FAIL: rio-migrate Job lacks helm.sh/resource-policy: keep (helm would prune the old-named Job mid-run on upgrade)" >&2
  exit 1
}
[ "$(printf '%s\n' "$job" | yq '.spec.ttlSecondsAfterFinished')" = 3600 ] || {
  echo "FAIL: rio-migrate Job lacks ttlSecondsAfterFinished (resource-policy keep means helm never reaps it)" >&2
  exit 1
}
[ "$(printf '%s\n' "$job" | yq '.spec.template.spec.serviceAccountName')" = rio-migrate ] || {
  echo "FAIL: rio-migrate Job must run under the dedicated rio-migrate SA" >&2
  exit 1
}
[ "$(printf '%s\n' "$job" | yq '.spec.template.spec.automountServiceAccountToken')" = false ] || {
  echo "FAIL: rio-migrate Job must not automount a service account token" >&2
  exit 1
}

# Negative: the Job carries NO hook machinery. (Job-scoped grep — the
# bootstrap Job is a real hook and must stay one.)
if printf '%s\n' "$job" | grep -q 'helm.sh/hook'; then
  echo "FAIL: rio-migrate Job carries helm.sh/hook annotations — it must be a plain Job" >&2
  exit 1
fi

no_certs() {
  if printf '%s\n' "$2" | grep -qiE 'sslmode|sslrootcert|rds-ca|global-bundle'; then
    echo "FAIL: $1 render contains ssl/cert references (must be EKS-only)" >&2
    printf '%s\n' "$2" | grep -inE 'sslmode|sslrootcert|rds-ca|global-bundle' >&2
    exit 1
  fi
}
no_certs dev "$render_dev"
no_certs vmtest "$render_vm"

printf '%s\n' "$job" \
  | yq '.spec.template.spec.containers[0].volumeMounts[].name' \
  | grep -qx rds-ca || {
  echo "FAIL: EKS rio-migrate Job lacks the rds-ca mount" >&2
  exit 1
}

# The migrate egress CNP renders on the EKS profile (networkPolicy
# enabled in base values) and selects by the stable name label.
sel=$(printf '%s\n' "$render_eks" \
  | yq 'select(.kind=="CiliumNetworkPolicy" and .metadata.name=="rio-migrate-egress") | .spec.endpointSelector.matchLabels."app.kubernetes.io/name"')
[ "$sel" = rio-migrate ] || {
  echo "FAIL: rio-migrate-egress CNP missing from EKS render or selector wrong (got '$sel')" >&2
  exit 1
}

# Vendored RDS bundle integrity: recompute the body hash and compare
# against the pin in the 6-line header. Catches a re-vendored bundle
# whose header pin wasn't updated (or a corrupted/truncated bundle).
pinned=$(sed -n '4s/.*sha256 (of the bundle below this 6-line header): //p' files/rds-global-bundle.pem)
actual=$(tail -n +7 files/rds-global-bundle.pem | sha256sum | cut -d' ' -f1)
[ -n "$pinned" ] && [ "$pinned" = "$actual" ] || {
  echo "FAIL: rds-global-bundle.pem sha256 mismatch (header pin '$pinned' vs actual '$actual')" >&2
  exit 1
}
