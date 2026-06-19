# sh-043: Sprig `default` treats integer 0 as empty — an operator
# setting `karpenter.nodeclaimPool.maxInflightUnlaunched: 0` as an
# emergency mint-halt would have rendered `= 50`. The chart-level
# default in values.yaml covers nil; the template MUST NOT shadow it
# (the 47-template-default-ban single-default convention). Explicit 0
# is the meaningful kill-switch the law's own doc treats as halt.
#
# r3: `required` because Sprig `int64 nil`/`float64 nil` coerce to 0 —
# `--set …=null` would otherwise silently render the dangerous 0 (the
# value values.yaml's own comment forbids). Helm `required` checks
# nil/"" only, so explicit 0 still passes the guard.

. "$(dirname "$0")/_lib.sh"

# Explicit 0 renders 0 (NOT the Sprig-swallowed 50; NOT a `required`
# refusal — Helm `required` checks nil/"" only).
got=$(render_controller_toml --set karpenter.nodeclaimPool.maxInflightUnlaunched=0 \
  | toml_int_key max_inflight_unlaunched)
test "$got" = "0" || {
  echo "FAIL: maxInflightUnlaunched=0 rendered max_inflight_unlaunched=$got, want 0" >&2
  echo "  (Sprig 'default N' treats integer 0 as empty — the operator's mint-halt" >&2
  echo "   kill-switch was silently swallowed; sh-043-r1)" >&2
  exit 1
}

# Unset renders the values.yaml default (50).
got=$(render_controller_toml | toml_int_key max_inflight_unlaunched)
test "$got" = "50" || {
  echo "FAIL: unset maxInflightUnlaunched rendered $got, want values.yaml default 50" >&2
  exit 1
}

# nil → render REFUSES (the planted-red gate leg). Without the
# `required` wrapper, `int64 nil` / `float64 nil` coerce to 0.
err=$TMPDIR/nil-guard.err
if render_karpenter --set karpenter.nodeclaimPool.maxInflightUnlaunched=null >/dev/null 2>"$err"; then
  echo "FAIL: maxInflightUnlaunched=null rendered — required guard fail-open (nil→0 is the mint-halt)" >&2
  exit 1
fi
grep -q "maxInflightUnlaunched must be set" "$err" || {
  echo "FAIL: maxInflightUnlaunched=null refused but without naming the key:" >&2
  sed 's/^/  /' "$err" >&2
  exit 1
}
if render_karpenter --set scheduler.sla.defaultLeadTimeSeed=null >/dev/null 2>"$err"; then
  echo "FAIL: defaultLeadTimeSeed=null rendered — required guard fail-open (nil→0.0 reaps every NodeClaim before boot)" >&2
  exit 1
fi
grep -q "defaultLeadTimeSeed must be set" "$err" || {
  echo "FAIL: defaultLeadTimeSeed=null refused but without naming the key:" >&2
  sed 's/^/  /' "$err" >&2
  exit 1
}
