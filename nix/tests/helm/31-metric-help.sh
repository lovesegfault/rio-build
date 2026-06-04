# Rendered alert semantics open with the canonical describe_*! HELP
# (bug_330). The PrometheusRule's RioSchedulerMaterializationStalled
# description is `{{ get ((.Files.Get "generated/metric-help.json") |
# fromJson) "<name>" }}` + hand-written ACTION text — this fragment
# proves the Files.Get indirection renders the json entry verbatim
# (a garbled file or a dropped key renders empty/garbage and fails
# here; helm-obs-drift separately proves the json matches the code).
#
# (documentary — .sh is not tracey-scanned.)

out=$TMPDIR/metric-help-render.yaml
helm template rio . --set global.image.tag=test --set monitoring.enabled=true >"$out"

want=$(jq -r '."rio_scheduler_materialization_stalled"' generated/metric-help.json)
test -n "$want" && test "$want" != "null" || {
  echo "FAIL: generated/metric-help.json lacks rio_scheduler_materialization_stalled — run 'cargo xtask regen helm-obs'" >&2
  exit 1
}

desc=$(yq -N 'select(.kind=="PrometheusRule") | .spec.groups[].rules[] | select(.alert=="RioSchedulerMaterializationStalled") | .annotations.description' "$out")
test -n "$desc" || {
  echo "FAIL: RioSchedulerMaterializationStalled rendered no description" >&2
  exit 1
}

case "$desc" in
  "$want"*) ;;
  *)
    echo "FAIL: rendered description does not OPEN with the canonical HELP sentence" >&2
    echo "  want prefix: $want" >&2
    echo "  rendered:    $desc" >&2
    exit 1
    ;;
esac
