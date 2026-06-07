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
#    dotted uses `$v.b` join to `a.b`. An opaquely-passed binding
#    (helper include) deliberately consumes nothing — keys read only
#    through such passes must be allowlisted with a justification.
#    Strictness is the point: the failure mode is a loud false red
#    that forces a reviewable allowlist line, never a silent bless.
#  - every allowlist entry must match at least one otherwise-unconsumed
#    leaf (census: a dead allowlist entry is itself an error).
#
# Self-test (no fail-open enforcement): a planted dead key in a values
# copy must be flagged before the real check may gate.
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
extract_unconsumed() {
  chartdir=$1; out=$2
  # leaves: structural scalar paths, numeric (array) segments dropped
  yq -o=json "$chartdir/values.yaml" \
    | jq -r 'paths(scalars) | map(select(type=="string")) | join(".")' \
    | sed '/^$/d' | sort -u > "$T/leaves"
  # template text, {{/* ... */}} comments stripped (multi-line aware)
  cat "$chartdir"/templates/*.yaml "$chartdir"/templates/*.tpl | awk '
    {
      line=$0; out=""
      while (1) {
        if (inc) {
          e = index(line, "*/}}")
          if (e) { line = substr(line, e+4); inc=0; continue }
          line=""; break
        }
        s = index(line, "{{/*")
        if (s) { out = out substr(line,1,s-1); line = substr(line,s+4); inc=1; continue }
        out = out line; break
      }
      print out
    }' > "$T/text0"
  # binding RHS removed so `$v := .Values.a` does not count as a ref
  sed -E 's/:= *\$?\.Values\.[A-Za-z0-9_.]+//g' "$T/text0" > "$T/text"
  grep -oE '\.Values\.[A-Za-z0-9_.]+' "$T/text" | sed 's/^\.Values\.//' | sort -u > "$T/refs"
  grep -oE '\$[A-Za-z0-9_]+ *:= *\$?\.Values\.[A-Za-z0-9_.]+' "$T/text0" \
    | sed -E 's/\$([A-Za-z0-9_]+) *:= *\$?\.Values\./\1 /' | sort -u > "$T/binds"
  : > "$T/joins"
  while read -r var path; do
    grep -oE "\\\$$var\.[A-Za-z0-9_.]+" "$T/text" | sed "s/^\\\$$var\./$path./" >> "$T/joins" || true
  done < "$T/binds"
  sort -u "$T/refs" "$T/joins" > "$T/consumed"
  awk -F. 'NR==FNR{c[$0]=1;next}{p="";ok=0;for(i=1;i<=NF;i++){p=(p==""?$i:p"."$i);if(p in c){ok=1;break}}if(!ok)print}' \
    "$T/consumed" "$T/leaves" > "$out"
}

# ── planted RED: a dead key in a values copy MUST be flagged ──
red=$TMPDIR/chart-181-red
rm -rf "$red"
cp -r . "$red"
printf '\nzzBughunt3DeadKnob: 1\n' >> "$red/values.yaml"
extract_unconsumed "$red" "$T/unconsumed-red"
grep -qx 'zzBughunt3DeadKnob' "$T/unconsumed-red" || {
  echo "FAIL: planted dead values key was NOT flagged — the lint is fail-open" >&2
  exit 1
}

# ── real chart ──
extract_unconsumed "$root" "$T/unconsumed-raw"

# apply allowlist; every entry must be live (census)
jq -r '.[].prefix' "$T/allowlist.json" > "$T/allowprefixes"
: > "$T/unconsumed"
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
