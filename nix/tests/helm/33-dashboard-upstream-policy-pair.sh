# Dashboard upstream registry ⇒ BOTH network-policy sides, by
# construction (bug_221, §5-Q16). files/dashboard-upstreams.json
# generates the nginx upstream blocks, the dashboard-egress edges, AND
# a standalone per-record `dashboard-to-<component>` ingress CNP (CNPs
# merge additively with the component's own policy). Before this, the
# ingress side was two hand includes — a third registry record would
# ship an upstream + egress edge whose ingress side silently dropped
# at enforcement.
#
# Negative self-test: append a SYNTHETIC third record in a private
# chart copy (fragments share the staged chart; never mutate it
# in place) and assert the record yields both sides with no hand
# edit anywhere.
#
# bug_019 rider: CNP identity is keyed on the registry's UNIQUE field
# (`name`, underscores DNS-1123-mapped) — never on `component`, which
# may legitimately repeat (a second port/service on one backend). The
# same-component second record below is the UNAUTHORED shape the
# original pair-test missed: pre-fix it rendered two CNPs with
# identical kind/name/namespace (helm duplicate error, or last-wins
# ingress drop under `template | apply`). The registry's `name` field
# itself is asserted unique, so "unique key" is enforced, not assumed.
#
# (documentary — .sh is not tracey-scanned.)

priv=$TMPDIR/chart-221
rm -rf "$priv"
cp -r . "$priv"
cd "$priv"

jq '. += [{
  "name": "rio_synthetic",
  "component": "rio-synthetic",
  "service": "rio-synthetic",
  "namespaceRef": "system",
  "port": 9999,
  "env": "RIO_SYNTHETIC_FQDN"
}]' files/dashboard-upstreams.json > files/dashboard-upstreams.json.tmp
mv files/dashboard-upstreams.json.tmp files/dashboard-upstreams.json

out=$TMPDIR/dash-pair-synth.yaml
helm template rio . --set global.image.tag=test --set dashboard.enabled=true >"$out"

# Side 1: the egress edge for the synthetic record exists.
yq -N 'select(.metadata.name=="rio-dashboard-egress")
       | .spec.egress[].toPorts[]?.ports[]?.port' "$out" | grep -qx '9999' || {
  echo "FAIL: synthetic registry record produced no dashboard-egress edge" >&2
  exit 1
}

# Side 2 (the bug_221 half): the per-record ingress CNP exists, selects
# the component, and admits exactly the dashboard pod on the record's
# port.
kind=$(yq -N 'select(.metadata.name=="dashboard-to-rio-synthetic") | .kind' "$out")
test "$kind" = "CiliumNetworkPolicy" || {
  echo "FAIL: synthetic registry record produced no dashboard-to-rio-synthetic" >&2
  echo "      ingress CNP — the registry does not generate the ingress side" >&2
  exit 1
}
sel=$(yq -N 'select(.metadata.name=="dashboard-to-rio-synthetic")
             | .spec.endpointSelector.matchLabels."app.kubernetes.io/name"' "$out")
test "$sel" = "rio-synthetic" || {
  echo "FAIL: dashboard-to-rio-synthetic selects '$sel', want rio-synthetic" >&2
  exit 1
}
yq -N 'select(.metadata.name=="dashboard-to-rio-synthetic")
       | .spec.ingress[].toPorts[]?.ports[]?.port' "$out" | grep -qx '9999' || {
  echo "FAIL: dashboard-to-rio-synthetic does not open the record port 9999" >&2
  exit 1
}
yq -N 'select(.metadata.name=="dashboard-to-rio-synthetic")
       | .spec.ingress[].fromEndpoints[].matchLabels."k8s:app.kubernetes.io/name"' "$out" \
  | grep -qx 'rio-dashboard' || {
  echo "FAIL: dashboard-to-rio-synthetic ingress is not pinned to the dashboard pod" >&2
  exit 1
}

# bug_019: the UNAUTHORED shape — a second record sharing an existing
# component (second port on rio-store). Identity must key on `name`:
# two DISTINCT CNP names render, each opening exactly its record's
# port, and no dashboard-to-* identity (name|namespace) repeats.
# Pre-fix red (component-keyed name):
#   2 dashboard-to-rio-store|rio-store   ← identical kind/name/ns, ports 9002+9005
cd "$OLDPWD"
root=$PWD
priv2=$TMPDIR/chart-019
rm -rf "$priv2"
cp -r . "$priv2"
cd "$priv2"
jq '. += [{
  "name": "rio_store_admin",
  "component": "rio-store",
  "service": "rio-store-admin",
  "namespaceRef": "store",
  "port": 9005,
  "env": "RIO_STORE_ADMIN_FQDN"
}]' files/dashboard-upstreams.json > files/dashboard-upstreams.json.tmp
mv files/dashboard-upstreams.json.tmp files/dashboard-upstreams.json

out=$TMPDIR/dash-pair-samecomp.yaml
helm template rio . --set global.image.tag=test --set dashboard.enabled=true >"$out"

dup=$(yq -N 'select(.kind=="CiliumNetworkPolicy")
             | select(.metadata.name == "dashboard-to-*")
             | .metadata.name + "|" + .metadata.namespace' "$out" | sort | uniq -d)
test -z "$dup" || {
  echo "FAIL: duplicate dashboard-to-* CNP identity rendered: $dup" >&2
  echo "      (same-component records must yield DISTINCT names — key on .name)" >&2
  exit 1
}
for pair in "dashboard-to-rio-store:9002" "dashboard-to-rio-store-admin:9005"; do
  name=${pair%%:*} port=${pair##*:}
  got=$(yq -N "select(.metadata.name==\"$name\") | .spec.ingress[].toPorts[]?.ports[]?.port" "$out")
  test "$got" = "$port" || {
    echo "FAIL: same-component render lacks $name opening exactly $port (got '$got')" >&2
    exit 1
  }
done
# Both egress edges still merge inside the ONE dashboard-egress CNP.
for port in 9002 9005; do
  yq -N 'select(.metadata.name=="rio-dashboard-egress")
         | .spec.egress[].toPorts[]?.ports[]?.port' "$out" | grep -qx "$port" || {
    echo "FAIL: dashboard-egress lost the port-$port edge under same-component records" >&2
    exit 1
  }
done

# The registry's declared unique key IS unique (the identity law's
# premise): duplicate `name` values fail loudly here, not at deploy.
cd "$root"
dupnames=$(jq -r 'group_by(.name) | map(select(length > 1) | .[0].name) | .[]' files/dashboard-upstreams.json)
test -z "$dupnames" || {
  echo "FAIL: dashboard-upstreams.json has duplicate record name(s): $dupnames" >&2
  exit 1
}

# Defaults (real registry, original chart): both real records pair, and
# the retired hand-include helper is fully deleted.
cd "$root"
out=$TMPDIR/dash-pair-default.yaml
helm template rio . --set global.image.tag=test --set dashboard.enabled=true >"$out"
for pair in "dashboard-to-rio-scheduler:9001" "dashboard-to-rio-store:9002"; do
  name=${pair%%:*} port=${pair##*:}
  got=$(yq -N "select(.metadata.name==\"$name\") | .spec.ingress[].toPorts[]?.ports[]?.port" "$out")
  test "$got" = "$port" || {
    echo "FAIL: default render lacks $name opening port $port (got '$got')" >&2
    exit 1
  }
done
if grep -rn 'dashboardIngressFrom' templates/; then
  echo "FAIL: the hand-include helper survives — the registry range replaced it" >&2
  exit 1
fi
