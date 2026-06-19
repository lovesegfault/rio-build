# WO-S8-9 (merged_bug_001): the chart's single-default convention
# becomes a CHECK. A `| default <literal>` on a .Values-rooted
# reference is a DOUBLE-DEFAULT — values.yaml owns the single default
# (the chart's own prose convention, store.yaml's "No `| default`
# shadow" blocks), and a template shadow drifts independently:
# execRetentionDays shipped `| default 30` against values.yaml's 30,
# and the pair can split silently on the next edit of either home.
#
# Population: every templates/*.yaml line whose defaulted operand is
# `.Values.…` (directly or via `$`/root) or `$alias.…` where the
# alias is bound to a .Values subtree (`{{- $s := .Values.store -}}`
# form). DERIVED exemptions (boundary by rule, not allowlist):
#   - range-item fields (karpenter pool entries) are not
#     .Values-rooted per-key references — list entries have no
#     values.yaml default home, so entry-level defaults are legal;
#   - a fallback that is ITSELF a .Values reference is the documented
#     FALLTHROUGH form (machineName -> clusterName: values.yaml's ""
#     sentinel documents the chain), not a literal shadow;
#   - `| default ""` is a null guard (provides no default value).
#
# BURN-DOWN (shrink-only, the standing ledger semantics): the
# pre-existing double-default keys are carried NAMED until their
# owners retire them (each also has a values.yaml home — the same
# class, outside this close's grant; routed in the wave-log DONE
# record, never silent). A stale entry fails; a NEW site fails.
#
# KNOWN FALSE-NEGATIVES (sh-043-r4, the §Nth-strike scoped retreat):
# r2/r3 widened the operand class three times; r4 review found four
# more shape gaps. Each is best-effort-unfixable in a per-line regex
# scope-tracker — closing one opens another (r3 adding `if` to the
# end-paired set introduced the inline `{{if}}{{end}}` over-count it
# could not see). The lint stays at its r3 reach with the residual
# population NAMED here, not chased:
#   - `with required "…" $alias.X` / `with $alias.X` opens a
#     .Values-rooted scope WITH_VALUES_RE cannot recognize (the
#     `required` wrapper and a `$s := .Values.…` alias both defeat
#     `with\s+(?:\$\.|\.)\s*Values`); bare `.k` inside reads as
#     unscoped → exempt. The one live instance (scheduler.yaml
#     `.probe.deadlineSecs` under `with required … $s.sla`) is carried
#     in BURNDOWN below.
#   - single-line `{{ if … }}…{{ end }}` inside a tracked `with` body
#     net-+1s with_depth (the elif chain sees one action per line, so
#     the same-line `end` is skipped); depth then leaks past the
#     enclosing `with` and false-positives later range-item bare `.X`.
#   - a `range` nested under `with .Values` puts range-ITEM fields at
#     with_depth>0, so `{{ .itemField | default LIT }}` flags a
#     documented-legal list-entry default (lines above: list entries
#     have no values.yaml home).
#   - bare→bare fallthrough inside `with .Values` (`.name | default
#     .fallbackName`) trips: the fallback exemption only recognizes
#     `.Values.`/`$…Values.` prefixes, not the with-scoped bare form.
# None of the latter three has a live template line today; the first
# is BURNDOWN-carried. Regex scope-tracking is best-effort; the
# structural close is values.schema.json (Helm-native nullability) or
# a helm-template AST walk — carry to r28.
#
# Self-test (W11-CB): the planted literal-default on a direct .Values
# ref and on a values-bound alias must both fire; the range-var,
# fallthrough, and null-guard plants must not — the check cannot pass
# without its reds proving the classifier.

python3 - <<'PY'
import pathlib
import re
import sys

ALIAS_RE = re.compile(r"\{\{-?\s*\$(\w+)\s*:=\s*\.Values[\w.]*")
# sh-043-r2: function-form `(default <lit> X)` alternation. The pipe-form
# regex alone missed the prefix-form Sprig 0-swallow that motivated 52
# (and the adjacent defaultLeadTimeSeed). The operand population gains
# `with`-scoped bare `.X` (a `with .Values.…` body) — same exemption
# rules apply (range vars, fallthrough, null guard). r3: factored ONE
# operand class consumed by BOTH syntactic arms — the pipe arm alone
# left `{{ .key | default LIT }}` inside `with .Values` invisible.
OPERAND = r"\.Values\.[\w.]+|\$(\w+)\.[\w.]+|\.(\w[\w.]*)"
DEFAULT_RE = re.compile(
    r"(" + OPERAND + r")\s*\|\s*default\s+([^|}]+)"
    r"|\(\s*default\s+(\S+)\s+(" + OPERAND + r")\s*\)"
)
WITH_VALUES_RE = re.compile(r"\{\{-?\s*with\s+(?:\$\.|\.)\s*Values[\w.]*")
WITH_END_RE = re.compile(r"\{\{-?\s*end\b")
# r3: every end-paired keyword balances. `if`/`define`/`block` were
# uncounted, so a nested `{{ if }}` inside `{{ with .Values }}` popped
# the scope to 0 at the if's `end` — exempting the very class the r2
# widening was added to catch.
WITH_OTHER_RE = re.compile(r"\{\{-?\s*(?:if|with|range|define|block)\b")

