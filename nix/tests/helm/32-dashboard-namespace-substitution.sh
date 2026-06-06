# Dashboard namespace placeholders render from .Values.namespaces
# (bug_279). store.json's panel exprs carry __RIO_NS_STORE__ — the
# dashboards-configmap template substitutes every __RIO_NS_<KEY>__
# from the namespaces map (values-RANGED replace; NOT a blanket tpl,
# which would evaluate Grafana's {{pod}} legend syntax). This fragment
# proves the round-trip both at defaults and under an override; the
# obs-surface-lint separately denies literal rio namespaces in exprs
# and pins placeholders to real namespaces keys.
#
# (documentary — .sh is not tracey-scanned.)

# Default render: placeholder resolved to the default store namespace,
# zero placeholder residue, Grafana legend syntax intact.
out=$TMPDIR/dash-ns-default.yaml
helm template rio . --set global.image.tag=test --set monitoring.enabled=true >"$out"

grep -q 'namespace=\\"rio-store\\"' "$out" || {
  echo "FAIL: default render lacks namespace=\"rio-store\" in dashboard exprs" >&2
  exit 1
}
if grep -q '__RIO_NS_' "$out"; then
  echo "FAIL: unsubstituted __RIO_NS_ placeholder survives the default render" >&2
  grep -n '__RIO_NS_' "$out" | head -3 >&2
  exit 1
fi
grep -q '{{pod}}' "$out" || {
  echo "FAIL: Grafana {{pod}} legend syntax was mangled by the substitution" >&2
  exit 1
}

# Override render: namespaces.store.name flows into the exprs.
out=$TMPDIR/dash-ns-override.yaml
helm template rio . --set global.image.tag=test --set monitoring.enabled=true \
  --set namespaces.store.name=custom-store >"$out"

grep -q 'namespace=\\"custom-store\\"' "$out" || {
  echo "FAIL: namespaces.store.name override did not reach the dashboard exprs" >&2
  exit 1
}
if grep -q 'namespace=\\"rio-store\\"' "$out"; then
  echo "FAIL: overridden render still carries the default store namespace in an expr" >&2
  exit 1
fi
