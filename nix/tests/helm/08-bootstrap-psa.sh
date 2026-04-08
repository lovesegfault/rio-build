# r[sec.psa.control-plane-restricted] bootstrap-job.
#
# P0460 missed bootstrap-job.yaml (default-off → CI never rendered it).
# First prod install with bootstrap.enabled=true failed PSA admission
# at the pre-install hook. Assert both pod- and container-level
# securityContext render so future helper drift can't re-break it.

out=$TMPDIR/bootstrap-on.yaml
helm template rio . \
  --set bootstrap.enabled=true \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$out"

yq 'select(.kind=="Job" and .metadata.name=="rio-bootstrap")
    | .spec.template.spec.securityContext.runAsNonRoot' "$out" |
  grep -x true >/dev/null || {
  echo "FAIL: rio-bootstrap Job pod securityContext.runAsNonRoot != true" >&2
  exit 1
}
yq 'select(.kind=="Job" and .metadata.name=="rio-bootstrap")
    | .spec.template.spec.containers[0].securityContext.capabilities.drop[0]' "$out" |
  grep -x ALL >/dev/null || {
  echo "FAIL: rio-bootstrap Job container securityContext.capabilities.drop[0] != ALL" >&2
  exit 1
}
