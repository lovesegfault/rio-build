# merged_bug_058 (the reason↔alert join, BOTH directions): the wave-8
# reason split seeded `ready_all_cells_ice_masked` into
# INTENT_DROP_REASONS — HELP declaring it "alert-worthy at low
# thresholds" — while the only alert expr exact-matched the disjoint
# cold-start sibling: a live_050(a) recurrence paged NOTHING. The
# forward direction (every alert-expr-referenced counter is seeded)
# was already pinned by ALERT_SEEDED_COUNTERS + the parity test; the
# REVERSE direction (every alert-worthy reason has a referencing
# expr) had no check, so seeding an orphaned reason passed silently.
#
# This fragment pins the join bidirectionally over the closed reason
# set, machine-derived from the staged rio-controller source (the
# const is the single source; hand-listing reasons here would be the
# author-census R15 anti-shape):
#
#   (i)   TOTALITY: every INTENT_DROP_REASONS value has exactly one
#         disposition row below (alert:<name> | none:<why>) — a NEW
#         reason fails this fragment until an explicit alert decision
#         is recorded;
#   (ii)  FORWARD: every `reason=` value referenced by an
#         intent_dropped_total alert expr is a member of the const
#         (no alert on a phantom reason — it would never fire);
#   (iii) REVERSE: every reason whose disposition is `alert:` is
#         matched by >= 1 rendered expr (the merged_bug_058 shape).
#
# Self-test arms run FIRST (the census_enrollment.py / 34-contracts
# house pattern): a check that cannot fail its planted fixtures does
# not gate. Arm A plants the merged_bug_058 shape itself (alert-
# disposition reason with no expr); arm B a phantom expr reason; arm
# C an undispositioned reason.
#
# Harness contract: runner cd's into $TMPDIR/chart, bash -euo pipefail.
# The controller source is staged by misc-checks.nix as
# .observability-source.rs (the 22-alert-quality runbook precedent).

obs_src=$TMPDIR/chart/.observability-source.rs
[ -f "$obs_src" ] || { echo "FAIL: staged observability source missing" >&2; exit 1; }

# --- the disposition table (the committed decision record) -----------
# reason<TAB>disposition. `alert:` rows are checked against rendered
# exprs; `none:` rows carry the recorded why (the explicit decision
# this fragment exists to force).
dispositions=$TMPDIR/42-dispositions.tsv
cat >"$dispositions" <<'EOF'
all_cells_ice_masked	alert:RioNodeclaimPoolAllCellsIceMasked
ready_all_cells_ice_masked	alert:RioNodeclaimPoolReadyAllCellsIceMasked
no_hosting_class	alert:RioNodeclaimPoolNoHostingClass
no_pool_covers	alert:RioNodeclaimPoolNoHostingClass
exceeds_cell_cap	none: single-intent producer/config anomaly (override-bypass hole); visible in the drop tally + cover WARN; per-intent pages would be noise without an incident class
unknown_hw_class	none: <=300s self-healing GetHwClassConfig skew; a persistent rate is the hw_refresh failure trace read off the counter; promoting to a page awaits an incident class
EOF

# --- extraction -------------------------------------------------------
extract_reasons() { # <observability.rs> -> one reason per line
  sed -n '/pub const INTENT_DROP_REASONS/,/^];/p' "$1" \
    | grep -o '"[a-z_]*"' | tr -d '"'
}

extract_expr_reasons() { # <rendered-rules.yaml> -> one reason per line
  # Every reason= matcher on the intent_dropped_total family; regex
  # matchers (reason=~"a|b") expand to their alternatives.
  grep -o 'nodeclaim_intent_dropped_total{reason=~\?"[a-z_|]*"' "$1" \
    | sed 's/.*"\([a-z_|]*\)"/\1/' | tr '|' '\n' | sort -u
}

