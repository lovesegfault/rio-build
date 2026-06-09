# Unconsumed values keys — a knob no template reads fails CI (bug_181).
#
# `helm upgrade --set controller.execRetentionDays=90` rendered and
# deployed cleanly while changing NOTHING: the knob was defined three
# times in values.yaml (controller/scheduler/gateway, identical
# authoritative-looking 9-line comment) but only the scheduler copy is
# consumed. A dead knob with authoritative documentation is worse than
# no knob — the successful upgrade masks the no-op. This lint makes the
# class structural: every scalar leaf in values.yaml must be reachable
# from a template `.Values.` reference (or an allowlisted prefix with a
# written justification), so a values key consumed by no template fails
# here instead of rendering as a silent no-op.
#
# Extraction rules (fail-closed by construction):
#  - direct refs `.Values.a.b` mark the `a.b` SUBTREE consumed (covers
#    `index $.Values.namespaces .namespaceRef` dynamic access and
#    `with .Values.x` bodies via their target);
#  - `$v := .Values.a` bindings consume NOTHING by themselves; only
#    dotted uses `$v.b` join to `a.b`, and the join is PER TEMPLATE
#    FILE (merged_bug_328: the chart really rebinds `$s`/`$a` to
#    different subtrees in different files — a global join let
#    scheduler.yaml's `$s.replicas` bless a dead store twin, exactly
#    the duplicated-dead-knob class this lint exists for). An
#    opaquely-passed binding (helper include) deliberately consumes
#    nothing — keys read only through such passes must be allowlisted
#    with a justification. Strictness is the point: the failure mode
#    is a loud false red that forces a reviewable allowlist line,
#    never a silent bless.
#  - helm comments are stripped in BOTH spellings — `{{/* … */}}` and
#    the trim forms `{{- /* … */ -}}` (every templates/*.yaml comment
#    uses the trim form; merged_bug_328's no-op stripper keyed on the
#    literal form only) — and `#` YAML comment tails are stripped, so
#    a knob mentioned only in prose counts as UNconsumed.
#  - every allowlist entry must match at least one otherwise-unconsumed
#    leaf (census: a dead allowlist entry is itself an error).
#
# Self-test (no fail-open enforcement; banner a — one planted red per
# production shape): (1) a dead key in a values copy; (2) a colliding
# `$var` rebind across files whose dead twin a global join would
# bless; (3) keys mentioned ONLY in a trim-spelling helm comment or a
# `#` prose line.
#
# Natural red, pre-fix (the lint's first run against the live chart):
#   controller.execRetentionDays
#   gateway.execRetentionDays
#   postgresql.primary.persistence.size   ← allowlisted (subchart)
#
# (documentary — .sh is not tracey-scanned.)

root=$PWD
T=$TMPDIR/values-unconsumed
rm -rf "$T"; mkdir -p "$T"

# Allowlist: prefix → justification. Keys consumed outside our
# templates (subcharts, helpers fed by opaque passes) live here, each
# with a reason a reviewer can audit.
cat > "$T/allowlist.json" <<'EOF'
[
  {
    "prefix": "postgresql.",
    "why": "bitnami subchart values — consumed by charts/postgresql templates, not ours; condition-gated by postgresql.enabled"
  }
]
EOF

# ── extraction (shared by self-test and real run) ──
strip_comments() {
  # helm comments in literal AND trim spellings, multi-line aware,
  # then `#` YAML comment tails (line start or after whitespace — a
  # `.Values.` ref can never contain `#`, so over-stripping only
  # UNDER-counts refs: the fail-closed direction).
  awk '
    {
      line=$0; out=""
      while (1) {
        if (inc) {
          if (match(line, /\*\/ *-?\}\}/)) { line = substr(line, RSTART+RLENGTH); inc=0; continue }
          line=""; break
        }
        if (match(line, /\{\{-? *\/\*/)) { out = out substr(line, 1, RSTART-1); line = substr(line, RSTART+RLENGTH); inc=1; continue }
        out = out line; break
      }
      print out
    }' "$1" | sed -E 's/(^|[[:space:]])#.*$/\1/'
}

