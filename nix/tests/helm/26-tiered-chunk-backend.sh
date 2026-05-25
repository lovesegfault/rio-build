# P0554 tiered chunk backend rendering.
#
# kind=tiered turns on the per-AZ S3 Express read-through cache in
# front of the authoritative S3-standard bucket. The shape that must
# hold:
#   - tiered reuses the s3 keys (BUCKET/PREFIX) for the authoritative
#     tier and adds RIO_CHUNK_BACKEND__EXPRESS_BUCKET for the cache
#     tier — and ONLY when expressBucket is non-empty. An empty value
#     must render NO env var (Some("") would point the SDK at a bucket
#     named ""); absent is what deserializes to express_bucket=None,
#     i.e. "degraded to direct S3-standard reads", which is the correct
#     behaviour for a replica in an AZ without Express.
#   - the env var is gated on kind, not just on the key being set: a
#     leftover expressBucket value with kind=s3 must not inject an
#     unknown field into the S3 config variant.
#   - expressBucketByAzId is a values-level landing spot for the
#     terraform output (the per-pod AZ→bucket selection is store-side,
#     not helm-side) and must never leak into the container env.
#   - tiered is an S3-shaped backend: it must not drag in the
#     filesystem PVC.

dep='select(.kind=="Deployment" and .metadata.name=="rio-store")'
env_names="$dep | .spec.template.spec.containers[0].env[].name"

# ── default profile: kind=s3, no express env ─────────────────────────
base=$TMPDIR/cb-base.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$base"

test "$(yq "$dep | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_CHUNK_BACKEND__KIND\") | .value" "$base")" = "s3" || {
  echo "FAIL: default chunkBackend.kind did not render as s3" >&2
  exit 1
}
# Capture-then-grep (not `yq | grep -q`): a producer SIGPIPE under
# pipefail in the `if pipeline; then FAIL` direction silently skips
# the FAIL (the r39 pass-gap shape from 02-monitoring-kinds.sh).
names=$(yq "$env_names" "$base")
if grep -qx "RIO_CHUNK_BACKEND__EXPRESS_BUCKET" <<<"$names"; then
  echo "FAIL: kind=s3 rendered RIO_CHUNK_BACKEND__EXPRESS_BUCKET" >&2
  exit 1
fi

# ── tiered + expressBucket: full cache tier ──────────────────────────
tiered=$TMPDIR/cb-tiered.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set store.chunkBackend.kind=tiered \
  --set store.chunkBackend.bucket=rio-chunks \
  --set store.chunkBackend.expressBucket=rio-build-chunk-cache--use2-az1--x-s3 \
  --set-json 'store.chunkBackend.expressBucketByAzId={"use2-az1":"rio-build-chunk-cache--use2-az1--x-s3"}' \
  >"$tiered"

test "$(yq "$dep | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_CHUNK_BACKEND__KIND\") | .value" "$tiered")" = "tiered" || {
  echo "FAIL: chunkBackend.kind=tiered did not render KIND=tiered" >&2
  exit 1
}
test "$(yq "$dep | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_CHUNK_BACKEND__BUCKET\") | .value" "$tiered")" = "rio-chunks" || {
  echo "FAIL: tiered did not render the authoritative BUCKET" >&2
  exit 1
}
test "$(yq "$dep | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_CHUNK_BACKEND__EXPRESS_BUCKET\") | .value" "$tiered")" = "rio-build-chunk-cache--use2-az1--x-s3" || {
  echo "FAIL: tiered + expressBucket did not render EXPRESS_BUCKET" >&2
  exit 1
}
# The map is values-only plumbing for the terraform output; nothing in
# the pod spec may carry it (there is no config field for it yet).
if grep -q "expressBucketByAzId" "$tiered"; then
  echo "FAIL: expressBucketByAzId leaked into rendered manifests" >&2
  exit 1
fi
# tiered is S3-shaped — no PVC, no chunks volume.
if test "$(yq 'select(.kind=="PersistentVolumeClaim" and .metadata.name=="rio-store-chunks") | .metadata.name' "$tiered")" = "rio-store-chunks"; then
  echo "FAIL: kind=tiered rendered the filesystem PVC" >&2
  exit 1
fi

# ── tiered without expressBucket: local=None degraded mode ───────────
deg=$TMPDIR/cb-degraded.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set store.chunkBackend.kind=tiered \
  --set store.chunkBackend.bucket=rio-chunks \
  >"$deg"

test "$(yq "$dep | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_CHUNK_BACKEND__KIND\") | .value" "$deg")" = "tiered" || {
  echo "FAIL: tiered without expressBucket did not render KIND=tiered" >&2
  exit 1
}
names=$(yq "$env_names" "$deg")
if grep -qx "RIO_CHUNK_BACKEND__EXPRESS_BUCKET" <<<"$names"; then
  echo "FAIL: tiered with empty expressBucket rendered EXPRESS_BUCKET (must be absent for express_bucket=None)" >&2
  exit 1
fi

# ── kind gate: expressBucket with kind=s3 must not render ────────────
gate=$TMPDIR/cb-gate.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set store.chunkBackend.kind=s3 \
  --set store.chunkBackend.bucket=rio-chunks \
  --set store.chunkBackend.expressBucket=rio-build-chunk-cache--use2-az1--x-s3 \
  >"$gate"

names=$(yq "$env_names" "$gate")
if grep -qx "RIO_CHUNK_BACKEND__EXPRESS_BUCKET" <<<"$names"; then
  echo "FAIL: kind=s3 + expressBucket rendered EXPRESS_BUCKET (env must be gated on kind=tiered)" >&2
  exit 1
fi

echo "OK"
