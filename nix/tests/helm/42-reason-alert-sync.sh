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
# C an undispositioned reason. Arms D/E (bug_111) gate the EXTRACTION
# layer itself with raw-source plants — the wave-9 arms entered one
# layer short (pre-extracted fixtures certified only the join):
# arm D a leading-sibling-label + regex-metachar expr (the old
# `{reason`-anchored grep was blind to any expr whose reason matcher
# is not the first label); arm E a digit-bearing const member (the
# old `"[a-z_]*"` class dropped it silently — totality bypassed).
#
# EXTRACTION DERIVES FROM OWNED MACHINERY (bug_111): the reason set
# comes from the shared rust lexer's const-array span primitive
# (.rust-strip.py --const-strings, staged by the driver) — digits and
# escapes live by construction, comments excluded by the lexer's own
# classification, never by a hand regex over source text. The expr
# side parses the WHOLE matcher block, label-position-independent.
#
# Harness contract: runner cd's into $TMPDIR/chart, bash -euo pipefail.
# The controller source is staged by misc-checks.nix as
# .observability-source.rs (the 22-alert-quality runbook precedent);
# the shared lexer as .rust-strip.py (python3 in the driver inputs).

obs_src=$TMPDIR/chart/.observability-source.rs
[ -f "$obs_src" ] || { echo "FAIL: staged observability source missing" >&2; exit 1; }
lexer=$TMPDIR/chart/.rust-strip.py
[ -f "$lexer" ] || { echo "FAIL: staged shared lexer missing" >&2; exit 1; }

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
forecast_all_cells_ice_masked	none: the forecast half of the masked split (merged_bug_013) — no build waits yet, so the landed HELP says observe-don't-page; the ready sibling carries the page and the split exists to keep its calibration honest during routine ICE x forecast churn
unknown_hw_class	none: <=300s self-healing GetHwClassConfig skew; a persistent rate is the hw_refresh failure trace read off the counter; promoting to a page awaits an incident class
EOF

# --- extraction -------------------------------------------------------
extract_reasons() { # <observability.rs> -> one reason per line
  # Span-derived (bug_111): the lexer owns string/comment
  # classification and the const item's extent; the identifier
  # grammar is whatever the const carries — digit-bearing members
  # live (the old "[a-z_]*" class dropped them).
  python3 "$lexer" --const-strings INTENT_DROP_REASONS "$1"
}

extract_expr_reasons() { # <rendered-rules.yaml> -> one reason per line
  # Every reason= matcher on the intent_dropped_total family,
  # LABEL-POSITION-INDEPENDENT (bug_111): the whole `{...}` matcher
  # block is captured and the reason matcher found anywhere inside it
  # — a leading sibling label or a regex-metachar sibling matcher no
  # longer hides the expr from the join. Regex matchers
  # (reason=~"a|b") expand to their alternatives; the value class is
  # the identifier grammar (digits live).
  grep -o 'nodeclaim_intent_dropped_total{[^}]*}' "$1" \
    | sed -n 's/.*[{,] *reason=~\{0,1\}"\([a-z0-9_|]*\)".*/\1/p' \
    | tr '|' '\n' | grep -v '^$' | sort -u
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

# --- extraction-layer plants (bug_111 — raw source in, reasons out) ----
# Arm D: a rendered expr whose reason matcher sits BEHIND a sibling
# label, beside a regex-metachar sibling matcher — both shapes were
# invisible to the {reason-anchored grep (phantom-reason guarantee
# (ii) passed silently); and one first-position control row.
cat >"$plant/rules.yaml" <<'EOF'
      expr: rate(rio_controller_nodeclaim_intent_dropped_total{pool="general",reason="planted_sibling"}[5m]) > 0
      expr: rate(rio_controller_nodeclaim_intent_dropped_total{job=~"rio.*",reason=~"planted_meta|planted_alt"}[5m]) > 0
      expr: rate(rio_controller_nodeclaim_intent_dropped_total{reason="planted_first"}[5m]) > 0
EOF
extracted=$(extract_expr_reasons "$plant/rules.yaml" | tr '\n' ' ')
for want in planted_sibling planted_meta planted_alt planted_first; do
  case " $extracted " in
    *" $want "*) ;;
    *) echo "FAIL: self-test arm D (label-position/metachar extraction) lost '$want' — got: $extracted" >&2; exit 1 ;;
  esac
done

# Arm E: a digit-bearing const member must survive extraction (the
# old [a-z_]-only class silently dropped it — totality (i) bypassed),
# and a commented-out member must NOT leak in (the lexer's own
# string/comment classification, not a hand regex).
cat >"$plant/obs.rs" <<'EOF'
pub const INTENT_DROP_REASONS: &[&str] = &[
    "planted_alpha",
    // "planted_commented",
    "planted_v2",
];
EOF
got=$(python3 "$lexer" --const-strings INTENT_DROP_REASONS "$plant/obs.rs" | tr '\n' ' ')
[ "$got" = "planted_alpha planted_v2 " ] || {
  echo "FAIL: self-test arm E (digit-bearing/comment extraction) — got: '$got'" >&2
  exit 1
}

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
# Premise pin: the const had 6 members when this fragment landed and
# 7 at the wave-integrated tree (S4's forecast_all_cells_ice_masked
# joined); the floor only ratchets UP — a shrink below it means the
# extraction broke, not policy.
[ "$(grep -c . "$reasons")" -ge 7 ] || { echo "FAIL: fewer reasons extracted than the landing floor (7)" >&2; exit 1; }

expr_reasons=$TMPDIR/42-expr-reasons.txt
extract_expr_reasons "$rules" >"$expr_reasons"

check_join "$reasons" "$expr_reasons" "$dispositions"
echo "reason-alert-sync: $(grep -c . "$reasons") reasons, $(grep -c . "$expr_reasons") expr-referenced, join verified both directions"
