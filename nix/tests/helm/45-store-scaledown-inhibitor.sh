# D2 + live_056-c (bughunt-9 W9-BT/W9-CS, the structural faces): the
# abort-aware fast collapse fires only when abort evidence and QUIET
# demand-side telemetry AGREE. KEDA/HPA semantics make the conjunction
# structural — desired = max over triggers — so the four W9-CS cells
# map onto the rendered object:
#   (i)   outage (zero work, nonzero client retries): the inhibitor
#         trigger demands ceil(rate/threshold) replicas — desired holds;
#   (ii)  terminal cancel DURING the outage: work triggers go to zero
#         but the inhibitor still holds — the fast collapse does NOT
#         fire (max semantics; the AGREE-conjunction's defining cell);
#   (iii) release edge: retries cease => the inhibitor's demand is 0;
#         after the 300s window the Percent-100/60s policy actuates
#         monotone descent to floor (the SLO's T<=5min);
#   (iv)  same plane / same instance: ONE ScaledObject carries both
#         the fast path and the inhibitor — non-deadlock by metricType
#         max, not by coordination.
# This fragment pins the faces a render can certify; the live KEDA
# timeline is the committed incident capture (the commit body's
# [GEN-SET] arm — k3s VM tests run no KEDA operator by design).
#
# Pre-fix RED (the shipped truth): no demand-plane trigger existed —
# the outage read as idleness (live_056-c's 8->4).

out=$TMPDIR/store-inhibitor.yaml
helm template rio . --set global.image.tag=test >"$out"

trig=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.triggers' "$out")
test -n "$trig" || {
  echo "FAIL: rio-store ScaledObject did not render — inhibitor assertion vacuous" >&2
  exit 1
}

# (i)/(ii): the inhibitor trigger exists, queries the CLIENT-plane
# retry series (rate over the gateway's class-labeled emit-law
# counter — merged_bug_038: the aborted-only counter left the
# trigger structurally flat during reachability outages), and
# nothing else.
grep -q 'rio_gateway_putpath_retry_events_total' <<<"$trig" || {
  echo "FAIL: no demand-side inhibitor trigger on the class-labeled emit-law counter — a reachability outage reads as idleness (live_056-c/merged_bug_038)" >&2
  exit 1
}
grep -q 'sum(rate(rio_gateway_putpath_retry_events_total\[2m\]))' <<<"$trig" || {
  echo "FAIL: inhibitor query drifted from the client-plane rate shape" >&2
  exit 1
}
# Query strings ONLY for the negative pin — $trig carries the
# template's narration comments (helm preserves them), which
# legitimately cite the old counter as history.
neg_queries=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.triggers[] | select(.type=="prometheus") | .metadata.query' "$out")
if grep -q 'rio_gateway_putpath_aborted_retries_total' <<<"$neg_queries"; then
  echo "FAIL: a trigger query still consumes the aborted-only counter — that series is structurally flat during reachability outages (merged_bug_038); it persists for the I-168 dashboard only" >&2
  exit 1
fi
# Probe-immunity (the law's population clause): the inhibitor must
# NOT be built from health/SYN/accept-level series — those are
# permanently nonzero under kubelet probing and would veto the Q6
# fast collapse forever.
if grep -qE 'grpc_health|grpc\.health|tcp_accept|syn' <<<"$trig"; then
  echo "FAIL: inhibitor query counts platform health traffic — the release edge becomes unreachable" >&2
  exit 1
fi

# Threshold = the named violable knob (values.yaml), wired verbatim.
thr=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.triggers[] | select(.metadata.query == "sum(rate(rio_gateway_putpath_retry_events_total[2m]))") | .metadata.threshold' "$out")
test "$thr" = "0.05" || {
  echo "FAIL: inhibitor threshold ($thr) != targetRetryAttemptsPerReplica default (0.05) — the named const is not wired" >&2
  exit 1
}

# (iii): the actuated-release machinery — 300s quiet consensus then
# one-pass descent.
sd=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown' "$out")
grep -q 'stabilizationWindowSeconds: 300' <<<"$sd" || {
  echo "FAIL: scaleDown window is not the 300s N-scrape debounce — the floor SLO (T<=5min) is unreachable" >&2
  exit 1
}
grep -q 'value: 100' <<<"$sd" || {
  echo "FAIL: scaleDown lost the Percent-100 fast-collapse policy" >&2
  exit 1
}

# (iv): one object, both planes — the work triggers AND the inhibitor
# ride the same ScaledObject (max semantics joins them).
for q in 'rio_scheduler_substituting_derivations' 'rio_scheduler_open_attempts'; do
  grep -q "$q" <<<"$trig" || {
    echo "FAIL: work trigger $q missing — the AGREE conjunction needs both planes on one object" >&2
    exit 1
  }
done

# merged_bug_038 (W10-BV, the per-class contract legs): the inhibitor
# consumes the gateway's class-labeled PutPath retry counter — the
# H8″ emit-law contract: Aborted / Unavailable / DeadlineExceeded
# each an emitting arm at the client chokepoint. The render-level
# halves pinned here: (a) the query SUMS ACROSS the class label (a
# class= selector re-creates the aborted-only blindness that left
# the trigger structurally flat during reachability outages), and
# (b) the trigger narration names EVERY class of the alphabet (the
# lexicon bind). The series-moves half lives crate-side beside the
# emit law (inject each guarded class ⇒ the series moves ⇒ the
# inhibitor holds the floor mid-outage).
all_queries=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.triggers[] | select(.type=="prometheus") | .metadata.query' "$out")
case "$all_queries" in
  *class=*)
    echo "FAIL: a prometheus trigger narrows by class= — the inhibitor must aggregate the whole typed retry alphabet" >&2
    exit 1
    ;;
esac
# The landed alphabet's label values (rio_common code_class_label —
# lowercase snake): the narration must name each guarded class AS
# THE WIRE-FACING LABEL the query aggregates over.
for cls in aborted unavailable deadline_exceeded resource_exhausted; do
  grep -q "$cls" templates/store-scaledobject.yaml || {
    echo "FAIL: trigger narration does not name typed retry class label '$cls' (the emit-law bind, merged_bug_038)" >&2
    exit 1
  }
done

# merged_bug_013 (W10-BU): the damping header binds to the RENDERED
# scaleDown spec — quantified prose contradicting the spec in the
# same file was the r23-drift shape (the retired 1800s/25%-per-600s
# ladder narrated as current). The R23' lexicon seed rows.
w=$(yq -N 'select(.kind=="ScaledObject" and .metadata.name=="rio-store") | .spec.advanced.horizontalPodAutoscalerConfig.behavior.scaleDown.stabilizationWindowSeconds' "$out")
grep -qE "^# .*${w}s stabilization window" templates/store-scaledobject.yaml || {
  echo "FAIL: the scaledobject header does not narrate the rendered ${w}s scaleDown window" >&2
  exit 1
}
grep -qE '^# .*Percent-100/60s' templates/store-scaledobject.yaml || {
  echo "FAIL: the scaledobject header does not narrate the rendered Percent-100/60s collapse policy" >&2
  exit 1
}
# Any surviving 1800s mention must be explicitly historical.
stale_1800=$(grep -nE '1800s' templates/store-scaledobject.yaml | grep -vE 'retired|old 1800s' || true)
test -z "$stale_1800" || {
  echo "FAIL: 1800s damping narrated as current (must carry the retired/historical qualifier):" >&2
  echo "$stale_1800" >&2
  exit 1
}

echo "OK: demand-side inhibitor trigger + 300s/Percent-100 fast collapse on one ScaledObject (AGREE by max-over-triggers)"
