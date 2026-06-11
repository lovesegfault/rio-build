# D4 (bughunt-9 W9-BF): the store budget/limit law at the chart tier.
# The shipped defect was the IDENTITY — deploy.rs set limits.memory =
# "32Gi" while the binary's default NAR budget is 8 × MAX_NAR_SIZE =
# 32 GiB: budget == limit exactly, non-NAR reserve zero, so the budget
# semaphore admitted NAR bytes the cgroup OOM-kills on. The law
# (enforced by Config::validate at boot, satisfied by construction at
# the xtask set-site, pinned here for the chart-default pair):
#
#   RIO_NAR_BUFFER_BUDGET_BYTES + RESERVE <= limits.memory
#
# RESERVE = rio_common::limits::STORE_NON_NAR_RESERVE_BYTES (4 GiB =
# chunk cache 2 GiB + log ingest 1 GiB + runtime slack 1 GiB); the
# rio-common unit `store_reserve_terms_sum_and_budget_relation` pins
# the constant — change both together.
#
# Pre-fix RED (the shipped truth, quoted in the landing commit): the
# chart rendered NO budget env (binary default 32 GiB) against a 4Gi
# default limit — 32 GiB + 4 GiB > 4 Gi.

RESERVE=4294967296    # 4 GiB — STORE_NON_NAR_RESERVE_BYTES
NAR_FLOOR=4294967296  # 4 GiB — MAX_NAR_SIZE (validate()'s budget floor)

out=$TMPDIR/store-memory-identity.yaml
helm template rio . --set global.image.tag=test >"$out"

# Premise guard: the store Deployment renders at defaults.
dep=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")' "$out")
test -n "$dep" || {
  echo "FAIL: rio-store Deployment did not render at chart defaults — law assertion vacuous" >&2
  exit 1
}

budget=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")
  | .spec.template.spec.containers[0].env[]
  | select(.name=="RIO_NAR_BUFFER_BUDGET_BYTES") | .value' "$out" | tr -d '"')
limit=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")
  | .spec.template.spec.containers[0].resources.limits.memory' "$out")
limit_src=$(yq -N 'select(.kind=="Deployment" and .metadata.name=="rio-store")
  | .spec.template.spec.containers[0].env[]
  | select(.name=="RIO_MEMORY_LIMIT_BYTES") | .valueFrom.resourceFieldRef.resource' "$out")

case "$budget$limit" in
*null* | "")
  echo "FAIL: could not read RIO_NAR_BUFFER_BUDGET_BYTES ($budget) / limits.memory ($limit) from the render" >&2
  exit 1
  ;;
esac

# The validate() arm only fires if the limit is actually visible to the
# binary: the downward-API env must reference limits.memory.
test "$limit_src" = "limits.memory" || {
  echo "FAIL: RIO_MEMORY_LIMIT_BYTES is not downward-API-injected from limits.memory (got: $limit_src) — the boot-time law is blind" >&2
  exit 1
}

# Parse the k8s quantity (the chart uses whole-Gi limits; anything else
# is a deliberate change that should update this fragment).
case "$limit" in
*Gi)
  limit_bytes=$((${limit%Gi} * 1024 * 1024 * 1024))
  ;;
*)
  echo "FAIL: store limits.memory ($limit) is not a whole-Gi quantity" >&2
  exit 1
  ;;
esac

test "$budget" -ge "$NAR_FLOOR" || {
  echo "FAIL: chart-default NAR budget ($budget) is below the MAX_NAR_SIZE floor ($NAR_FLOOR) — validate() refuses this at boot" >&2
  exit 1
}

if [ $((budget + RESERVE)) -gt "$limit_bytes" ]; then
  echo "FAIL: D4 law violated at chart defaults: budget ($budget) + reserve ($RESERVE) > limits.memory ($limit = $limit_bytes bytes)." >&2
  echo "      A pod whose NAR semaphore admits more bytes than its cgroup allows OOM-kills instead of parking." >&2
  echo "      Raise store.resources.limits.memory or lower store.narBufferBudgetBytes (floor 4 GiB)." >&2
  exit 1
fi

echo "OK: store budget ($budget) + reserve ($RESERVE) <= limit ($limit); downward-API limit env wired"
