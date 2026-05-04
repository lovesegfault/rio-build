# §13d STRIKE-7 (r30 bug_007 / mb_012): metal feature-routing config
# invariants. The pre-§13c kvm Pool advertised `[kvm, nixos-test,
# big-parallel]`; §13c replaced it with `metal-*` hwClasses carrying
# `providesFeatures: [kvm]` only — `features_compatible` is
# subset-on-required, so any nixpkgs `nixosTest` derivation
# (`requiredSystemFeatures=["nixos-test","kvm"]`) is unroutable.
#
# These are static config invariants no runtime chokepoint can catch:
# `provides_features ∋ kvm` and `labels ∋ {rio.build/kvm: "true"}` are
# sibling keys in the same TOML block with no structural coupling. The
# pod's `nodeSelector{rio.build/kvm}` is satisfied IFF the metal class's
# `labels` carries it; the kvm pod's toleration matches IFF the class's
# `taints` carries it; the builder pod schedules onto the cover-minted
# node IFF `poolDefaults.tolerations` ⊇ `cover.rs::builder_taint()`.
# A typo in any one is a permanently-Pending pod with a `nodeSelector`
# no Node satisfies.

# `poolDefaults.enabled=true` so the default `pools[]` list actually
# renders Pool CRDs — they're gated off by default (the production
# chart sets it via overlay).
render=$TMPDIR/feature-routing.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set poolDefaults.enabled=true \
  >"$render"

sched_toml=$TMPDIR/sched-feature.toml
yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
       | .data."scheduler.toml"' "$render" >"$sched_toml"

fail=0

