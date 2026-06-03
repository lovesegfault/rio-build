# Builder/fetcher → store L7 allow-list (bug_290): untrusted build
# pods may call exactly the data-plane RPCs a build needs on
# rio-store:9002 — and TailLog (tenant log reads) is deliberately
# ABSENT from the list, so build-log content is unreachable from
# builder pods even with a leaked tenant token.
#
# (documentary — .sh is not tracey-scanned; the normative rule is
# store.log.method-credential. This fragment is the merge-gate render
# proof of the rule's chart half: the L7 rules exist, AppendLog is
# present, TailLog is not.)
#
# Cilium enforces gRPC at L7 as HTTP/2 POSTs to /pkg.Service/Method;
# adding `rules.http` to the store toPorts entry switches that edge to
# the L7 proxy. The render assertions below pin both directions:
# losing the rules block entirely (silent L4 fallback = TailLog
# reachable again) AND accidentally allow-listing TailLog both fail.

out=$TMPDIR/builder-l7.yaml
helm template rio . --set global.image.tag=test >"$out"

for policy in builder-egress fetcher-egress; do
  doc=$(yq -N "select(.kind==\"CiliumClusterwideNetworkPolicy\" and .metadata.name==\"$policy\")" "$out")
  test -n "$doc" || {
    echo "FAIL: $policy did not render — assertions vacuous" >&2
    exit 1
  }

  # The store:9002 egress entry must carry an L7 http rules block.
  l7=$(echo "$doc" | yq -N '.spec.egress[] | select(.toPorts[0].ports[0].port=="9002" and (.toEndpoints[0].matchLabels["k8s:app.kubernetes.io/name"]=="rio-store")) | .toPorts[0].rules.http')
  if [ -z "$l7" ] || [ "$l7" = "null" ]; then
    echo "FAIL: $policy store:9002 entry has no L7 http rules — TailLog is reachable at L4" >&2
    exit 1
  fi

  # AppendLog present (builds must still upload logs).
  echo "$l7" | grep -q '/rio.store.LogService/AppendLog' || {
    echo "FAIL: $policy L7 list is missing AppendLog — builds could not upload logs" >&2
    exit 1
  }

  # TailLog absent — THE property this fragment exists to pin.
  if echo "$l7" | grep -q '/rio.store.LogService/TailLog'; then
    echo "FAIL: $policy L7 list allow-lists TailLog — builder pods could read build logs" >&2
    exit 1
  fi
done

echo "OK: builder/fetcher store L7 allow-lists carry AppendLog and exclude TailLog"
