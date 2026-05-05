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
# 4. Pools with `features ∋ kvm` get the `rio.build/kvm:NoSchedule`
#    toleration auto-injected by the controller (`r[ctrl.pool.kvm-device]`,
#    pod.rs `wants_metal`); NOT chart-asserted. r31 bug_022: the prior §4
#    `Pool.spec.tolerations` check tested a non-load-bearing path —
#    production `deploy.rs::POOLS_JSON` ships kvm Pools without it and
#    schedules fine via the auto-inject. Contrast §3: `effective_tolerations`
#    does NOT auto-append `rio.build/builder`, so that one IS load-bearing.
pool_check=$(yq -N 'select(.kind=="Pool") | {
  "name": .metadata.name,
  "kind": .spec.kind,
  "builder_tol": ([.spec.tolerations[]?
    | select(.key=="rio.build/builder" and .value=="true" and .effect=="NoSchedule")] | length)
}' -o=json "$render" | jq -r '
  if .kind == "Builder" and .builder_tol == 0 then
    "\(.name): Builder Pool missing rio.build/builder=true:NoSchedule toleration"
  else empty end
')
if [ -n "$pool_check" ]; then
  echo "FAIL (18-metal-feature-routing §pool toleration):" >&2
  echo "$pool_check" >&2
  fail=1
fi

# 5. Every `(hwClass, capacityType)` cell from `hwClasses ×
#    capacityTypes` MUST have a `leadTimeSeed` entry. Without one,
#    `seed_for(cell)` falls to `defaultLeadTimeSeed` (30s, virtualized
#    boot) and `health::classify` reaps the NodeClaim as `BootTimeout`
#    at `2×30=60s` — long before bare-metal kubelet registration →
#    infinite mint→reap loop, $/hr burn, kvm builds Pending forever.
#    `BootTimeout` is not ICE-masked, so nothing breaks the loop.
#    (bug_015 — same lifecycle hole bug_029's test doc describes.)
#
#    A future hwClass added without re-running `xtask k8s probe-boot`
#    is a chart-render error, NOT a "the default covers it" — fail at
#    lint, not in production. `parse_cell` accepts both `od` and
#    `on-demand` for the cap suffix; `capacity_types` uses Karpenter's
#    `on-demand` label, the hand-pasted seeds use the short form.
#
#    Two-pass via END so the check is order-independent of where
#    `[sla.lead_time_seed]` renders relative to `[sla.hw_classes.*]`.
#    `h` extraction: anchored prefix/suffix sub (not the §1/§2 greedy
#    `sub(/.*"/,…)` which collapses to `]` — §1 doesn't print `h` so it
#    never noticed; §5 uses `h` as the seed-map key).
lead_time_seed_coverage_awk='
  function flush() {
    if (h == "") return
    if (caps == "") caps = "spot,on-demand"   # serde default [Spot, Od]
    cell_caps[h] = caps
  }
  in_seed && /^\[/                  { in_seed = 0 }
  /^\[sla\.lead_time_seed\]/        { in_seed = 1; next }
  in_seed && /=/                    { seeds[$1] = 1 }
  /^\[sla\.hw_classes\./            { flush(); h = $0; sub(/^\[sla\.hw_classes\."/,"",h); sub(/"\]$/,"",h); caps = "" }
  /^\[sla\./ && !/^\[sla\.hw_classes\./ { flush(); h = "" }
  h && /^capacity_types = /         { caps = $0; sub(/.*\[/,"",caps); sub(/\].*/,"",caps); gsub(/[" ]/,"",caps) }
  END {
    flush()
    for (h in cell_caps) {
      n = split(cell_caps[h], a, ",")
      for (i = 1; i <= n; i++) {
        cap = a[i]; alt = (cap == "on-demand") ? "od" : cap
        if (!(("\"" h ":" cap "\"") in seeds) && !(("\"" h ":" alt "\"") in seeds))
          printf "%s:%s has no leadTimeSeed entry — falls to defaultLeadTimeSeed (boot-timeout reap)\n", h, alt
      }
    }
  }
'
seed_check=$(awk "$lead_time_seed_coverage_awk" "$sched_toml")
if [ -n "$seed_check" ]; then
  echo "FAIL (18-metal-feature-routing §leadTimeSeed coverage):" >&2
  echo "$seed_check" >&2
  echo "  re-run \`xtask k8s probe-boot\` against a deployed chart and paste the leadTimeSeed block, or add a placeholder." >&2
  fail=1
fi
# Sanity: the seed table must be non-empty (an awk-broken parse →
# every cell flagged, but a chart that simply drops the table renders
# `{{- with .leadTimeSeed }}` as nothing → the seeds[] set is empty
# AND cell_caps[] is non-empty → flagged. Both paths fail above.)
# Negative: §5 must FAIL when a hwClass cell has no seed entry.
neg_render=$TMPDIR/feature-routing-noseed.yaml
helm template rio . \
  --set karpenter.enabled=true \
  --set karpenter.clusterName=ci \
  --set karpenter.nodeRoleName=ci-role \
  --set karpenter.amiTag=test \
  --set global.image.tag=test \
  --set postgresql.enabled=false \
  --set poolDefaults.enabled=true \
  --set-json 'scheduler.sla.hwClasses.metal-noseed={"nodeClass":"rio-metal","capacityTypes":["on-demand"],"providesFeatures":["kvm","nixos-test"],"labels":[{"key":"rio.build/kvm","value":"true"},{"key":"kubernetes.io/arch","value":"amd64"}],"taints":[{"key":"rio.build/kvm","value":"true","effect":"NoSchedule"}],"requirements":[{"key":"kubernetes.io/arch","operator":"In","values":["amd64"]}]}' \
  >"$neg_render"
neg_toml=$TMPDIR/sched-noseed.toml
yq -N 'select(.kind=="ConfigMap" and .metadata.name=="rio-scheduler-config")
       | .data."scheduler.toml"' "$neg_render" >"$neg_toml"
neg_seed_check=$(awk "$lead_time_seed_coverage_awk" "$neg_toml")
if ! grep -q '^metal-noseed:od ' <<<"$neg_seed_check"; then
  echo "FAIL: §5 leadTimeSeed-coverage predicate is vacuous — a hwClass without a seed entry (metal-noseed:od) should be flagged. Got:" >&2
  echo "$neg_seed_check" >&2
  fail=1
fi

[ "$fail" = 0 ] || exit 1
