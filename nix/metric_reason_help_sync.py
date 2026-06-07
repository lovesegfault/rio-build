#!/usr/bin/env python3
"""metric-reason-help-sync scanner (see nix/misc-checks.nix).

Argv: <src-root>. For every `"reason" => <expr>` label on a
literal-named `metrics::counter!`, resolve the reason values —
string literals directly, or the `=> "lit"` arms of a same-file
helper fn when the expr is a call — and require each value to appear
in that metric's `describe_counter!` HELP text. An operator triaging
a labeled counter reads the HELP; a reason the HELP never mentions is
an undocumented failure mode (the bug_110 drift class).

Out-of-scope shapes are CENSUSED, never silently dropped: method-call
reasons (`.as_label()`, `.as_str()`), variable reasons, inline-if
reasons, dynamic metric names, and metrics with no describe at all.
A planted-sample self-test runs first.
"""

import pathlib
import re
import sys
from collections import defaultdict

CRATES = ["rio-gateway", "rio-store", "rio-scheduler", "rio-controller", "rio-builder"]

DESCRIBE = re.compile(r'describe_counter!\(\s*"([\w]+)"\s*,\s*((?:"(?:[^"\\]|\\.)*"\s*)+)\)')
EMIT = re.compile(r'counter!\(\s*"([\w]+)"\s*,[^;]*?"reason"\s*=>\s*([^,)]+)')
# merged_bug_189: a counter! whose first argument is a FIELD PATH
# (`hooks.stale_reclaimed_metric`) — resolved through `*_metric: "lit"`
# struct-literal inits collected across the scanned crates, so the
# hooks-indirected emissions are checked against EVERY metric name the
# field can carry; unresolvable shapes are CENSUSED, never dropped.
EMIT_DYN = re.compile(r'(?<!describe_)counter!\(\s*([A-Za-z_][\w]*(?:\.[A-Za-z_][\w]*)+)\s*,[^;]*?"reason"\s*=>\s*([^,)]+)')
FIELD_INIT = re.compile(r'([a-z_][\w]*_metric)\s*:\s*"([\w]+)"')
CONST_STR = re.compile(r'const\s+([A-Z_][A-Z0-9_]*)\s*:\s*&\s*str\s*=\s*"([\w]+)"')
LIT = re.compile(r'^"((?:[^"\\]|\\.)*)"$')
CALL = re.compile(r"^([A-Za-z_][\w]*)\s*\($")
ARM_LIT = re.compile(r'=>\s*"((?:[^"\\]|\\.)*)"')
CONST_PATH = re.compile(r"^[A-Za-z_][\w]*(?:::[A-Za-z_][\w]*)*$")


def strip_comments(text: str) -> str:
    return "\n".join(line.split("//")[0] if "//" in line and '"//' not in line else line for line in text.splitlines())


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
    emissions = []  # (file, metric, expr, file_text)
    dyn_emissions = []  # (file, field_path, expr, file_text)
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
                emissions.append((rel, m.group(1), m.group(2).strip(), raw))
            for m in EMIT_DYN.finditer(flat):
                dyn_emissions.append((rel, m.group(1), m.group(2).strip(), raw))
            for m in FIELD_INIT.finditer(flat):
                field_inits[m.group(1)].add(m.group(2))
            for m in CONST_STR.finditer(flat):
                const_strs.setdefault(m.group(1), m.group(2))
    return describes, emissions, dyn_emissions, field_inits, const_strs


def check(describes, emissions, dyn_emissions=(), field_inits=None, const_strs=None):
    field_inits = field_inits or {}
    const_strs = const_strs or {}
    fails, census = [], []
    for rel, metric, expr, file_text in emissions:
        values, skip = resolve_reasons(expr, file_text, const_strs)
        if skip:
            census.append(f"{rel}: {metric} reason `{expr}` [{skip}]")
            continue
        if metric not in describes:
            census.append(f"{rel}: {metric} has no describe_counter! [no-describe]")
            continue
        for v in values:
            if v not in describes[metric]:
                fails.append(f"{rel}: {metric} reason \"{v}\" absent from its describe HELP")
    # merged_bug_189: hooks-indirected emissions — the field can carry
    # ANY of its initializer names, so the reason must be documented in
    # EVERY candidate metric's HELP.
    for rel, field_path, expr, file_text in dyn_emissions:
        field = field_path.rsplit(".", 1)[-1]
        names = sorted(field_inits.get(field, ()))
        if not names:
            census.append(f"{rel}: dynamic metric `{field_path}` reason `{expr}` [dynamic-unresolved]")
            continue
        values, skip = resolve_reasons(expr, file_text, const_strs)
        if skip:
            census.append(f"{rel}: dynamic metric `{field_path}` reason `{expr}` [{skip}]")
            continue
        for name in names:
            if name not in describes:
                census.append(f"{rel}: {name} (via {field_path}) has no describe_counter! [no-describe]")
                continue
            for v in values:
                if v not in describes[name]:
                    fails.append(f'{rel}: {name} (via {field_path}) reason "{v}" absent from its describe HELP')
    return fails, census


def main() -> int:
    src_root = pathlib.Path(sys.argv[1])

    # Self-test: a planted mismatch MUST fire; a matching pair must not.
    d = {"rio_x_total": "Things (labeled by reason: good = fine)."}
    e = [("planted.rs", "rio_x_total", '"bad"', ""), ("planted.rs", "rio_x_total", '"good"', "")]
    f, _ = check(d, e)
    if len(f) != 1:
        print(f"FAIL: self-test expected exactly 1 planted failure, got {len(f)}", file=sys.stderr)
        return 1
    # Self-test (merged_bug_189 arms): a hooks-indirected emission must
    # check EVERY candidate name (red on the one whose HELP lacks the
    # reason), a const-path reason must resolve, and an unresolvable
    # dynamic shape must be censused — never silently dropped.
    d2 = {
        "rio_x_total": "Things (labeled by reason: good = fine).",
        "rio_y_total": "Things too (reason: nothing here).",
    }
    dyn = [("planted.rs", "hooks.x_metric", "PLANTED_REASON", "")]
    f2, c2 = check(d2, [], dyn, {"x_metric": {"rio_x_total", "rio_y_total"}}, {"PLANTED_REASON": "good"})
    if len(f2) != 1 or "rio_y_total" not in f2[0]:
        print(f"FAIL: dyn self-test expected exactly 1 failure naming rio_y_total, got {f2}", file=sys.stderr)
        return 1
    f3, c3 = check(d2, [], [("planted.rs", "hooks.zz_metric", '"bad"', "")])
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