extract_unconsumed() {
  chartdir=$1; out=$2
  # leaves: structural scalar paths, numeric (array) segments dropped
  yq -o=json "$chartdir/values.yaml" \
    | jq -r 'paths(scalars) | map(select(type=="string")) | join(".")' \
    | sed '/^$/d' | sort -u > "$T/leaves"
  # refs globally; binds and `$var.path` joins PER TEMPLATE FILE
  # (merged_bug_328 — Helm variable scope is the file/define, never
  # the chart-wide concatenation).
  : > "$T/refs"; : > "$T/joins"
  for tf in "$chartdir"/templates/*.yaml "$chartdir"/templates/*.tpl; do
    [ -e "$tf" ] || continue
    strip_comments "$tf" > "$T/tf0"
    # binding RHS removed so `$v := .Values.a` does not count as a ref
    sed -E 's/:= *\$?\.Values\.[A-Za-z0-9_.]+//g' "$T/tf0" > "$T/tf1"
    # grep exits 1 on no-match and the runner is pipefail — most files
    # have no binds at all, so every pipeline head is || true guarded.
    { grep -oE '\.Values\.[A-Za-z0-9_.]+' "$T/tf1" || true; } | sed 's/^\.Values\.//' >> "$T/refs"
    { grep -oE '\$[A-Za-z0-9_]+ *:= *\$?\.Values\.[A-Za-z0-9_.]+' "$T/tf0" || true; } \
      | sed -E 's/\$([A-Za-z0-9_]+) *:= *\$?\.Values\./\1 /' | sort -u > "$T/tbinds"
    while read -r var path; do
      { grep -oE "\\\$$var\.[A-Za-z0-9_.]+" "$T/tf1" || true; } | sed "s/^\\\$$var\./$path./" >> "$T/joins"
    done < "$T/tbinds"
  done
  sort -u "$T/refs" "$T/joins" > "$T/consumed"
  awk -F. 'NR==FNR{c[$0]=1;next}{p="";ok=0;for(i=1;i<=NF;i++){p=(p==""?$i:p"."$i);if(p in c){ok=1;break}}if(!ok)print}' \
    "$T/consumed" "$T/leaves" > "$out"
}

# ── planted RED 1: a dead key in a values copy MUST be flagged ──
red=$TMPDIR/chart-181-red
rm -rf "$red"
cp -r . "$red"
printf '\nzzBughunt3DeadKnob: 1\n' >> "$red/values.yaml"
extract_unconsumed "$red" "$T/unconsumed-red"
grep -qx 'zzBughunt3DeadKnob' "$T/unconsumed-red" || {
  echo "FAIL: planted dead values key was NOT flagged — the lint is fail-open" >&2
  exit 1
}

# ── planted RED 2 (merged_bug_328): a `$var` rebound to a DIFFERENT
# subtree in another file must not bless this file's dead twin ──
red2=$TMPDIR/chart-328-collide
rm -rf "$red2"
cp -r . "$red2"
printf '\nzzCollideHost:\n  zzDeadLeaf: 1\nzzCollideOther:\n  zzDeadLeaf: 1\n' >> "$red2/values.yaml"
cat > "$red2/templates/zz-collide-a.yaml" <<'TPL'
{{- $zc := .Values.zzCollideHost }}
TPL
cat > "$red2/templates/zz-collide-b.yaml" <<'TPL'
{{- $zc := .Values.zzCollideOther }}
zz: {{ $zc.zzDeadLeaf }}
TPL
extract_unconsumed "$red2" "$T/unconsumed-red2"
grep -qx 'zzCollideHost.zzDeadLeaf' "$T/unconsumed-red2" || {
  echo "FAIL: colliding \$var rebind cross-blessed a dead twin key — joins must be per template file" >&2
  exit 1
}

# ── planted RED 3 (merged_bug_328): trim-spelling helm comments and
# `#` prose must not count as consumption ──
red3=$TMPDIR/chart-328-comment
rm -rf "$red3"
cp -r . "$red3"
printf '\nzzCommentOnlyA: 1\nzzCommentOnlyB: 1\n' >> "$red3/values.yaml"
cat > "$red3/templates/zz-comment.yaml" <<'TPL'
{{- /* .Values.zzCommentOnlyA is documented here only */ -}}
# prose mention: .Values.zzCommentOnlyB
TPL
extract_unconsumed "$red3" "$T/unconsumed-red3"
grep -qx 'zzCommentOnlyA' "$T/unconsumed-red3" || {
  echo "FAIL: a key mentioned only in a {{- /* trim-spelling */ -}} comment counted as consumed" >&2
  exit 1
}
grep -qx 'zzCommentOnlyB' "$T/unconsumed-red3" || {
  echo "FAIL: a key mentioned only in a '#' YAML comment counted as consumed" >&2
  exit 1
}

# ── real chart ──
extract_unconsumed "$root" "$T/unconsumed-raw"

# apply allowlist; every entry must be live (census)
jq -r '.[].prefix' "$T/allowlist.json" > "$T/allowprefixes"
: > "$T/unconsumed"
: > "$T/allowused"
while read -r leaf; do
  hit=""
  while read -r pfx; do
    case "$leaf" in "$pfx"*) hit=1; echo "$pfx" >> "$T/allowused"; break;; esac
  done < "$T/allowprefixes"
  [ -z "$hit" ] && echo "$leaf" >> "$T/unconsumed"
done < "$T/unconsumed-raw"
touch "$T/unconsumed" "$T/allowused"

if [ -s "$T/unconsumed" ]; then
  echo "FAIL: values.yaml keys consumed by NO template (dead knobs — delete them" >&2
  echo "      or allowlist with a justification in 36-values-unconsumed-keys.sh):" >&2
  sed 's/^/        /' "$T/unconsumed" >&2
  exit 1
fi
while read -r pfx; do
  grep -qx "$pfx" "$T/allowused" || {
    echo "FAIL: allowlist entry '$pfx' matched nothing — dead allowlist entries" >&2
    echo "      hide future regressions; remove it or fix the extraction" >&2
    exit 1
  }
done < "$T/allowprefixes"
