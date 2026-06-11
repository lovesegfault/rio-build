#!/usr/bin/env python3
"""metric-reason-help-sync scanner (see nix/misc-checks.nix).

Argv: <src-root>. For every `"<key>" => <expr>` label on a
literal-named `metrics::counter!` whose key is in LABEL_KEYS (the
enumerated-alphabet label-key census), resolve the label values —
string literals directly, or the `=> "lit"` arms of a same-file
helper fn when the expr is a call — and require each value to appear
in that metric's `describe_counter!` HELP text. An operator triaging
a labeled counter reads the HELP; a value the HELP never mentions is
an undocumented failure mode (the bug_110 drift class).

merged_bug_109 (the label-key lint gap): the original scanner
hardcoded `"reason"`, so every sibling enumerated key — `exit`,
`outcome`, `wake`, … — drifted unchecked one key over. The key set is
now the census table below, and the self-test plants the exact
shipped evasion (an exit-labeled value absent from HELP).

Out-of-scope shapes are CENSUSED, never silently dropped: method-call
values (`.as_label()`, `.as_str()`), variable values, inline-if
values, dynamic metric names, and metrics with no describe at all.
A planted-sample self-test runs first.
"""

import pathlib
import re
import sys
from collections import defaultdict

import rust_strip

CRATES = ["rio-gateway", "rio-store", "rio-scheduler", "rio-controller", "rio-builder"]

# The enumerated-alphabet label keys (the label-key census). Inclusion
# criterion: the key's value set is a CLOSED ALPHABET the emitting
# code chooses from (every value is a designed failure/outcome mode an
# operator must be able to look up in HELP). Data-driven keys whose
# values come from config or workload (pool, cell, tenant, rpc, cmd,
# hw_class, drv, system, …) are deliberately ABSENT: their values are
# unbounded and HELP documents the AXIS, not each value. Growing the
# alphabet family means growing THIS table — the meta-lint corpus
# plants one drift per key so a hardcoded-key regression reds here.
LABEL_KEYS = (
    "reason",
    "exit",
    "outcome",
    "disposition",
    "cause",
    "wake",
    "class",
    "domain",
    "surface",
    "phase",
    "kind",
)
_KEYS_ALT = "|".join(LABEL_KEYS)

DESCRIBE = re.compile(r'describe_counter!\(\s*"([\w]+)"\s*,\s*((?:"(?:[^"\\]|\\.)*"\s*)+)\)')
EMIT = re.compile(rf'counter!\(\s*"([\w]+)"\s*,[^;]*?"({_KEYS_ALT})"\s*=>\s*([^,)]+)')
# merged_bug_189: a counter! whose first argument is a FIELD PATH
# (`hooks.stale_reclaimed_metric`) — resolved through `*_metric: "lit"`
# struct-literal inits collected across the scanned crates, so the
# hooks-indirected emissions are checked against EVERY metric name the
# field can carry; unresolvable shapes are CENSUSED, never dropped.
EMIT_DYN = re.compile(rf'(?<!describe_)counter!\(\s*([A-Za-z_][\w]*(?:\.[A-Za-z_][\w]*)+)\s*,[^;]*?"({_KEYS_ALT})"\s*=>\s*([^,)]+)')
FIELD_INIT = re.compile(r'([a-z_][\w]*_metric)\s*:\s*"([\w]+)"')
CONST_STR = re.compile(r'const\s+([A-Z_][A-Z0-9_]*)\s*:\s*&\s*str\s*=\s*"([\w]+)"')
LIT = re.compile(r'^"((?:[^"\\]|\\.)*)"$')
CALL = re.compile(r"^([A-Za-z_][\w]*)\s*\($")
ARM_LIT = re.compile(r'=>\s*"((?:[^"\\]|\\.)*)"')
CONST_PATH = re.compile(r"^[A-Za-z_][\w]*(?:::[A-Za-z_][\w]*)*$")


def strip_comments(text: str) -> str:
    # Shared exact lexer (merged_bug_009): comments blanked, string
    # bodies KEPT — metric names, label keys, and values are read from
    # the literals. Newline-preserving, so nothing shifts.
    out, _ = rust_strip.lex(text, blank_string_bodies=False)
    return out


def squash(text: str) -> str:
    return re.sub(r"\s+", " ", text)


