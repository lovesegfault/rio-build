# r38 (§process-strike — sibling of 21-control-plane-readiness.sh):
# every alert in prometheusrule.yaml has shipped with at least one
# quality bug per round since r37. This check encodes the three
# bug classes found so far so the NEXT alert fails CI on day 1:
#
# 1. absent()-polarity: an alert using `absent()` watches a series
#    that exists ONLY when its producing component is enabled. It must
#    be gated on the component toggle, or it fails OPEN (fires
#    permanently) when the component is disabled (r38 bug_002).
# 2. age-vs-count: an alert keying `_live{...} > 0` for ≥5m on a
#    state count cannot establish per-claim age (r38 merged_001;
#    the StuckPending sibling already names this anti-pattern).
# 3. runbook coverage: every RioNodeclaimPool* alert has a runbook
#    table row (r38 merged_001 — alert added without one).
#
# Harness contract (nix/misc-checks.nix `helm-lint` runCommand): the runner
# already `cd`s into `$TMPDIR/chart` and executes us with `bash -euo pipefail`.
# No shebang, no `set`, no `cd`, no TMPDIR reassignment — match the
# shape of 02-monitoring-kinds.sh.

# --- (1) absent()-polarity ---
# An alert using `absent()` watches a series that exists ONLY when
# its producing component is enabled. Each one MUST be gated on the
# component toggle, or it fails OPEN (fires permanently) when the
# component is disabled (r38 bug_002).
mon_on=$TMPDIR/mon-bs.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true --set buildScheduler.enabled=true >"$mon_on"

# r39 merged_003: enumerate ALL absent() alerts. A new absent() alert
# fails this assertion loudly with an instruction, instead of silently
# passing because it isn't KubeBuildScheduler*. When you add one to
# this list, ALSO add a "render with the component disabled" check
# below — the per-component check is what actually proves the gating.
absent_alerts=$(yq -N \
  '.spec.groups[].rules[] | select(.expr // "" | test("absent\(")) | .alert' "$mon_on" | sort)
known_absent="KubeBuildSchedulerDown"
test "$absent_alerts" = "$known_absent" || {
  echo "FAIL: absent() alert set changed." >&2
  echo "  got:  $(echo "$absent_alerts" | tr '\n' ' ')" >&2
  echo "  want: $known_absent" >&2
  echo "An absent() alert MUST be gated on its component's enabled" >&2
  echo "toggle — add it to known_absent AND add a render-with-disabled" >&2
  echo "check below (see KubeBuildScheduler precedent)." >&2
  exit 1
}

# Per-component gating check: KubeBuildScheduler* must NOT render
# when buildScheduler.enabled=false.
mon_off=$TMPDIR/mon-no-bs.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true --set buildScheduler.enabled=false >"$mon_off"
if grep -q 'KubeBuildScheduler' "$mon_off"; then
  echo "FAIL: KubeBuildScheduler* alert renders with buildScheduler.enabled=false" \
       "— absent() fires permanently when the scrape target doesn't exist" >&2
  exit 1
fi
# Vacuity guard: the same alert MUST appear when the component IS enabled.
grep -q 'alert: KubeBuildSchedulerDown' "$mon_on" || {
  echo "FAIL: KubeBuildSchedulerDown missing when buildScheduler.enabled=true" \
       "— assertion vacuous" >&2
  exit 1
}

# --- (2) age-vs-count: every `_live{...} > 0` alert with for: ≥5m ---
# must instead key on a sibling `_age_max_seconds` gauge. r39
# merged_003: drop the unimplementable `# count-ok:` exemption claim
# (yq parses YAML structure, not comments). When a count-keyed alert
# is genuinely correct, list it in `count_ok_alerts` below — a
# shell-variable allowlist (same shape as `exempt_aff_pdb` in
# 21-control-plane-readiness.sh).
count_ok_alerts=""    # space-separated alert names; none today.
n_total_alerts=$(yq -N '.spec.groups[].rules[] | .alert' "$mon_on" | grep -c .)
test "$n_total_alerts" -ge 5 || {
  echo "FAIL: expected ≥5 alerts in PrometheusRule, got $n_total_alerts" \
       "— §2 assertion vacuous (yq filter selecting nothing)" >&2
  exit 1
}
# Positive smoke-test: the _live{} > 0 regex must match a synthetic
# expr of the shape it is designed to catch — catches regex rot
# (typo / anchoring change) that the alert-count guard above misses.
smoke=$(printf 'spec:\n  groups:\n  - rules:\n    - alert: Smoke\n      expr: x_live{a="b"} > 0\n' \
  | yq -N '.spec.groups[].rules[] | select(.expr // "" | test("_live\{.*\}\s*>\s*0")) | .alert')
test "$smoke" = "Smoke" || {
  echo "FAIL: §2 _live regex is vacuous — does not match the shape it claims to catch" >&2
  exit 1
}
count_alerts=$(yq -N \
  '.spec.groups[].rules[] | select(.expr // "" | test("_live\{.*\}\s*>\s*0")) | .alert' "$mon_on")
n_count_alerts=0
while read -r alert; do
  [ -z "$alert" ] && continue
  case " $count_ok_alerts " in *" $alert "*) continue ;; esac
  echo "FAIL: alert $alert keys on a count gauge '_live{...} > 0'" \
       "— count cannot establish per-claim age. Use '_age_max_seconds'" \
       "or add to count_ok_alerts with a justification comment." >&2
  n_count_alerts=$((n_count_alerts + 1))
done <<<"$count_alerts"
test "$n_count_alerts" -eq 0 || exit 1

# --- (3) runbook coverage ---
# Copied into the helm-lint sandbox by the runCommand body — see NOTE above.
runbook="$TMPDIR/chart/.runbook-sla-model.md"
# Process substitution (NOT `cmd | while`): keeps `missing` and
# `n_alerts` in the parent shell. The `... || echo 0` pipeline shape
# is a footgun under pipefail — `grep -c` outputs "0" AND exits 1, so
# the fallback double-emits and `test -eq` errors on a non-integer.
missing=0
n_alerts=0
while read -r alert; do
  n_alerts=$((n_alerts + 1))
  grep -q "$alert" "$runbook" || {
    echo "FAIL: alert $alert not in $runbook" >&2
    missing=$((missing + 1))
  }
done < <(yq -N '.spec.groups[].rules[] | select(.alert // "" | test("^RioNodeclaimPool")) | .alert' "$mon_on")
test "$missing" -eq 0 || exit 1
test "$n_alerts" -ge 5 || {
  echo "FAIL: expected ≥5 RioNodeclaimPool* alerts, got $n_alerts — assertion vacuous" >&2
  exit 1
}

echo "OK"