check_join() { # <reasons-file> <expr-reasons-file> <dispositions-tsv>
  local reasons=$1 expr_reasons=$2 table=$3 rc=0
  # (i) totality: every reason has a disposition row.
  while read -r reason; do
    if ! cut -f1 "$table" | grep -qx "$reason"; then
      echo "FAIL(i): reason '$reason' has no disposition row — record alert:<name> or none:<why>" >&2
      rc=1
    fi
  done <"$reasons"
  # stale table rows (the burn-down face): a row whose reason left
  # the const is dead policy text.
  while IFS=$'\t' read -r reason _; do
    if ! grep -qx "$reason" "$reasons"; then
      echo "FAIL(i): disposition row for '$reason' but the const no longer carries it — remove the stale row" >&2
      rc=1
    fi
  done <"$table"
  # (ii) forward: every expr reason is a const member.
  while read -r reason; do
    [ -n "$reason" ] || continue
    if ! grep -qx "$reason" "$reasons"; then
      echo "FAIL(ii): alert expr references reason '$reason' which is not in INTENT_DROP_REASONS — the alert can never fire" >&2
      rc=1
    fi
  done <"$expr_reasons"
  # (iii) reverse: every alert-disposition reason is expr-matched.
  while IFS=$'\t' read -r reason disposition; do
    case "$disposition" in
      alert:*)
        if ! grep -qx "$reason" "$expr_reasons"; then
          echo "FAIL(iii): reason '$reason' is dispositioned ${disposition} but no rendered expr matches it (the merged_bug_058 orphan shape)" >&2
          rc=1
        fi
        ;;
      none:*) ;;
      *)
        echo "FAIL: disposition for '$reason' is neither alert:<name> nor none:<why>: '$disposition'" >&2
        rc=1
        ;;
    esac
  done <"$table"
  return $rc
}

# --- self-test arms (planted, must fail) ------------------------------
plant=$TMPDIR/42-plant
mkdir -p "$plant"
printf 'orphaned_reason\n' >"$plant/reasons"
: >"$plant/expr_reasons"
printf 'orphaned_reason\talert:RioPlantedOrphan\n' >"$plant/table"
if check_join "$plant/reasons" "$plant/expr_reasons" "$plant/table" 2>/dev/null; then
  echo "FAIL: self-test arm A (alert-dispositioned reason with no expr) did not fail" >&2
  exit 1
fi
printf 'real_reason\n' >"$plant/reasons"
printf 'phantom_reason\n' >"$plant/expr_reasons"
printf 'real_reason\tnone: recorded\n' >"$plant/table"
if check_join "$plant/reasons" "$plant/expr_reasons" "$plant/table" 2>/dev/null; then
  echo "FAIL: self-test arm B (expr on a phantom reason) did not fail" >&2
  exit 1
fi
printf 'undispositioned\n' >"$plant/reasons"
: >"$plant/expr_reasons"
: >"$plant/table"
if check_join "$plant/reasons" "$plant/expr_reasons" "$plant/table" 2>/dev/null; then
  echo "FAIL: self-test arm C (reason with no disposition row) did not fail" >&2
  exit 1
fi

# --- the real check ----------------------------------------------------
mon=$TMPDIR/42-mon.yaml
helm template rio . --set global.image.tag=test \
  --set monitoring.enabled=true --set buildScheduler.enabled=true >"$mon"

rules=$TMPDIR/42-rules.yaml
yq -N 'select(.kind == "PrometheusRule")' "$mon" >"$rules"
[ -s "$rules" ] || { echo "FAIL: no PrometheusRule rendered" >&2; exit 1; }

reasons=$TMPDIR/42-reasons.txt
extract_reasons "$obs_src" >"$reasons"
[ -s "$reasons" ] || { echo "FAIL: extracted zero reasons from the staged source — the extraction regex rotted" >&2; exit 1; }
# Premise pin: the const had 6 members when this fragment landed; a
# shrink below that means the extraction broke, not policy.
[ "$(grep -c . "$reasons")" -ge 6 ] || { echo "FAIL: fewer reasons extracted than the landing floor (6)" >&2; exit 1; }

expr_reasons=$TMPDIR/42-expr-reasons.txt
extract_expr_reasons "$rules" >"$expr_reasons"

check_join "$reasons" "$expr_reasons" "$dispositions"
echo "reason-alert-sync: $(grep -c . "$reasons") reasons, $(grep -c . "$expr_reasons") expr-referenced, join verified both directions"
