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
# Self-test (W11-CB): the planted literal-default on a direct .Values
# ref and on a values-bound alias must both fire; the range-var,
# fallthrough, and null-guard plants must not — the check cannot pass
# without its reds proving the classifier.

python3 - <<'PY'
import pathlib
import re
import sys

ALIAS_RE = re.compile(r"\{\{-?\s*\$(\w+)\s*:=\s*\.Values[\w.]*")
DEFAULT_RE = re.compile(
    r"(\.Values\.[\w.]+|\$(\w+)\.[\w.]+)\s*\|\s*default\s+([^|}]+)"
)

BURNDOWN = {
    ("templates/store.yaml", "$s.replicas"),
    ("templates/store.yaml", "$s.streamDrainSecs"),
    ("templates/store.yaml", "$s.chunkPrefetchK"),
    ("templates/controller.yaml", ".Values.karpenter.nodeclaimPool.leaseName"),
    ("templates/rbac.yaml", ".Values.karpenter.nodeclaimPool.leaseName"),
    ("templates/rbac.yaml", ".Values.scheduler.leaseName"),
}


def scan(path, text):
    aliases = set(ALIAS_RE.findall(text))
    hits = []
    for i, line in enumerate(text.splitlines(), 1):
        for m in DEFAULT_RE.finditer(line):
            if m.group(2) and m.group(2) not in aliases:
                continue  # $x not bound to .Values (range vars etc.)
            fallback = m.group(3).strip()
            if fallback.startswith(".Values.") or re.match(r"\$\w*\.Values\.", fallback):
                continue  # documented fallthrough (values -> values)
            if fallback == '""':
                continue  # null guard, not a default value
            hits.append((path, i, m.group(1)))
    return hits


# --- self-test arms run FIRST (the house pattern) ---------------------
plant = (
    "{{- $v := .Values.x -}}\n"
    "value: {{ .Values.a.b | default 3 }}\n"
    "other: {{ $v.c | default 4 }}\n"
    "fall: {{ .Values.m.n | default .Values.k.c | default \"\" | quote }}\n"
    "guard: {{ .Values.p.q | default \"\" | quote }}\n"
    "{{- range .Values.pools }}\n"
    'p: {{ .policy | default "w" }}\n'
    "q: {{ $r.s | default 1 }}\n"
    "{{- end }}\n"
)
got = sorted(k for _, _, k in scan("planted.yaml", plant))
if got != ["$v.c", ".Values.a.b"]:
    print(
        f"FAIL: default-ban self-test — classifier got {got}, want exactly the "
        f"two planted literal-default .Values-rooted refs (fallthrough, null "
        f"guard, and range plants exempt)",
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