# 1. Every `nodeClass: rio-metal` hwClass MUST `providesFeatures ⊇
#    [kvm, nixos-test]`. Both are hardware gates (not `softFeatures`);
#    metal is the only class hosting them. (bug_007)
# 2. Every kvm-providing hwClass MUST carry `labels ∋ {rio.build/kvm:
#    "true"}` (pod nodeSelector) AND `taints ∋ {key: rio.build/kvm,
#    effect: NoSchedule}` (keeps non-kvm pods off). (mb_012 §1)
#
# awk: track which block (`labels = [` / `taints = [`) we are inside so
# the same `key = "rio.build/kvm"` line can be classified as label vs
# taint without depending on the exact `{ ... }` line shape.
metal_check=$(awk '
  function flush() {
    if (h == "") return
    if (nc == "\"rio-metal\"") {
      if (pf !~ /"kvm"/)        printf "%s: nodeClass=rio-metal but providesFeatures missing \"kvm\"\n", h
      if (pf !~ /"nixos-test"/) printf "%s: nodeClass=rio-metal but providesFeatures missing \"nixos-test\"\n", h
    }
    if (pf ~ /"kvm"/) {
      if (!haslbl)   printf "%s: providesFeatures has kvm but labels missing rio.build/kvm=true\n", h
      if (!hastaint) printf "%s: providesFeatures has kvm but taints missing rio.build/kvm:NoSchedule\n", h
    }
  }
  /^\[sla\.hw_classes\./ {
    flush(); h=$0; sub(/.*"/,"",h); sub(/".*/,"",h)
    nc=""; pf=""; haslbl=0; hastaint=0; sect=""
  }
  /^\[sla\./ && !/^\[sla\.hw_classes\./ { flush(); h="" }
  h && /^node_class = /        { nc=$3 }
  h && /^provides_features = / { pf=$0 }
  h && /^labels = \[/          { sect="labels" }
  h && /^taints = \[/          { sect="taints" }
  h && /^requirements = \[/    { sect="" }
  h && /^\]/                   { sect="" }
  h && sect=="labels" && /key = "rio\.build\/kvm"/ && /value = "true"/                       { haslbl=1 }
  h && sect=="taints" && /key = "rio\.build\/kvm"/ && /value = "true"/ && /effect = "NoSchedule"/ { hastaint=1 }
  END { flush() }
' "$sched_toml")
if [ -n "$metal_check" ]; then
  echo "FAIL (18-metal-feature-routing §hwClass cross-field):" >&2
  echo "$metal_check" >&2
  fail=1
fi
# Sanity: at least one rio-metal class in the default chart.
if ! grep -q 'node_class = "rio-metal"' "$sched_toml"; then
  echo "FAIL: no nodeClass=rio-metal hwClass in default chart — assertion vacuous" >&2
  fail=1
fi

# 3. `poolDefaults.tolerations ⊇ {rio.build/builder=true:NoSchedule}` —
#    `cover.rs::builder_taint()` stamps this on every cover-minted Node;
#    a Pool without the toleration permanently Pending. (mb_012 §2)
# 4. Pools with `features ∋ kvm ⟹ tolerations ⊇ {rio.build/kvm=true:
#    NoSchedule}` — metal Nodes carry the kvm taint. The default chart
#    has no kvm pool; this guards overlays that add one. (mb_012 §2)
pool_check=$(yq -N 'select(.kind=="Pool") | {
  "name": .metadata.name,
  "kind": .spec.kind,
  "features": (.spec.features // []),
  "builder_tol": ([.spec.tolerations[]?
    | select(.key=="rio.build/builder" and .value=="true" and .effect=="NoSchedule")] | length),
  "kvm_tol": ([.spec.tolerations[]?
    | select(.key=="rio.build/kvm" and .value=="true" and .effect=="NoSchedule")] | length)
}' -o=json "$render" | jq -r '
  if .kind == "Builder" and .builder_tol == 0 then
    "\(.name): Builder Pool missing rio.build/builder=true:NoSchedule toleration"
  elif (.features | index("kvm")) and .kvm_tol == 0 then
    "\(.name): kvm Pool missing rio.build/kvm=true:NoSchedule toleration"
  else empty end
')
if [ -n "$pool_check" ]; then
  echo "FAIL (18-metal-feature-routing §pool toleration):" >&2
  echo "$pool_check" >&2
  fail=1
fi

# 4b. Same kvm-Pool toleration check against an overlay that adds a
#     kvm pool — the default chart has none, so 4 is vacuous without
#     this. Reuses §4's predicate against a `--set-json pools=...`
#     render.
kvm_render=$TMPDIR/feature-routing-kvm.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set poolDefaults.enabled=true \
  --set-json 'pools=[{"name":"x86-64-kvm","kind":"Builder","systems":["x86_64-linux"],"features":["kvm"],"tolerations":[{"key":"rio.build/builder","operator":"Equal","value":"true","effect":"NoSchedule"},{"key":"rio.build/kvm","operator":"Equal","value":"true","effect":"NoSchedule"}]}]' \
  >"$kvm_render"
kvm_pool_check=$(yq -N 'select(.kind=="Pool") | {
  "name": .metadata.name,
  "features": (.spec.features // []),
  "kvm_tol": ([.spec.tolerations[]?
    | select(.key=="rio.build/kvm" and .value=="true" and .effect=="NoSchedule")] | length)
}' -o=json "$kvm_render" | jq -r '
  if (.features | index("kvm")) and .kvm_tol == 0 then
    "\(.name): kvm Pool missing rio.build/kvm=true:NoSchedule toleration"
  else empty end
')
if [ -n "$kvm_pool_check" ]; then
  echo "FAIL (18-metal-feature-routing §kvm pool toleration overlay):" >&2
  echo "$kvm_pool_check" >&2
  fail=1
fi
# Negative: a kvm pool WITHOUT the toleration must trigger the predicate.
kvm_neg_render=$TMPDIR/feature-routing-kvm-neg.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set poolDefaults.enabled=true \
  --set-json 'pools=[{"name":"x86-64-kvm-bad","kind":"Builder","systems":["x86_64-linux"],"features":["kvm"]}]' \
  >"$kvm_neg_render"
kvm_neg_check=$(yq -N 'select(.kind=="Pool") | {
  "name": .metadata.name,
  "features": (.spec.features // []),
  "kvm_tol": ([.spec.tolerations[]?
    | select(.key=="rio.build/kvm" and .value=="true" and .effect=="NoSchedule")] | length)
}' -o=json "$kvm_neg_render" | jq -r '
  if (.features | index("kvm")) and .kvm_tol == 0 then
    "\(.name): kvm Pool missing rio.build/kvm=true:NoSchedule toleration"
  else empty end
')
if [ -z "$kvm_neg_check" ]; then
  echo "FAIL: §4 kvm-pool predicate is vacuous — a kvm pool without the toleration should be flagged" >&2
  fail=1
fi

[ "$fail" = 0 ] || exit 1