BURNDOWN = {
    ("templates/store.yaml", "$s.replicas"),
    ("templates/store.yaml", "$s.streamDrainSecs"),
    ("templates/store.yaml", "$s.chunkPrefetchK"),
    ("templates/controller.yaml", ".Values.karpenter.nodeclaimPool.leaseName"),
    ("templates/rbac.yaml", ".Values.karpenter.nodeclaimPool.leaseName"),
    ("templates/rbac.yaml", ".Values.scheduler.leaseName"),
    # sh-043-r4: the one live instance the `with required … $s.sla`
    # false-negative (KNOWN FALSE-NEGATIVES above) hides — carried by
    # name so its retirement is a reviewable BURNDOWN shrink, not a
    # silent classifier hole. The scanner does NOT see this site
    # (with_depth stays 0), so the stale-entry sweep below would flag
    # it as unseen; BURNDOWN_INVISIBLE marks entries the classifier
    # structurally cannot reach.
    ("templates/scheduler.yaml", ".probe.deadlineSecs"),
}
BURNDOWN_INVISIBLE = {("templates/scheduler.yaml", ".probe.deadlineSecs")}


def scan(path, text):
    aliases = set(ALIAS_RE.findall(text))
    hits = []
    with_depth = 0  # >0 ⇔ inside a `with .Values.…` body (bare .X is .Values-rooted)
    for i, line in enumerate(text.splitlines(), 1):
        for m in DEFAULT_RE.finditer(line):
            if m.group(1):  # pipe-form: X | default LIT
                operand, alias, bare = m.group(1), m.group(2), m.group(3)
                fallback = m.group(4).strip()
            else:  # function-form: (default LIT X)
                operand, alias, bare = m.group(6), m.group(7), m.group(8)
                fallback = m.group(5).strip()
            if bare and with_depth <= 0:
                continue  # bare .X outside `with .Values` — not .Values-rooted
            if alias and alias not in aliases:
                continue  # $x not bound to .Values (range vars etc.)
            if fallback.startswith(".Values.") or re.match(r"\$\w*\.Values\.", fallback):
                continue  # documented fallthrough (values -> values)
            if fallback == '""':
                continue  # null guard, not a default value
            hits.append((path, i, operand))
        # Scope tracking AFTER the match (a `with` line's own body is the
        # subsequent block; an `end` closes the block it sits in).
        if WITH_VALUES_RE.search(line):
            with_depth += 1
        elif with_depth > 0 and WITH_OTHER_RE.search(line):
            with_depth += 1  # nested with/range — its `end` does not pop our scope
        elif with_depth > 0 and WITH_END_RE.search(line):
            with_depth -= 1
    return hits


# --- self-test arms run FIRST (the house pattern) ---------------------
plant = (
    "{{- $v := .Values.x -}}\n"
    "value: {{ .Values.a.b | default 3 }}\n"
    "other: {{ $v.c | default 4 }}\n"
    "fall: {{ .Values.m.n | default .Values.k.c | default \"\" | quote }}\n"
    "guard: {{ .Values.p.q | default \"\" | quote }}\n"
    "fnform: {{ int64 (default 50 .Values.f.g) }}\n"
    "fnfall: {{ int64 (default .Values.f.h .Values.f.g) }}\n"
    "fnbare: {{ float64 (default 30.0 .bare) }}\n"
    "{{- with .Values.sla }}\n"
    # r3: nested `if` — its `end` MUST NOT pop the with-scope to 0.
    "{{- if .enabled }}\n"
    "x: 1\n"
    "{{- end }}\n"
    "scoped: {{ float64 (default 30.0 .scopedKey) }}\n"
    # r3: pipe-form bare `.X` inside `with .Values` — the chart's
    # dominant idiom; both syntactic arms consume the one operand class.
    "pscoped: {{ .pipeScoped | default 30.0 }}\n"
    "{{- end }}\n"
    "{{- range .Values.pools }}\n"
    'p: {{ .policy | default "w" }}\n'
    "q: {{ $r.s | default 1 }}\n"
    "{{- end }}\n"
)
got = sorted(k for _, _, k in scan("planted.yaml", plant))
if got != ["$v.c", ".Values.a.b", ".Values.f.g", ".pipeScoped", ".scopedKey"]:
    print(
        f"FAIL: default-ban self-test — classifier got {got}, want exactly the "
        f"five planted literal-default .Values-rooted refs (pipe + function "
        f"form, both with-scoped bare past a nested if; fallthrough, null "
        f"guard, range, and unscoped-bare plants exempt)",
        file=sys.stderr,
    )
    sys.exit(1)

# --- the real scan -----------------------------------------------------
fails = []
seen_burn = set()
for f in sorted(pathlib.Path("templates").glob("*.yaml")):
    rel = f"templates/{f.name}"
    for path, line, key in scan(rel, f.read_text()):
        norm = (path, key)
        if norm in BURNDOWN:
            seen_burn.add(norm)
            continue
        fails.append(
            f"{path}:{line}: `{key} | default …` — double-default: values.yaml "
            f"owns the single default (the chart convention); drop the template "
            f"shadow and document the knob at its values.yaml home"
        )
# BURNDOWN_INVISIBLE entries are structurally unreachable by scan() —
# their stale check is a literal-grep presence test (so retirement IS
# detectable, just not via the classifier).
for path, key in sorted(BURNDOWN_INVISIBLE):
    if f"default 3600 {key}" in pathlib.Path(path).read_text():
        seen_burn.add((path, key))
for stale in sorted(BURNDOWN - seen_burn):
    fails.append(
        f"{stale[0]}: stale burn-down entry `{stale[1]}` (the site was fixed) — "
        f"remove it from BURNDOWN in 47-template-default-ban.sh"
    )
if fails:
    print("FAIL: template default-ban —", file=sys.stderr)
    for x in fails:
        print(f"  {x}", file=sys.stderr)
    sys.exit(1)
print(f"template default-ban: clean ({len(seen_burn)} burn-down keys carried, shrink-only)")
PY
