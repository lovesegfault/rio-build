# rio.optBool: with-on-bool footgun guard.
#
# Explicit `false` MUST render. Helm's `with` is falsy-skip —
# `hostUsers: false` in values produced NO key (controller default
# applied instead of the explicit override). hasKey renders the key
# whenever it's SET, regardless of value. pool.yaml deep-merges
# poolDefaults into each pools[] entry; setting on defaults covers
# all CRs.

f=$TMPDIR/pl-false.yaml
helm template rio . \
  --set poolDefaults.enabled=true \
  --set poolDefaults.hostUsers=false \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$f"
yq 'select(.kind=="Pool" and .spec.kind=="Builder") | .spec.hostUsers' "$f" |
  grep -x false >/dev/null || {
  echo "FAIL: poolDefaults.hostUsers=false did not render (with-on-bool bug)" >&2
  exit 1
}

# Unset stays unset — no spurious key. --set key=null deletes the key
# from the values map (Helm deep-merge semantics), so hasKey sees it as
# absent and the template renders nothing.
u=$TMPDIR/pl-unset.yaml
helm template rio . \
  --set poolDefaults.enabled=true \
  --set poolDefaults.hostUsers=null \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  >"$u"
test "$(yq 'select(.kind=="Pool" and .spec.kind=="Builder") | .spec | has("hostUsers")' "$u")" = false || {
  echo "FAIL: poolDefaults.hostUsers unset but key rendered (spurious key)" >&2
  exit 1
}
