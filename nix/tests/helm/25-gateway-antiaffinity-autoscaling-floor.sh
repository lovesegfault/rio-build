# Gateway spread vs PDB gating asymmetry at the autoscaling floor.
#
# Under autoscaling KEDA owns the live pod count anywhere in
# [minReplicas, maxReplicas], so gateway.yaml keys its required
# podAntiAffinity gate on the CEILING: spread must hold whenever the
# autoscaler MAY run more than one pod, and a required anti-affinity
# rule constrains nothing while only one pod exists. Keying on the
# floor would let an operator-set minReplicas=1 render NO anti-affinity
# while KEDA still scales to maxReplicas pods that can all bin-pack
# onto one node — one drain then removes every NLB target of the only
# build-submission ingress (r37 bug_020 all over again).
#
# The rio-gateway PDB (pdb.yaml) deliberately keys on the FLOOR: a
# minAvailable=1 budget against a 1-pod floor would block every node
# drain. This fragment pins BOTH sides of that asymmetry at the
# pathological floor so neither gate regresses onto the other's
# expression.
#
# Every replica knob except the ceiling is forced to 1, so the
# anti-affinity below can only come from the gate keying on
# maxReplicas (chart default 8).

floor=$TMPDIR/gw-floor1.yaml
helm template rio . --set global.image.tag=test \
  --set podDisruptionBudget.enabled=true \
  --set gateway.replicas=1 \
  --set gateway.autoscaling.enabled=true \
  --set gateway.autoscaling.minReplicas=1 >"$floor"

# Premise guard (fail loudly, not vacuously): the gateway Deployment
# must actually render in this configuration.
dep=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-gateway")' "$floor")
test -n "$dep" || {
  echo "FAIL: rio-gateway Deployment did not render with autoscaling on — assertion vacuous" >&2
  exit 1
}

# Spread keys on the ceiling: required podAntiAffinity must render even
# at a 1-pod floor. Capture-then-grep (not yq | grep -q) — same SIGPIPE
# shape called out in 21-control-plane-readiness.sh.
aff=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-gateway") | .spec.template.spec.affinity.podAntiAffinity' "$floor")
grep -q 'requiredDuringSchedulingIgnoredDuringExecution' <<<"$aff" || {
  echo "FAIL: rio-gateway lost required podAntiAffinity at autoscaling minReplicas=1 — KEDA can still scale to maxReplicas pods on one node (anti-affinity must gate on the ceiling)" >&2
  exit 1
}

# PDB keys on the floor: at minReplicas=1 the rio-gateway PDB must NOT
# render, or every node drain would be blocked by minAvailable=1
# against a single pod.
pdb=$(yq -N 'select(.kind=="PodDisruptionBudget" and .metadata.name=="rio-gateway")' "$floor")
test -z "$pdb" || {
  echo "FAIL: rio-gateway PDB rendered at autoscaling minReplicas=1 — minAvailable=1 against a 1-pod floor blocks every drain (PDB must gate on the floor)" >&2
  exit 1
}

echo "OK"
