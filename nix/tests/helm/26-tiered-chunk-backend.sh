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
#   - per-pod selection (multi-AZ): tiered renders RIO_NODE_ZONE from
#     the POD's topology.kubernetes.io/zone label via the downward API
#     (KEP-4742 PodTopologyLabelsAdmission copies it from the node at
#     binding), and expressBucketByZone renders as ONE JSON env var
#     (RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE) — only when the map
#     is non-empty. The store matches the two at startup; helm never
#     resolves per-pod placement itself.
#   - every express env var is gated on kind, not just on the key being
#     set: leftover express values with kind=s3 must not inject unknown
#     fields into the S3 config variant.
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
for var in RIO_CHUNK_BACKEND__EXPRESS_BUCKET RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE RIO_NODE_ZONE; do
  if grep -qx "$var" <<<"$names"; then
    echo "FAIL: kind=s3 rendered $var" >&2
    exit 1
  fi
done

# ── tiered + expressBucket + zone map: full cache tier ────────────────
tiered=$TMPDIR/cb-tiered.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set store.chunkBackend.kind=tiered \
  --set store.chunkBackend.bucket=rio-chunks \
  --set store.chunkBackend.expressBucket=rio-build-chunk-cache--use2-az1--x-s3 \
  --set-json 'store.chunkBackend.expressBucketByZone={"us-east-2a":"rio-build-chunk-cache--use2-az1--x-s3","us-east-2b":"rio-build-chunk-cache--use2-az2--x-s3"}' \
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
# Per-pod zone comes from the pod's own topology label via the downward
# API — the exact fieldPath is what makes mechanism (c) of P0554 work;
# a typo here degrades every replica to local=None silently.
test "$(yq "$dep | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_NODE_ZONE\") | .valueFrom.fieldRef.fieldPath" "$tiered")" = "metadata.labels['topology.kubernetes.io/zone']" || {
  echo "FAIL: tiered did not render RIO_NODE_ZONE from the pod topology label fieldRef" >&2
  exit 1
}
# The zone map reaches the pod as ONE JSON value (helm toJson sorts
# keys, so the rendered string is deterministic).
test "$(yq "$dep | .spec.template.spec.containers[0].env[] | select(.name==\"RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE\") | .value" "$tiered")" = '{"us-east-2a":"rio-build-chunk-cache--use2-az1--x-s3","us-east-2b":"rio-build-chunk-cache--use2-az2--x-s3"}' || {
  echo "FAIL: tiered + expressBucketByZone did not render the JSON map env var" >&2
  exit 1
}
# tiered is S3-shaped — no PVC, no chunks volume.
if test "$(yq 'select(.kind=="PersistentVolumeClaim" and .metadata.name=="rio-store-chunks") | .metadata.name' "$tiered")" = "rio-store-chunks"; then
  echo "FAIL: kind=tiered rendered the filesystem PVC" >&2
  exit 1
fi

# ── tiered without expressBucket/map: local=None degraded mode ───────
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
# Empty map → no env var (an empty JSON object would be pure noise:
# selection needs a non-empty map anyway).
if grep -qx "RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE" <<<"$names"; then
  echo "FAIL: tiered with empty expressBucketByZone rendered EXPRESS_BUCKET_BY_ZONE" >&2
  exit 1
fi
# The zone var itself is harmless without the map and rides every
# tiered render — the store ignores it unless the map is present.
if ! grep -qx "RIO_NODE_ZONE" <<<"$names"; then
  echo "FAIL: kind=tiered did not render RIO_NODE_ZONE" >&2
  exit 1
fi

# ── kind gate: express values with kind=s3 must not render ───────────
gate=$TMPDIR/cb-gate.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set store.chunkBackend.kind=s3 \
  --set store.chunkBackend.bucket=rio-chunks \
  --set store.chunkBackend.expressBucket=rio-build-chunk-cache--use2-az1--x-s3 \
  --set-json 'store.chunkBackend.expressBucketByZone={"us-east-2a":"rio-build-chunk-cache--use2-az1--x-s3"}' \
  >"$gate"

names=$(yq "$env_names" "$gate")
for var in RIO_CHUNK_BACKEND__EXPRESS_BUCKET RIO_CHUNK_BACKEND__EXPRESS_BUCKET_BY_ZONE RIO_NODE_ZONE; do
  if grep -qx "$var" <<<"$names"; then
    echo "FAIL: kind=s3 + express values rendered $var (env must be gated on kind=tiered)" >&2
    exit 1
  fi
done

echo "OK"
