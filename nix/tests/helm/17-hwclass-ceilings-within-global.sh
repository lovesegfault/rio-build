# §13c-2: per-class `max_cores`/`max_mem` are an OPTIONAL operator
# tightening override on the boot-derived catalog ceiling. When set,
# `SlaConfig::validate` enforces `0 < n ≤ global` — out of range
# crash-loops the scheduler at boot. Static catch here at helm-lint.
#
# An UNSET per-class ceiling is the EXPECTED case (`{{- with $def.maxCores
# }}` skips on nil/zero) — the scheduler derives the physical ceiling
# from `describe_instance_types` at boot. The default chart SHOULD render
# zero per-class lines; if someone re-introduces a `default $.sla.maxCores`
# fallback in the template, the redundant-override assertion below catches it.

check_render() {
  local label="$1"
  shift
  helm template rio . --set global.image.tag=test "$@" 2>/dev/null \
    | yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
             | .data."scheduler.toml"' \
      >"$TMPDIR/sched-$label.toml"
  # awk: extract global max_cores/max_mem (under [sla], before any
  # [sla.*] section), then per [sla.hw_classes.*] block independently
  # validate any per-class max_cores / max_mem in [1, global]. Both
  # axes are checked on their OWN line (the original fired only on
  # `/^max_mem = /` — leaked when only max_cores rendered). A block
  # with NEITHER is legal (catalog-derived).
  # §13c-3: when sla.maxCores/maxMem are unset (the default — the
  # scheduler derives them from the catalog at boot), the [sla] block
  # has no max_cores/max_mem lines. Fall back to the hard constants
  # (MAX_CORES_GLOBAL=1023, MAX_MEM_GLOBAL=32 TiB) so a per-class
  # override is still range-checked against the structural ceiling.
  bad=$(awk '
    BEGIN                      { gmc=1023; gmm=35184372088832 }
    /^\[sla\]$/                { in_sla=1; h="" }
    /^\[sla\./                 { in_sla=0; h="" }
    in_sla && /^max_cores = /  { gmc=$3+0 }
    in_sla && /^max_mem = /    { gmm=$3+0 }
    /^\[sla\.hw_classes\./     { h=$0; sub(/.*"/,"",h); sub(/".*/,"",h) }
    h && /^max_cores = / {
      cmc=$3+0
      if (cmc<1 || cmc>gmc) printf "%s.max_cores=%d not in [1,%d]\n", h, cmc, gmc
    }
    h && /^max_mem = / {
      cmm=$3+0
      if (cmm<1 || cmm>gmm) printf "%s.max_mem=%d not in [1,%d]\n", h, cmm, gmm
    }
  ' "$TMPDIR/sched-$label.toml")
  if [ -n "$bad" ]; then
    echo "FAIL ($label): per-class ceiling override outside [1, global]:" >&2
    echo "$bad" | sed 's/^/  /' >&2
    echo "  SlaConfig::validate rejects this — scheduler crash-loops at boot" >&2
    return 1
  fi
  # Default chart contract: NO class sets a per-class override — the
  # scheduler derives all 14 from the catalog. A non-zero count here
  # means either (a) someone hand-pinned a class in values.yaml (legal
  # but should carry rationale), or (b) the template regressed back to
  # `default $.sla.maxCores` (renders the override on every class —
  # redundant, defeats §13c-2). Allow override=0 by default.
  n=$(awk '/^\[sla\.hw_classes\./{h=1} h && /^max_cores = /{c++} h && /^max_mem = /{c++} END{print c+0}' \
        "$TMPDIR/sched-$label.toml")
  if [ "${ALLOW_OVERRIDES:-0}" -eq 0 ] && [ "$n" -gt 0 ]; then
    echo "FAIL ($label): $n per-class max_cores/max_mem lines rendered." >&2
    echo "  §13c-2: per-class ceilings are catalog-derived at boot. If a" >&2
    echo "  hand-pin is intentional, set ALLOW_OVERRIDES=1 in this test" >&2
    echo "  with a rationale; if the template regressed to a default fallback," >&2
    echo "  fix templates/scheduler.yaml." >&2
    return 1
  fi
}

check_render prod \
  --set karpenter.enabled=true --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role --set karpenter.amiTag=test \
  --set postgresql.enabled=false
check_render vmtest-full -f values/vmtest-full.yaml
# Range check: an explicit > global override MUST be flagged. Empty by
# default; this proves the awk fires when a line IS rendered.
# §13c-3: pin sla.maxCores so the awk has a global to compare against
# (when sla.maxCores is unset the [sla] block has no max_cores line and
# the awk falls to MAX_CORES_GLOBAL=1023 — only catches > 1023).
ALLOW_OVERRIDES=1
if check_render bad-override \
  --set karpenter.enabled=true --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role --set karpenter.amiTag=test \
  --set postgresql.enabled=false \
  --set scheduler.sla.maxCores=192 \
  --set scheduler.sla.maxMem=1649267441664 \
  --set scheduler.sla.hwClasses.hi-ebs-x86.maxCores=999 2>/dev/null; then
  echo "FAIL: maxCores=999 (> global=192) not flagged — awk regression" >&2
  exit 1
fi
# §13c-3: with no global set, the awk falls to the hard structural
# constant MAX_CORES_GLOBAL=1023 — a per-class > 1023 must still flag.
if check_render bad-override-hard \
  --set karpenter.enabled=true --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role --set karpenter.amiTag=test \
  --set postgresql.enabled=false \
  --set scheduler.sla.hwClasses.hi-ebs-x86.maxCores=2000 2>/dev/null; then
  echo "FAIL: maxCores=2000 (> MAX_CORES_GLOBAL=1023) not flagged" >&2
  exit 1
fi
ALLOW_OVERRIDES=0