def help_text(raw: str) -> str:
    # Concatenated adjacent string literals, unescaped enough for
    # substring checks.
    parts = re.findall(r'"((?:[^"\\]|\\.)*)"', raw)
    return "".join(p.replace("\\\n", "").replace("\\'", "'") for p in parts)


def resolve_reasons(expr: str, file_text: str, const_strs=None):
    """Returns (values, skip_class). values is a list of reason strings."""
    const_strs = const_strs or {}
    expr = expr.strip()
    m = LIT.match(expr)
    if m:
        return [m.group(1)], None
    if "(" in expr and not expr.startswith("if "):
        name = expr.split("(")[0].strip()
        if re.fullmatch(r"[A-Za-z_][\w]*", name):
            # Same-file helper fn: collect its `=> "lit"` arms from a
            # bounded window after the definition.
            decl = re.search(rf"fn {name}\s*\(", file_text)
            if decl:
                window = file_text[decl.start() : decl.start() + 2500]
                vals = ARM_LIT.findall(window)
                if vals:
                    return vals, None
            return [], "helper-unresolved"
        return [], "method-call"
    # Const path (`STALE_RECLAIM_HEARTBEAT`, `crate::ingest::…`):
    # resolved through the cross-file `const NAME: &str = "lit"` index
    # (merged_bug_189 — these are exactly the hooks-indirected reason
    # exprs the old scanner could not see).
    if CONST_PATH.match(expr):
        leaf = expr.rsplit("::", 1)[-1]
        if leaf in const_strs:
            return [const_strs[leaf]], None
        return [], "const-unresolved"
    return [], "non-literal"


