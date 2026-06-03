# Cross-replica log-tail peer template: single-sourced and fail-closed
# (merged_bug_051). values.yaml `store.logPeerUrlTemplate` is the ONLY
# default — the template renders it verbatim (no `| default` shadow
# that could silently diverge), and nulling the key renders an EMPTY
# env, which the binary treats as proxy-disabled rather than guessing
# an address scheme.
#
# (documentary — .sh is not tracey-scanned; the binary-side rules are
# the config validate() and PeerResolver::uri_for unit tests. This
# fragment is the merge-gate render proof of the chart half.)

out=$TMPDIR/log-peer.yaml
helm template rio . --set global.image.tag=test >"$out"

values_default=$(yq -N '.store.logPeerUrlTemplate' values.yaml)
test "$values_default" = "http://{pod}:9002" || {
  echo "FAIL: values.yaml logPeerUrlTemplate changed ($values_default) — update the binary docs + this fragment together" >&2
  exit 1
}

rendered=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.containers[0].env[] | select(.name=="RIO_LOG_PEER_URL_TEMPLATE") | .value' "$out")
test "$rendered" = "$values_default" || {
  echo "FAIL: rendered RIO_LOG_PEER_URL_TEMPLATE ($rendered) != values default ($values_default) — a default shadow crept back into store.yaml" >&2
  exit 1
}

case "$rendered" in
  *'[{pod}]'*)
    echo "FAIL: the template carries literal brackets; uri_for brackets IPv6 itself and validate() rejects this form" >&2
    exit 1
    ;;
esac

# Nulling the key disables the proxy: the env renders empty, not a
# fallback address.
out_null=$TMPDIR/log-peer-null.yaml
helm template rio . --set global.image.tag=test --set store.logPeerUrlTemplate= >"$out_null"
rendered_null=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.containers[0].env[] | select(.name=="RIO_LOG_PEER_URL_TEMPLATE") | .value' "$out_null")
test -z "$rendered_null" || {
  echo "FAIL: nulled logPeerUrlTemplate rendered \"$rendered_null\" instead of empty (fail-closed)" >&2
  exit 1
}

echo "OK: log peer template single-sourced ($rendered) and fail-closed on null"
