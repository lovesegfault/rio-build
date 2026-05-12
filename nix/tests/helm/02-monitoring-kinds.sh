# monitoring-on: ServiceMonitor/PodMonitor/PrometheusRule templates are
# gated and otherwise never rendered by CI.

out=$TMPDIR/monitoring.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true >"$out"

for k in ServiceMonitor PodMonitor PrometheusRule; do
  grep -qx "kind: $k" "$out" || {
    echo "FAIL: monitoring.enabled=true did not render kind: $k" >&2
    exit 1
  }
done

# r33 bug_001: RioNodeclaimPoolStuckPending's threshold MUST be derived
# from the per-cell lead-time gauge (a hardcoded `> 90` false-fires for
# the entire 7-15min metal boot window). Tripwire against a future
# "simplification" that drops the join and silently re-globalizes the
# threshold — the §SCC sweep miss this alert was last bitten by.
#
# r39: capture grep -A4 output before grep -q. Same SIGPIPE shape as
# the yq | grep -q pipes swept in 12-priorityclass.sh — `grep -q`
# exits at first match → producer SIGPIPE (141) → pipefail flags the
# pipeline → false-positive FAIL. The here-string avoids the pipe.
# `|| true` keeps `set -e` from aborting before the diagnostic when
# the alert is missing (grep -A4 exits 1 on no match — the inner
# grep -q on empty input then exits 1 and the FAIL block fires).
sp_block=$(grep -A4 'alert: RioNodeclaimPoolStuckPending' "$out" || true)
grep -q 'rio_controller_nodeclaim_ice_timeout_seconds' <<<"$sp_block" || {
  echo "FAIL: RioNodeclaimPoolStuckPending expr does not join on" \
    "rio_controller_nodeclaim_ice_timeout_seconds — must anchor on the" \
    "reaper's actual threshold, not a lead_time proxy that can fall" \
    "below 2×seed (r41 bug_026; supersedes r33 bug_001's lead_time join)" >&2
  exit 1
}
# r41 bug_026: `lead_time_seconds` must NOT appear in the StuckPending
# expr. It's `q_0.9(boot − eta)` which learns DOWN with no floor at the
# seed — once `< (2/3)×seed`, `3 × lead_time < 2×seed` and the alert
# pages while the reaper is in its grace period. `! grep ... ||` so the
# pass case (grep exits 1 → ! → 0) doesn't trip `set -e`.
! grep -q 'rio_controller_nodeclaim_lead_time_seconds' <<<"$sp_block" || {
  echo "FAIL: RioNodeclaimPoolStuckPending expr still references" \
    "lead_time_seconds — that anchor diverges from the reaper's 2×seed" \
    "floor (r41 bug_026)" >&2
  exit 1
}

# r34 bug_019 (§Verifier-one-step-removed inverse of r33 bug_001):
# the StuckPending threshold sits above the reaper's 2×seed; a
# successfully-reaped boot-timeout loop is silent to it. The sibling
# alert keys on the reap rate, covering the false-negative arm.
grep -q 'alert: RioNodeclaimPoolBootTimeoutLoop' "$out" || {
  echo "FAIL: RioNodeclaimPoolBootTimeoutLoop alert missing —"   \
    "the StuckPending threshold (2×ice_timeout, cap from maxLeadTime)"   \
    "sits above the reaper's 2×seed boot-timeout reap; a sustained" \
    "mint→reap loop is silent without this sibling. (r34 bug_019)" >&2
  exit 1
}
btl_block=$(grep -A4 'alert: RioNodeclaimPoolBootTimeoutLoop' "$out" || true)
grep -q 'reason="boot-timeout"' <<<"$btl_block" || {
  echo "FAIL: BootTimeoutLoop expr does not key on reason=boot-timeout" >&2
  exit 1
}

# r35 B1 (bug_003 second half): the no_hosting_class alert is the ONLY
# signal that an intent dropped at `fallback_cell` left a pod
# permanently Pending — every other nodeclaim alert is NodeClaim-derived
# and a never-minted NodeClaim emits no series. bug_003's whole shape
# was "no alert fires"; a future §SCC sweep silently dropping THIS alert
# repeats the failure mode. Tripwire pins both the alert name and the
# `reason="no_hosting_class"` key so a refactor can't keep the alert and
# disarm the expr.
grep -q 'alert: RioNodeclaimPoolNoHostingClass' "$out" || {
  echo "FAIL: RioNodeclaimPoolNoHostingClass alert missing — when no" \
    "configured hw-class hosts an intent the pod is permanently Pending" \
    "and NOTHING fires; bug_003's no-alert half regresses silently" \
    "(r35 B1)" >&2
  exit 1
}
nhc_block=$(grep -A4 'alert: RioNodeclaimPoolNoHostingClass' "$out" || true)
grep -q 'no_hosting_class' <<<"$nhc_block" || {
  echo "FAIL: NoHostingClass expr does not key on reason=no_hosting_class" >&2
  exit 1
}
# r41 merged_015: the alert is THE only signal that a SpawnIntent dropped
# without minting a NodeClaim. `no_pool_covers` is the same Pending-forever
# outcome via the Pool-coverage filter — it must be in the same alert.
grep -q 'no_pool_covers' <<<"$nhc_block" || {
  echo "FAIL: NoHostingClass expr does not also cover reason=no_pool_covers" \
       "— a hwClass with no covering Pool is the same Pending-forever shape" \
       "(r41 merged_015)" >&2
  exit 1
}

