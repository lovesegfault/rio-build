# Every `[sla.hw_classes.*]` entry rendered into scheduler.toml MUST carry
# a non-empty `requirements` list — `SlaConfig::validate` (config.rs)
# rejects an empty one and the scheduler crash-loops. bug_044: the
# vmtest-full.yaml overlay defined `hwClasses.vmtest` with `labels` only;
# helm rendered `requirements = []` and 14 k3sFull VM scenarios timed out
# on scheduler boot. Static catch here at helm-lint, not at runtime.
#
# Checks both the prod chart defaults AND the vmtest-full overlay (the
# overlay merges atop prod values, so a missing key falls through to []).

check_render() {
  local label="$1"; shift
  helm template rio . --set global.image.tag=test "$@" 2>/dev/null \
    | yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
             | .data."scheduler.toml"' \
    > "$TMPDIR/sched-$label.toml"
  # Find every `[sla.hw_classes."NAME"]` followed within the block by an
  # empty `requirements = [` `]` (the gotmpl renders the open-bracket on
  # one line, range body fills it, close-bracket on the next when empty).
  bad=$(awk '
    /^\[sla\.hw_classes\./ { h=$0; sub(/.*"/,"",h); sub(/".*/,"",h) }
    /^requirements = \[$/   { empty=1; next }
    empty && /^\]$/         { print h; empty=0; next }
    empty                   { empty=0 }
  ' "$TMPDIR/sched-$label.toml")
  if [ -n "$bad" ]; then
    echo "FAIL ($label): hwClasses with empty requirements: $bad" >&2
    echo "  SlaConfig::validate rejects this — scheduler crash-loops at boot" >&2
    return 1
  fi
}

check_render prod \
  --set karpenter.enabled=true --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role --set karpenter.amiTag=test \
  --set postgresql.enabled=false
check_render vmtest-full -f values/vmtest-full.yaml
