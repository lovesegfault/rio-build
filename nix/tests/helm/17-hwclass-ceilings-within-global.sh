# Every `[sla.hw_classes.*]` entry's per-class `max_cores`/`max_mem` MUST
# be in [1, sla.maxCores] / [1, sla.maxMem] — `SlaConfig::validate`
# rejects otherwise and the scheduler crash-loops at boot. The per-class
# values are the configured catalog ceiling for `evaluate_cell`'s
# `ClassCeiling` gate (what Karpenter is PERMITTED to launch for the
# class, as distinct from `CostTable.cells` which is what it has been
# OBSERVED launching). Static catch here at helm-lint, not at runtime.

check_render() {
  local label="$1"
  shift
  helm template rio . --set global.image.tag=test "$@" 2>/dev/null \
    | yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
             | .data."scheduler.toml"' \
      >"$TMPDIR/sched-$label.toml"
  # awk: extract global max_cores/max_mem (under [sla], before any
  # [sla.hw_classes.*] section), then for each hw_classes block check
  # 1 ≤ per-class ≤ global on both axes.
  bad=$(awk '
    /^\[sla\]/                 { in_sla=1 }
    /^\[sla\./                 { in_sla=0 }
    in_sla && /^max_cores = /  { gmc=$3+0 }
    in_sla && /^max_mem = /    { gmm=$3+0 }
    /^\[sla\.hw_classes\./     { h=$0; sub(/.*"/,"",h); sub(/".*/,"",h); cmc=0; cmm=0 }
    h && /^max_cores = /       { cmc=$3+0 }
    h && /^max_mem = /         { cmm=$3+0;
      if (cmc<1 || cmc>gmc) printf "%s.max_cores=%d not in [1,%d]\n", h, cmc, gmc
      if (cmm<1 || cmm>gmm) printf "%s.max_mem=%d not in [1,%d]\n", h, cmm, gmm
      h=""
    }
  ' "$TMPDIR/sched-$label.toml")
  if [ -n "$bad" ]; then
    echo "FAIL ($label): per-class ceilings outside [1, global]:" >&2
    echo "$bad" | sed 's/^/  /' >&2
    echo "  SlaConfig::validate rejects this — scheduler crash-loops at boot" >&2
    return 1
  fi
}

check_render prod \
  --set karpenter.enabled=true --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role --set karpenter.amiTag=test \
  --set postgresql.enabled=false
check_render vmtest-full -f values/vmtest-full.yaml
