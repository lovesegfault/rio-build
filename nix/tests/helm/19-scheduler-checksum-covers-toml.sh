# checksum/scheduler-toml MUST change when ANY input that lands in the
# rendered scheduler.toml changes — not just `.scheduler.softFeatures` /
# `.scheduler.sla`. bug_021: §13c-2 added `metal_sizes = {{ toJson
# $.Values.karpenter.metalSizes }}` to the TOML body, but the checksum
# still hashed only the `$s.softFeatures`/`$s.sla` subtrees — a
# metalSizes bump updated the ConfigMap without rolling the
# (subPath-mounted, read-once-at-boot) scheduler pod, so scheduler and
# controller diverged on the metal-partition predicate. Sibling of
# bug_022 / helm/16 (controller.yaml had the identical drift). Fix:
# hash the rendered `rio.schedulerToml` body via a named template.

render() {
  helm template rio . \
    --set karpenter.enabled=true \
    --set karpenter.clusterName=ci \
    --set karpenter.nodeRoleName=ci-role \
    --set karpenter.amiTag=test \
    --set global.image.tag=test \
    --set postgresql.enabled=false \
    "$@"
}

checksum_of() {
  yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-scheduler")
         | .spec.template.metadata.annotations."checksum/scheduler-toml"' "$@"
}

render_checksum() {
  render "$@" | checksum_of
}

base=$(render_checksum)
test -n "$base" || { echo "FAIL: checksum/scheduler-toml absent" >&2; exit 1; }

# Each axis below maps to one TOML key; changing it MUST change the hash.
# `karpenter.metalSizes` is the bug_021 case: outside `.Values.scheduler`
# but rendered into the TOML body. The other axes are belt-and-suspenders
# (already inside `$s.sla`/`$s.softFeatures`, the old key-list covered
# them) — pinning them keeps the test load-bearing if the named template
# is later split or partially inlined.
for axis in \
  'karpenter.metalSizes={metal,metal-zz}' \
  "scheduler.sla.maxFleetCores=12345" \
  "scheduler.sla.referenceHwClass=other-ref" \
  "scheduler.softFeatures={zz-soft-feature}"
do
  got=$(render_checksum --set "$axis")
  test "$got" != "$base" || {
    echo "FAIL: checksum/scheduler-toml unchanged after --set $axis" >&2
    echo "  (rendered scheduler.toml input changed but checksum did not roll the pod)" >&2
    exit 1
  }
done

# §one-step-removed (a) inverse: a helm input that does NOT land in the
# TOML must NOT roll the pod. The named template must be ONLY the TOML —
# if it accidentally includes a Deployment field, every replica/image
# bump rolls the scheduler for nothing.
base_render=$TMPDIR/scheduler-checksum-base.yaml
repl_render=$TMPDIR/scheduler-checksum-replicas.yaml
render >"$base_render"
render --set "scheduler.replicas=3" >"$repl_render"
# Control first: the perturbation must actually change the render.
# scheduler.replicas lands today (scheduler.yaml emits a static replica
# count), but if the scheduler ever grows an autoscaling gate that makes
# it a render no-op, the equality below would hold for ANY checksum
# definition — exactly how helm/26's gateway.replicas inverse went
# vacuous. Fail loudly instead of proving nothing.
if cmp -s "$base_render" "$repl_render"; then
  echo "FAIL: scheduler.replicas=3 left the render byte-identical — the inverse" >&2
  echo "  check below is vacuous; pick a perturbation that lands in the Deployment" >&2
  exit 1
fi
test "$(checksum_of "$repl_render")" = "$base" || {
  echo "FAIL: checksum/scheduler-toml changed on scheduler.replicas (non-TOML input)" >&2
  echo "  (named template must render only scheduler.toml, not Deployment fields)" >&2
  exit 1
}
