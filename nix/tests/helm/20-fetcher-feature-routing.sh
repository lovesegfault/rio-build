# §13e: fetcher feature-routing config invariants. FODs route by
# `required_features=[fetcher]` (derived from `is_fixed_output` at the
# scheduler chokepoint) through the same SLA-solve / per-intent-affinity
# / dynamic-NodeClaim path as builders. The bidirectional ∅-guard in
# `features_compatible` makes `[fetcher]` a strict partition: a
# featureless builder cannot route to a fetcher cell and a FOD cannot
# route to a builder cell — IFF the chart's hwClass config carries the
# right sibling keys.
#
# These are static config invariants no runtime chokepoint can catch:
# `provides_features ∋ fetcher`, `labels ∋ {rio.build/fetcher: "true"}`
# and `taints ∋ {key: rio.build/fetcher, effect: NoSchedule}` are
# sibling keys in the same TOML block with no structural coupling. The
# per-intent nodeAffinity pins fetcher pods to nodes carrying the
# class's `labels`; the fetcher pod's toleration matches IFF the class's
# `taints` carries the key; `taints_routing_to(FETCHER_TAINT_KEY)`
# returns the union of ALL taints from any class carrying the fetcher
# taint key. A typo in any one is a permanently-Pending pod with an
# affinity no Node satisfies — or a misconfigured class with both
# `rio.build/kvm` and `rio.build/fetcher` taints gives the fetcher pod
# both tolerations and lets it land on metal.

render=$TMPDIR/fetcher-routing.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set poolDefaults.enabled=true \
  >"$render"

sched_toml=$TMPDIR/sched-fetcher.toml
yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
       | .data."scheduler.toml"' "$render" >"$sched_toml"

fail=0

