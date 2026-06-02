# Store scaling surface (decision 5): one-replica-per-node placement.
#
# rio-store's three load classes (substitution ingest, builder
# read-serving, builder upload ingest) all share the pod's NIC, NAR
# buffer memory and PG pool — a replica only adds capacity when it
# lands on its own node. The spread is SOFT (ScheduleAnyway): scale-out
# must never be blocked by a temporarily full node pool; under normal
# capacity each replica still gets its own NIC.

out=$TMPDIR/store-scaling.yaml
helm template rio . --set global.image.tag=test >"$out"

# Premise guard: the store Deployment renders at defaults.
dep=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")' "$out")
test -n "$dep" || {
  echo "FAIL: rio-store Deployment did not render at chart defaults — assertions vacuous" >&2
  exit 1
}

# One-per-node spread: maxSkew 1 on the hostname topology, soft
# (ScheduleAnyway), label-selected on the store pods. Capture-then-grep
# (not yq | grep -q) — the SIGPIPE shape called out in
# 21-control-plane-readiness.sh.
tsc=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store") | .spec.template.spec.topologySpreadConstraints' "$out")
test "$tsc" != "null" && test -n "$tsc" || {
  echo "FAIL: rio-store Deployment has no topologySpreadConstraints — replicas can stack on one node and scale-out stops adding NICs (decision 5 placement)" >&2
  exit 1
}
grep -q 'maxSkew: 1' <<<"$tsc" || {
  echo "FAIL: store topologySpreadConstraints lost maxSkew: 1" >&2
  exit 1
}
grep -q 'topologyKey: kubernetes.io/hostname' <<<"$tsc" || {
  echo "FAIL: store topologySpreadConstraints not keyed on kubernetes.io/hostname" >&2
  exit 1
}
grep -q 'whenUnsatisfiable: ScheduleAnyway' <<<"$tsc" || {
  echo "FAIL: store spread must be SOFT (ScheduleAnyway) — DoNotSchedule would block scale-out on a full pool" >&2
  exit 1
}
grep -q 'app.kubernetes.io/name: rio-store' <<<"$tsc" || {
  echo "FAIL: store spread labelSelector does not select the store pods" >&2
  exit 1
}

echo "OK"
