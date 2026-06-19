# `rio.quantityBytes` (templates/_helpers.tpl) MUST `fail` at `helm
# template` time on any Quantity it can't parse to an integer byte
# count. The pre-fix `else` branch fell through to sprig `int64`, which
# returns 0 on parse failure (e.g. fractional "1.5Ti", decimal-SI
# "500G"). That zeroed `max_node_disk` in `controller.toml`, and
# `cover::sizing`'s over-cap filter (`id <= cfg.max_node_disk`) then
# dropped every intent — a fleet-wide provisioning halt whose only
# signal is `intent_dropped_total{reason=exceeds_cell_cap}`, which
# `prometheusrule.yaml` deliberately excludes from alerting.
#
# Per `feedback_error_messages_name_the_fix`: the helper must fail
# loudly with a message that names the field to fix.

. "$(dirname "$0")/_lib.sh"

# Negative: fractional Quantity → render MUST fail with a message that
# names quantityBytes (so the operator finds the helper, not just an
# opaque "template error").
err=$TMPDIR/quantity-fractional.err
if render_karpenter --set karpenter.dataVolumeSize=1.5Ti >/dev/null 2>"$err"; then
  echo "FAIL: dataVolumeSize=1.5Ti rendered (should fail — sprig int64 would coerce to 0)" >&2
  exit 1
fi
grep -q "quantityBytes" "$err" || {
  echo "FAIL: dataVolumeSize=1.5Ti error does not name quantityBytes:" >&2
  sed 's/^/  /' "$err" >&2
  exit 1
}

# Negative: decimal-SI suffix (G, not Gi). Same int64→0 hazard.
if render_karpenter --set karpenter.dataVolumeSize=500G >/dev/null 2>"$err"; then
  echo "FAIL: dataVolumeSize=500G rendered (should fail — decimal SI is not supported)" >&2
  exit 1
fi
grep -q "quantityBytes" "$err" || {
  echo "FAIL: dataVolumeSize=500G error does not name quantityBytes:" >&2
  sed 's/^/  /' "$err" >&2
  exit 1
}

# Positive: integer Gi (the values.yaml default shape) MUST render and
# produce a non-zero `max_node_disk`. Proves the regex guard didn't
# tighten past the happy path.
got=$(render_controller_toml --set karpenter.dataVolumeSize=500Gi | toml_int_key max_node_disk)
# 500 Gi × 0.9 reserve (controller.yaml) = 483183820800.
test "$got" -eq 483183820800 || {
  echo "FAIL: dataVolumeSize=500Gi → max_node_disk=$got, expected 483183820800 (500Gi × 0.9)" >&2
  exit 1
}

# Positive: integer Ti also renders (the helper supports the full
# binary-suffix set even though the default chart only uses Gi).
got=$(render_controller_toml --set karpenter.dataVolumeSize=2Ti | toml_int_key max_node_disk)
# 2 Ti × 0.9 = 1979120929996.8 → int64 truncates to 1979120929996.
test "$got" -eq 1979120929996 || {
  echo "FAIL: dataVolumeSize=2Ti → max_node_disk=$got, expected 1979120929996 (2Ti × 0.9, int64-truncated)" >&2
  exit 1
}
