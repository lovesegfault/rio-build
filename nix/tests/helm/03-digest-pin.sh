# Third-party image digest-pin enforcement.
#
# Every image that isn't a rio-build image (those get `:test` from
# --set global.image.tag=test) MUST be digest-pinned. A floating
# third-party tag that doesn't exist / gets deleted / gets overwritten
# upstream → ImagePullBackOff → component-specific silent brick.
#
# yq drills into every container spec (DaemonSet + Deployment +
# StatefulSet + Job). Runs over BOTH default (prod profile) and dash-on
# (dashboard-enabled superset). Filters out :test-tagged rio images and
# fails on any remaining bare-tag image. @sha256: is the pin marker.
# Subchart images (bitnami/ PG) are a separate supply-chain boundary —
# postgresql.enabled=false in both renders so they don't appear here.
#
# Cilium's per-Gateway envoy image is NOT chart-side (cilium-operator
# spawns it from its own config); the digest-pin for it lives in
# nix/docker-pulled.nix cilium-envoy.

default=$TMPDIR/digest-default.yaml
dash=$TMPDIR/digest-dash-on.yaml
helm template rio . --set global.image.tag=test >"$default"
helm template rio . \
  --set dashboard.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$dash"

thirdparty=$(yq eval-all '
  select(.kind=="DaemonSet" or .kind=="Deployment"
         or .kind=="StatefulSet" or .kind=="Job")
  | .spec.template.spec.containers[].image
' "$default" "$dash" |
  { grep -Ev ':test$|^---$|^null$' || true; } |
  sort -u)
echo "third-party images in default+dash-on renders:" >&2
echo "$thirdparty" >&2
bad=$(echo "$thirdparty" | grep -v '@sha256:' || true)
if [ -n "$bad" ]; then
  echo "FAIL: third-party image(s) not digest-pinned:" >&2
  echo "$bad" >&2
  exit 1
fi
