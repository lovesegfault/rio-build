# P0579/P0566 binary-cache compat env rendering.
#
# store.binaryCacheCompat drives the RIO_BINARY_CACHE_COMPAT__* env
# block on the rio-store Deployment. The gating rules that must hold:
#   - ENABLED is rendered whenever the key is set — hasKey, not
#     truthiness. rio-store's compiled default is ON, so an explicit
#     `enabled: false` MUST reach the pod as ENABLED=false; a
#     truthiness-style `with`/`if .enabled` skip would silently
#     re-enable compat.
#   - BUCKET is rendered only when non-empty AND enabled. An empty
#     value must render NO env var (Some("") would point the writer at
#     a bucket literally named ""); absent is what deserializes to
#     bucket=None, i.e. "use the chunk backend's bucket".
#   - The companion vars (BUCKET, COMPRESSION) are suppressed when
#     disabled — a disabled deployment renders only ENABLED=false.
#   - Dropping the whole block renders no compat env at all (the
#     compiled default, enabled, applies).

dep='select(.kind=="Deployment" and .metadata.name=="rio-store")'
env_names="$dep | .spec.template.spec.containers[0].env[].name"
env_value() { # $1=file $2=env var name
  yq "$dep | .spec.template.spec.containers[0].env[] | select(.name==\"$2\") | .value" "$1"
}

# ── default profile: enabled=true, zstd, no bucket ───────────────────
base=$TMPDIR/compat-base.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$base"

test "$(env_value "$base" RIO_BINARY_CACHE_COMPAT__ENABLED)" = "true" || {
  echo "FAIL: default profile did not render ENABLED=true" >&2
  exit 1
}
test "$(env_value "$base" RIO_BINARY_CACHE_COMPAT__COMPRESSION)" = "zstd" || {
  echo "FAIL: default profile did not render COMPRESSION=zstd" >&2
  exit 1
}
# Capture-then-grep (not `yq | grep -q`): a producer SIGPIPE under
# pipefail in the `if pipeline; then FAIL` direction silently skips
# the FAIL (the r39 pass-gap shape from 02-monitoring-kinds.sh).
names=$(yq "$env_names" "$base")
if grep -qx "RIO_BINARY_CACHE_COMPAT__BUCKET" <<<"$names"; then
  echo "FAIL: empty bucket rendered RIO_BINARY_CACHE_COMPAT__BUCKET (must be absent for bucket=None)" >&2
  exit 1
fi

# ── explicit enabled=false: hasKey gate + companion suppression ──────
off=$TMPDIR/compat-off.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set store.binaryCacheCompat.enabled=false \
  --set store.binaryCacheCompat.bucket=nix-cache \
  >"$off"

test "$(env_value "$off" RIO_BINARY_CACHE_COMPAT__ENABLED)" = "false" || {
  echo "FAIL: enabled=false must still render ENABLED=false (hasKey, not truthiness)" >&2
  exit 1
}
names=$(yq "$env_names" "$off")
if grep -qx "RIO_BINARY_CACHE_COMPAT__BUCKET" <<<"$names"; then
  echo "FAIL: disabled compat rendered BUCKET (companions must be suppressed when disabled)" >&2
  exit 1
fi
if grep -qx "RIO_BINARY_CACHE_COMPAT__COMPRESSION" <<<"$names"; then
  echo "FAIL: disabled compat rendered COMPRESSION (companions must be suppressed when disabled)" >&2
  exit 1
fi

# ── enabled + dedicated bucket: BUCKET rendered ───────────────────────
bkt=$TMPDIR/compat-bucket.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set store.binaryCacheCompat.bucket=nix-cache \
  --set store.binaryCacheCompat.compression=xz \
  >"$bkt"

test "$(env_value "$bkt" RIO_BINARY_CACHE_COMPAT__BUCKET)" = "nix-cache" || {
  echo "FAIL: enabled + non-empty bucket did not render BUCKET" >&2
  exit 1
}
test "$(env_value "$bkt" RIO_BINARY_CACHE_COMPAT__COMPRESSION)" = "xz" || {
  echo "FAIL: compression=xz did not render through" >&2
  exit 1
}

# ── whole block dropped: no compat env at all ─────────────────────────
none=$TMPDIR/compat-none.yaml
helm template rio . \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set-json 'store.binaryCacheCompat=null' \
  >"$none"

names=$(yq "$env_names" "$none")
if grep -q "RIO_BINARY_CACHE_COMPAT__" <<<"$names"; then
  echo "FAIL: binaryCacheCompat=null still rendered compat env vars (with-gate broken)" >&2
  exit 1
fi

echo "OK"
