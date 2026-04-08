# rio.optBool: with-on-bool footgun guard.
#
# Explicit `false` MUST render. Helm's `with` is falsy-skip —
# `hostUsers: false` in values produced NO key (controller default
# applied instead of the explicit override). hasKey renders the key
# whenever it's SET, regardless of value. fetcherpool.yaml deep-merges
# fetcherPoolDefaults into each fetcherPools[] entry; setting on
# defaults covers all CRs.

f=$TMPDIR/fp-false.yaml
helm template rio . \
  --set fetcherPoolDefaults.enabled=true \
  --set fetcherPoolDefaults.hostUsers=false \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$f"
yq 'select(.kind=="FetcherPool") | .spec.hostUsers' "$f" |
  grep -x false >/dev/null || {
  echo "FAIL: fetcherPoolDefaults.hostUsers=false did not render (with-on-bool bug)" >&2
  exit 1
}

# Unset stays unset — no spurious key. --set key=null deletes the key
# from the values map (Helm deep-merge semantics), so hasKey sees it as
# absent and the template renders nothing.
u=$TMPDIR/fp-unset.yaml
helm template rio . \
  --set fetcherPoolDefaults.enabled=true \
  --set fetcherPoolDefaults.hostUsers=null \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$u"
test "$(yq 'select(.kind=="FetcherPool") | .spec | has("hostUsers")' "$u")" = false || {
  echo "FAIL: fetcherPoolDefaults.hostUsers unset but key rendered (spurious key)" >&2
  exit 1
}