# §1+§2+§3+§3b in a single awk pass over the hwClass blocks. `h`
# extraction: anchored prefix/suffix sub (per r34 bug_014's fix —
# `sub(/.*"/,…)` greedily collapses `h` to `]`).
#
# §1: every `fetcher-*` hwClass MUST have `nodeClass: rio-default`,
#     `providesFeatures ∋ fetcher`, `labels ∋ {rio.build/fetcher: "true"}`,
#     `taints ∋ {key: rio.build/fetcher, effect: NoSchedule}`. All four
#     are load-bearing: nodeClass keeps fetchers off the BIOS-AMI
#     partition (cover.rs metal_partition_op gates instance-size on it);
#     providesFeatures is the routing key; the label is the per-intent
#     affinity term; the taint keeps non-fetcher pods off.
# §2: no NON-`fetcher-*` hwClass advertises `providesFeatures ∋ fetcher`
#     — the partition is exclusive. A builder class providing `fetcher`
#     would absorb FOD intents and starve the fetcher tier.
# §3: no `fetcher-*` hwClass advertises `kvm` or `nixos-test` — fetchers
#     don't run nixosTests; advertising the feature would route /dev/kvm
#     gated intents to nodes that lack it.
# §3b: taint partition is exclusive. No fetcher class has a
#     `rio.build/kvm` taint; no kvm-tainted class has a `rio.build/fetcher`
#     taint. `taints_routing_to(FETCHER_TAINT_KEY)` returns the UNION of
#     all taints from any class carrying the fetcher key — a class with
#     both keys gives fetcher pods a kvm toleration (lands on metal,
#     burns $/hr) and gives kvm pods a fetcher toleration (lands on a
#     tiny fetcher node, OOMs).
fetcher_routing_awk='
  function flush() {
    if (h == "") return
    if (h ~ /^fetcher-/) {
      if (nc != "\"rio-default\"") printf "%s: fetcher-* hwClass but nodeClass=%s (want rio-default)\n", h, nc
      if (pf !~ /"fetcher"/)       printf "%s: fetcher-* hwClass but providesFeatures missing \"fetcher\"\n", h
      if (!haslbl)                 printf "%s: fetcher-* hwClass but labels missing rio.build/fetcher=true\n", h
      if (!hastaint)               printf "%s: fetcher-* hwClass but taints missing rio.build/fetcher:NoSchedule\n", h
      if (pf ~ /"kvm"/)            printf "%s: fetcher-* hwClass advertises kvm — would route /dev/kvm intents to a node without it\n", h
      if (pf ~ /"nixos-test"/)     printf "%s: fetcher-* hwClass advertises nixos-test — fetchers do not run nixosTests\n", h
      if (haskvmtaint)             printf "%s: fetcher-* hwClass has rio.build/kvm taint — taints_routing_to(fetcher) would give fetcher pods a kvm toleration\n", h
    } else {
      if (pf ~ /"fetcher"/)        printf "%s: non-fetcher hwClass advertises providesFeatures fetcher — partition is not exclusive\n", h
      # key-only match (mirrors taints_routing_to: it matches on key alone,
      # then unions ALL the class taints — a metal class with a degenerate
      # {key: rio.build/fetcher, value: false} would slip past a strict
      # value/effect check AND leak its kvm taint into the fetcher toleration)
      if (hasfetcherkey)           printf "%s: non-fetcher hwClass has a rio.build/fetcher taint key — taints_routing_to(fetcher) would give its pods a fetcher toleration AND leak this class taints into fetcher pods\n", h
    }
  }
  /^\[sla\.hw_classes\./ {
    flush(); h=$0; sub(/^\[sla\.hw_classes\."/,"",h); sub(/"\]$/,"",h)
    nc=""; pf=""; haslbl=0; hastaint=0; hasfetcherkey=0; haskvmtaint=0; sect=""
  }
  /^\[sla\./ && !/^\[sla\.hw_classes\./ { flush(); h="" }
  h && /^node_class = /        { nc=$3 }
  h && /^provides_features = / { pf=$0 }
  h && /^labels = \[/          { sect="labels" }
  h && /^taints = \[/          { sect="taints" }
  h && /^requirements = \[/    { sect="" }
  h && /^\]/                   { sect="" }
  h && sect=="labels" && /key = "rio\.build\/fetcher"/ && /value = "true"/                           { haslbl=1 }
  h && sect=="taints" && /key = "rio\.build\/fetcher"/ && /value = "true"/ && /effect = "NoSchedule"/ { hastaint=1 }
  h && sect=="taints" && /key = "rio\.build\/fetcher"/                                                { hasfetcherkey=1 }
  h && sect=="taints" && /key = "rio\.build\/kvm"/                                                    { haskvmtaint=1 }
  END { flush() }
'
routing_check=$(awk "$fetcher_routing_awk" "$sched_toml")
if [ -n "$routing_check" ]; then
  echo "FAIL (20-fetcher-feature-routing §hwClass cross-field):" >&2
  echo "$routing_check" >&2
  fail=1
fi
# Sanity: at least one fetcher-* class in the default chart.
if ! grep -q '^\[sla\.hw_classes\."fetcher-' "$sched_toml"; then
  echo "FAIL: no fetcher-* hwClass in default chart — assertion vacuous" >&2
  fail=1
fi

# Negative: §1 must FAIL when a fetcher-* hwClass drops the taint.
# Single-source the awk so the positive and negative cannot drift
# (same lesson as 18-metal-feature-routing's §5/§6).
neg_render=$TMPDIR/fetcher-routing-notaint.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set poolDefaults.enabled=true \
  --set-json 'scheduler.sla.hwClasses.fetcher-notaint={"nodeClass":"rio-default","capacityTypes":["spot"],"providesFeatures":["fetcher"],"labels":[{"key":"rio.build/fetcher","value":"true"},{"key":"kubernetes.io/arch","value":"amd64"}],"requirements":[{"key":"kubernetes.io/arch","operator":"In","values":["amd64"]}]}' \
  >"$neg_render"
neg_toml=$TMPDIR/sched-fetcher-notaint.toml
yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
       | .data."scheduler.toml"' "$neg_render" >"$neg_toml"
neg_check=$(awk "$fetcher_routing_awk" "$neg_toml")
if ! grep -q '^fetcher-notaint: fetcher-\* hwClass but taints missing' <<<"$neg_check"; then
  echo "FAIL: §1 fetcher-routing predicate is vacuous — a fetcher hwClass without the taint (fetcher-notaint) should be flagged. Got:" >&2
  echo "$neg_check" >&2
  fail=1
fi

# Negative: §2 must FAIL when a non-fetcher hwClass advertises fetcher.
neg2_render=$TMPDIR/fetcher-routing-leaked.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set poolDefaults.enabled=true \
  --set-json 'scheduler.sla.hwClasses.builder-leaked={"nodeClass":"rio-default","capacityTypes":["spot"],"providesFeatures":["fetcher"],"requirements":[{"key":"kubernetes.io/arch","operator":"In","values":["amd64"]}]}' \
  >"$neg2_render"
neg2_toml=$TMPDIR/sched-fetcher-leaked.toml
yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
       | .data."scheduler.toml"' "$neg2_render" >"$neg2_toml"
neg2_check=$(awk "$fetcher_routing_awk" "$neg2_toml")
if ! grep -q '^builder-leaked: non-fetcher hwClass advertises providesFeatures fetcher' <<<"$neg2_check"; then
  echo "FAIL: §2 fetcher-exclusivity predicate is vacuous — a non-fetcher hwClass advertising fetcher (builder-leaked) should be flagged. Got:" >&2
  echo "$neg2_check" >&2
  fail=1
fi

[ "$fail" = 0 ] || exit 1
