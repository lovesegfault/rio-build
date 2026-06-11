# M1 (owner, 2026-06-11, SIGNED; bughunt-9 W9-BQ): metal joins spot+od.
# The spot+od doctrine ("ALL CLASSES spot+on-demand — spot if
# available, else od") had ONE divergence left: the metal classes'
# od-only carve-out, an interruption-economics judgment the B-0 audit
# showed was not market-derived (spot EXISTS for 21/21 sampled
# admitted metal types, all three AZs, ~70% od discount). The owner
# attacked it; this fragment pins the chart to the doctrine:
#
#   (i)  every metal hwClass declares capacityTypes [spot, on-demand]
#        (spot FIRST — the walk is capacity-major, spot band before od);
#   (ii) every metal class×capacity cell has a leadTimeSeed row (the
#        ladder rows parse — fragment 18 asserts the universal law;
#        this pins the metal:spot rows by name so the doctrine flip
#        cannot land seedless).
#
# Pre-fix RED (the shipped truth, quoted in the landing commit): both
# metal classes declared [on-demand] and no metal-*:spot seed existed.

out=$TMPDIR/metal-doctrine.yaml
helm template rio . --set global.image.tag=test >"$out"

sched_toml=$(yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
                    | .data."scheduler.toml"' "$out")
test -n "$sched_toml" || {
  echo "FAIL: rio-scheduler-config did not render — doctrine assertion vacuous" >&2
  exit 1
}

for h in metal-x86 metal-arm; do
  block=$(printf '%s\n' "$sched_toml" | awk -v h="$h" '
    $0 == "[sla.hw_classes.\"" h "\"]" { in_h=1; next }
    in_h && /^\[/ { exit }
    in_h { print }
  ')
  test -n "$block" || { echo "FAIL: scheduler.toml missing hw_classes.$h" >&2; exit 1; }
  echo "$block" | grep -q 'capacity_types = \["spot","on-demand"\]' || {
    echo "FAIL: $h does not declare capacity_types=[spot, on-demand] — the M1 doctrine flip is not rendered" >&2
    exit 1
  }
  for cap in spot od; do
    printf '%s\n' "$sched_toml" | grep -q "\"$h:$cap\"" || {
      echo "FAIL: lead-time seed row \"$h:$cap\" missing — an unseeded cell is structurally refused (helm/18); the doctrine flip must land WITH its ladder rows" >&2
      exit 1
    }
  done
done

echo "OK: metal-x86/metal-arm declare [spot, on-demand] with seeded spot+od ladder rows"