def scan(src_root: pathlib.Path):
    describes: dict[str, str] = {}
    emissions = []  # (file, metric, key, expr, file_text)
    dyn_emissions = []  # (file, field_path, key, expr, file_text)
    field_inits: dict[str, set] = defaultdict(set)
    const_strs: dict[str, str] = {}
    for crate in CRATES:
        for f in sorted((src_root / crate / "src").rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            if "/tests/" in rel or rel.endswith("test_helpers.rs"):
                continue
            raw = strip_comments(f.read_text())
            flat = squash(raw)
            for m in DESCRIBE.finditer(flat):
                describes[m.group(1)] = help_text(m.group(2))
            for m in EMIT.finditer(flat):
                emissions.append((rel, m.group(1), m.group(2), m.group(3).strip(), raw))
            for m in EMIT_DYN.finditer(flat):
                dyn_emissions.append((rel, m.group(1), m.group(2), m.group(3).strip(), raw))
            for m in FIELD_INIT.finditer(flat):
                field_inits[m.group(1)].add(m.group(2))
            for m in CONST_STR.finditer(flat):
                const_strs.setdefault(m.group(1), m.group(2))
    return describes, emissions, dyn_emissions, field_inits, const_strs


def check(describes, emissions, dyn_emissions=(), field_inits=None, const_strs=None):
    field_inits = field_inits or {}
    const_strs = const_strs or {}
    fails, census = [], []
    for rel, metric, key, expr, file_text in emissions:
        values, skip = resolve_reasons(expr, file_text, const_strs)
        if skip:
            census.append(f"{rel}: {metric} {key} `{expr}` [{skip}]")
            continue
        if metric not in describes:
            census.append(f"{rel}: {metric} has no describe_counter! [no-describe]")
            continue
        for v in values:
            if v not in describes[metric]:
                fails.append(f'{rel}: {metric} {key} "{v}" absent from its describe HELP')
    # merged_bug_189: hooks-indirected emissions — the field can carry
    # ANY of its initializer names, so the label value must be
    # documented in EVERY candidate metric's HELP.
    for rel, field_path, key, expr, file_text in dyn_emissions:
        field = field_path.rsplit(".", 1)[-1]
        names = sorted(field_inits.get(field, ()))
        if not names:
            census.append(f"{rel}: dynamic metric `{field_path}` {key} `{expr}` [dynamic-unresolved]")
            continue
        values, skip = resolve_reasons(expr, file_text, const_strs)
        if skip:
            census.append(f"{rel}: dynamic metric `{field_path}` {key} `{expr}` [{skip}]")
            continue
        for name in names:
            if name not in describes:
                census.append(f"{rel}: {name} (via {field_path}) has no describe_counter! [no-describe]")
                continue
            for v in values:
                if v not in describes[name]:
                    fails.append(f'{rel}: {name} (via {field_path}) {key} "{v}" absent from its describe HELP')
    return fails, census


def main() -> int:
    src_root = pathlib.Path(sys.argv[1])

    # A broken shared lexer fails closed before any scan may gate.
    lexer_err = rust_strip.selftest()
    if lexer_err:
        print(f"FAIL: shared lexer self-test — {lexer_err}", file=sys.stderr)
        return 1

    # Self-test: a planted mismatch MUST fire; a matching pair must not.
    d = {"rio_x_total": "Things (labeled by reason: good = fine)."}
    e = [("planted.rs", "rio_x_total", "reason", '"bad"', ""), ("planted.rs", "rio_x_total", "reason", '"good"', "")]
    f, _ = check(d, e)
    if len(f) != 1:
        print(f"FAIL: self-test expected exactly 1 planted failure, got {len(f)}", file=sys.stderr)
        return 1
    # Self-test (merged_bug_109, the label-key plant): an EXIT-labeled
    # value absent from HELP must fire — the exact drift the
    # reason-hardcoded scanner shipped past. Run through the live
    # regexes (not check() directly) so a regression that re-hardcodes
    # the key set reds HERE, at the extraction layer that had the gap.
    planted_src = 'describe_counter!("rio_p_total", "Probe sweeps (exit: all_masked = every rung masked).");\n' "counter!(\"rio_p_total\", \"exit\" => \"stale_resolved\").increment(1);"
    flat = squash(strip_comments(planted_src))
    d4 = {m.group(1): help_text(m.group(2)) for m in DESCRIBE.finditer(flat)}
    e4 = [("planted.rs", m.group(1), m.group(2), m.group(3).strip(), planted_src) for m in EMIT.finditer(flat)]
    if len(e4) != 1:
        print(f"FAIL: label-key self-test expected the exit emission extracted, got {e4}", file=sys.stderr)
        return 1
    f4, _ = check(d4, e4)
    if len(f4) != 1 or '"stale_resolved"' not in f4[0]:
        print(f"FAIL: label-key self-test expected the planted exit drift to fire, got {f4}", file=sys.stderr)
        return 1
    # Self-test (merged_bug_189 arms): a hooks-indirected emission must
    # check EVERY candidate name (red on the one whose HELP lacks the
    # reason), a const-path reason must resolve, and an unresolvable
    # dynamic shape must be censused — never silently dropped.
    d2 = {
        "rio_x_total": "Things (labeled by reason: good = fine).",
        "rio_y_total": "Things too (reason: nothing here).",
    }
    dyn = [("planted.rs", "hooks.x_metric", "reason", "PLANTED_REASON", "")]
    f2, c2 = check(d2, [], dyn, {"x_metric": {"rio_x_total", "rio_y_total"}}, {"PLANTED_REASON": "good"})
    if len(f2) != 1 or "rio_y_total" not in f2[0]:
        print(f"FAIL: dyn self-test expected exactly 1 failure naming rio_y_total, got {f2}", file=sys.stderr)
        return 1
    f3, c3 = check(d2, [], [("planted.rs", "hooks.zz_metric", "reason", '"bad"', "")])
    if f3 or not any("dynamic-unresolved" in c for c in c3):
        print(f"FAIL: dyn self-test expected a dynamic-unresolved census entry, got fails={f3} census={c3}", file=sys.stderr)
        return 1

    describes, emissions, dyn_emissions, field_inits, const_strs = scan(src_root)
    fails, census = check(describes, emissions, dyn_emissions, field_inits, const_strs)
    print(
        f"metric-reason-help-sync: {len(emissions)} literal + {len(dyn_emissions)} dynamic "
        f"reason-labeled emissions, {len(describes)} describes, "
        f"{len(field_inits)} metric-name fields, {len(census)} censused (out of scope)"
    )
    for c in census:
        print(f"  census: {c}")
    if fails:
        print(
            "FAIL: reason label(s) undocumented in describe HELP —\n"
            "an operator triaging the counter reads the HELP; add the\n"
            "reason and what it means:",
            file=sys.stderr,
        )
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
