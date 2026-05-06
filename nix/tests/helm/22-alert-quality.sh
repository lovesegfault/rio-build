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

# --- (1) absent()-polarity: render with the component DISABLED ---
mon=$TMPDIR/mon-no-bs.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true --set buildScheduler.enabled=false >"$mon"
if grep -q 'KubeBuildScheduler' "$mon"; then
  echo "FAIL: KubeBuildScheduler* alert renders with buildScheduler.enabled=false" \
       "— absent() fires permanently when the scrape target doesn't exist" >&2
  exit 1
fi
# Vacuity guard: the same alert MUST appear when the component IS enabled.
mon_on=$TMPDIR/mon-bs.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true --set buildScheduler.enabled=true >"$mon_on"
grep -q 'alert: KubeBuildSchedulerDown' "$mon_on" || {
  echo "FAIL: KubeBuildSchedulerDown missing when buildScheduler.enabled=true" \
       "— assertion vacuous" >&2
  exit 1
}

# --- (2) age-vs-count: every `_live{...} > 0` alert with for: ≥5m ---
# must instead key on a sibling `_age_max_seconds` gauge, OR carry a
# `# count-ok:` exemption comment (none today; the comment forces a
# future deliberate decision).
n_count_alerts=$(yq -N \
  '.spec.groups[].rules[] | select(.expr // "" | test("_live\{.*\}\s*>\s*0")) | .alert' \
  "$mon_on" | wc -l)
test "$n_count_alerts" -eq 0 || {
  echo "FAIL: $n_count_alerts alert(s) key on a count gauge '_live{...} > 0'" \
       "— count cannot establish per-claim age. Use '_age_max_seconds'." >&2
  yq -N '.spec.groups[].rules[] | select(.expr // "" | test("_live\{.*\}\s*>\s*0")) | .alert' "$mon_on" >&2
  exit 1
}

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
