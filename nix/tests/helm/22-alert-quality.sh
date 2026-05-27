# r38 (§process-strike — sibling of 21-control-plane-readiness.sh):
# every alert in prometheusrule.yaml has shipped with at least one
# quality bug per round since r37. This check encodes the four
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
# 4. staleness aggregation: a zero-activity alert
#    (`increase(...) == 0`) evaluates per series, so it must aggregate
#    across pods (sum) and carry a non-zero `for:` or it pages on
#    every fresh pod (Wave-A1 collector review C6).
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
# Runbook is the typst source (docs/ops/), `#refs.alert("Name")`; substring
# grep below matches `#refs.alert("RioNodeclaimPoolFoo")`.
runbook_typ="$TMPDIR/chart/.runbook-sla-model.typ"
test -f "$runbook_typ" || {
  echo "FAIL: no runbook staged — misc-checks.nix cp missing?" >&2
  exit 1
}
runbooks=("$runbook_typ")
# Process substitution (NOT `cmd | while`): keeps `missing` and
# `n_alerts` in the parent shell. The `... || echo 0` pipeline shape
# is a footgun under pipefail — `grep -c` outputs "0" AND exits 1, so
# the fallback double-emits and `test -eq` errors on a non-integer.
missing=0
n_alerts=0
while read -r alert; do
  n_alerts=$((n_alerts + 1))
  grep -q "$alert" "${runbooks[@]}" || {
    echo "FAIL: alert $alert not in ${runbooks[*]}" >&2
    missing=$((missing + 1))
  }
done < <(yq -N '.spec.groups[].rules[] | select(.alert // "" | test("^RioNodeclaimPool")) | .alert' "$mon_on")
test "$missing" -eq 0 || exit 1
test "$n_alerts" -ge 5 || {
  echo "FAIL: expected ≥5 RioNodeclaimPool* alerts, got $n_alerts — assertion vacuous" >&2
  exit 1
}

# --- (4) staleness / zero-activity alerts ---
# An `increase(...) == 0` staleness alert evaluates per series: on a
# multi-replica component every fresh pod fires it until that pod
# personally produces an event, and a replica that never wins the
# deduplicated work (e.g. another replica holds the GC advisory lock)
# looks stalled forever. Such alerts MUST aggregate across pods
# (sum(increase(...)) == 0) and carry a non-zero `for:` so a rolling
# restart does not page (Wave-A1 collector review C6).
# Positive smoke-test first — guards regex rot, same pattern as §2.
smoke4=$(printf 'spec:\n  groups:\n  - rules:\n    - alert: Smoke4\n      expr: increase(x_total[25h]) == 0\n' \
  | yq -N '.spec.groups[].rules[] | select(.expr // "" | test("increase\(.*\)\s*==\s*0")) | .alert')
test "$smoke4" = "Smoke4" || {
  echo "FAIL: §4 staleness regex is vacuous — does not match the shape it claims to catch" >&2
  exit 1
}
stale_alerts=$(yq -N \
  '.spec.groups[].rules[] | select(.expr // "" | test("increase\(.*\)\s*==\s*0")) | .alert' "$mon_on")
n_stale=0
stale_bad=0
while read -r alert; do
  [ -z "$alert" ] && continue
  n_stale=$((n_stale + 1))
  expr=$(yq -N ".spec.groups[].rules[] | select(.alert == \"$alert\" and (.expr // \"\" | test(\"increase\(.*\)\s*==\s*0\"))) | .expr" "$mon_on")
  for_val=$(yq -N ".spec.groups[].rules[] | select(.alert == \"$alert\" and (.expr // \"\" | test(\"increase\(.*\)\s*==\s*0\"))) | .[\"for\"]" "$mon_on")
  case "$expr" in
    sum*) : ;;
    *)
      echo "FAIL: staleness alert $alert evaluates per series" \
           "— aggregate across pods: sum(increase(...)) == 0" >&2
      stale_bad=$((stale_bad + 1))
      ;;
  esac
  case "$for_val" in
    0m | 0s | null | "")
      echo "FAIL: staleness alert $alert needs a non-zero 'for:'" \
           "so a freshly rolled-out pod does not page" >&2
      stale_bad=$((stale_bad + 1))
      ;;
  esac
done <<<"$stale_alerts"
test "$stale_bad" -eq 0 || exit 1
# Vacuity guard: the collector's stalled alert is the canonical member
# of this class; if no staleness alert renders, §4 selected nothing.
test "$n_stale" -ge 1 || {
  echo "FAIL: expected ≥1 'increase(...) == 0' staleness alert (RioStoreGcCollectStalled)" \
       "— §4 assertion vacuous" >&2
  exit 1
}

echo "OK"
