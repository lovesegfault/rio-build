# sh-016 (c): every hwClass referenced by ANY `ladder.rungs[].class` MUST
# itself declare `ladder` (even if `rungs: []` to mark an intentional
# terminal). A rung target with no `ladder` key dead-ends the BFS at
# `retain_hosting_cells`' `else { continue }` — when the τ-band collapsed
# to a single dead-end leaf (the production hi-nvme-x86-g7 monoculture),
# the all-masked widen had nowhere to go.
#
# Checks the rendered scheduler.toml (parsed as TOML via yq): every class
# named as a rung target must have a non-null `.ladder` key in its
# `[sla.hw_classes."NAME"]` block. `ladder = { rungs = [] }` (the terminal
# sentinel) satisfies this; `validate_shape` accepts empty rungs.

check_render() {
  local label="$1"; shift
  helm template rio . --set global.image.tag=test "$@" 2>/dev/null \
    | yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
             | .data."scheduler.toml"' \
    > "$TMPDIR/sched-$label.toml"
  # Rung-target set: every `.ladder.rungs[].class` across all classes.
  targets=$(yq -p toml -o json '.sla.hw_classes' "$TMPDIR/sched-$label.toml" \
    | jq -r 'to_entries[] | .value.ladder.rungs // [] | .[].class' \
    | sort -u)
  # Classes that DECLARE `.ladder` (key present, even if rungs empty).
  declared=$(yq -p toml -o json '.sla.hw_classes' "$TMPDIR/sched-$label.toml" \
    | jq -r 'to_entries[] | select(.value | has("ladder")) | .key' \
    | sort -u)
  bad=$(comm -23 <(echo "$targets") <(echo "$declared"))
  if [ -n "$bad" ]; then
    echo "FAIL ($label): hwClasses referenced as ladder.rungs[].class but" >&2
    echo "  declaring NO ladder of their own (dead-end leaf — sh-016):" >&2
    echo "$bad" | sed 's/^/    /' >&2
    echo "  Add 'ladder: {rungs: [...]}' (or '{rungs: []}' for an intentional" >&2
    echo "  terminal) to each in scheduler.sla.hwClasses." >&2
    return 1
  fi
}

check_render prod \
  --set karpenter.enabled=true --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role --set karpenter.amiTag=test \
  --set postgresql.enabled=false
check_render vmtest-full -f values/vmtest-full.yaml

# Self-check: a synthetic rung target with NO ladder MUST be flagged.
# Inject a phantom rung onto hi-ebs-x86 pointing at lo-nvme-x86 (which
# carries no ladder) and assert the check fails.
if check_render bad-dead-end \
  --set karpenter.enabled=true --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role --set karpenter.amiTag=test \
  --set postgresql.enabled=false \
  --set 'scheduler.sla.hwClasses.hi-ebs-x86.ladder.rungs[0].class=lo-nvme-x86' \
  2>/dev/null; then
  echo "FAIL: dead-end rung target lo-nvme-x86 not flagged — fragment regression" >&2
  exit 1
fi