# Sibling tripwire for the OTHER no-NodeClaim-ever-minted reason: every
# cell that could host the intent is ICE-masked. Same outcome (build's
# drv permanently Ready and unroutable, no NodeClaim, no Job), opposite
# operator action (fix the cloud, don't touch [sla.hw_classes]).
# Deliberately a SEPARATE alert from NoHostingClass: ICE drops self-heal
# on TTL expiry, so `for: 15m` (vs NoHostingClass `for: 5m`) avoids
# false-firing on multi-cell spot blips. Folding it into NoHostingClass's
# `for: 5m` would page on every transient capacity dip.
grep -q 'alert: RioNodeclaimPoolAllCellsIceMasked' "$out" || {
  echo "FAIL: RioNodeclaimPoolAllCellsIceMasked alert missing — when every" \
    "hosting cell is ICE-masked (NodeClaim launches failing in the cloud)" \
    "the build stalls and NOTHING fires; same no-alert shape as bug_003" >&2
  exit 1
}
icem_block=$(grep -A4 'alert: RioNodeclaimPoolAllCellsIceMasked' "$out" || true)
grep -q 'all_cells_ice_masked' <<<"$icem_block" || {
  echo "FAIL: AllCellsIceMasked expr does not key on reason=all_cells_ice_masked" >&2
  exit 1
}

# r34 bug_017 (§Partition-single-source): the StuckPending clamp cap
# must derive from `maxLeadTime`, not a hardcoded literal — the
# invariant `cap >= 2×maxLeadTime` is load-bearing (the alert must
# fire AFTER the reaper, which acts at 2×seed<=2×maxLeadTime).
# r35 merged_bug_027: BootTimeoutLoop's increase() window is
# `4×maxLeadTime + 4×TICK` = 2 full reap cycles + tick-grid slack
# (StuckPending stays `3×`) — the r34 `3×` window spanned only 1.5
# cycles when seed=maxLeadTime (metal cells), giving `>= 2` a ~50% duty
# cycle and a flapping alert. r43 bug_024: a reap cycle costs
# `2×seed + 2×TICK` (one tick to observe the reap, one for FFD to stop
# placing on the dying claim before re-mint), so the bare `4×maxLeadTime`
# under-spans by `4×TICK`; add the slack so `>= 2` holds for the loop.
expected_cap=$(( 3 * $(yq '.scheduler.sla.maxLeadTime' values.yaml) ))
expected_window=$(( 4 * $(yq '.scheduler.sla.maxLeadTime' values.yaml) + 40 ))
# Mirror the {{ max 90 ... }} floor in the template: a maxLeadTime < 30
# would render a window/cap < 90, which is degenerate (Prometheus rejects
# `[0s]` ranges; clamp(v, min, max) returns empty when min > max).
[ "$expected_cap" -lt 90 ] && expected_cap=90
[ "$expected_window" -lt 90 ] && expected_window=90
# Re-extract: $out hasn't changed since L20/L39 but local re-capture
# keeps each assertion self-contained — a future re-order can't go
# stale on the variable.
sp_block=$(grep -A4 'alert: RioNodeclaimPoolStuckPending' "$out" || true)
grep -q "clamp(.*, 90, ${expected_cap})" <<<"$sp_block" || {
  echo "FAIL: RioNodeclaimPoolStuckPending clamp cap != 3×maxLeadTime"  \
    "(expected ${expected_cap}; cap must derive from maxLeadTime so"     \
    "raising it never disarms the reaper-failed signal)" >&2
  exit 1
}
btl_block=$(grep -A4 'alert: RioNodeclaimPoolBootTimeoutLoop' "$out" || true)
grep -q "\[${expected_window}s\]" <<<"$btl_block" || {
  echo "FAIL: BootTimeoutLoop increase() window != 4×maxLeadTime + 4×TICK"  \
    "(expected [${expected_window}s]; window must span 2 full reap cycles"  \
    "+ tick-grid slack = 2×(2×seed + 2×TICK) <= 4×maxLeadTime + 4×TICK so" \
    ">= 2 reaps holds for the entire sustained loop, not just ~50% of it;"  \
    "r35 merged_bug_027, r43 bug_024)" >&2
  exit 1
}
