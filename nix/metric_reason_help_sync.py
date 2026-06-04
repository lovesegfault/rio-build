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

CRATES = ["rio-gateway", "rio-store", "rio-scheduler", "rio-controller", "rio-builder"]

DESCRIBE = re.compile(r'describe_counter!\(\s*"([\w]+)"\s*,\s*((?:"(?:[^"\\]|\\.)*"\s*)+)\)')
EMIT = re.compile(r'counter!\(\s*"([\w]+)"\s*,[^;]*?"reason"\s*=>\s*([^,)]+)')
LIT = re.compile(r'^"((?:[^"\\]|\\.)*)"$')
CALL = re.compile(r"^([A-Za-z_][\w]*)\s*\($")
ARM_LIT = re.compile(r'=>\s*"((?:[^"\\]|\\.)*)"')


def strip_comments(text: str) -> str:
    return "\n".join(line.split("//")[0] if "//" in line and '"//' not in line else line for line in text.splitlines())


def squash(text: str) -> str:
    return re.sub(r"\s+", " ", text)


def help_text(raw: str) -> str:
    # Concatenated adjacent string literals, unescaped enough for
    # substring checks.
    parts = re.findall(r'"((?:[^"\\]|\\.)*)"', raw)
    return "".join(p.replace("\\\n", "").replace("\\'", "'") for p in parts)


def resolve_reasons(expr: str, file_text: str):
    """Returns (values, skip_class). values is a list of reason strings."""
    expr = expr.strip()
    m = LIT.match(expr)
    if m:
        return [m.group(1)], None
    m = CALL.match(squash(expr).replace(" ", "") if "(" in expr else expr)
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
    return [], "non-literal"


def scan(src_root: pathlib.Path):
    describes: dict[str, str] = {}
    emissions = []  # (file, metric, expr, file_text)
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
    return describes, emissions


def check(describes, emissions):
    fails, census = [], []
    for rel, metric, expr, file_text in emissions:
        values, skip = resolve_reasons(expr, file_text)
        if skip:
            census.append(f"{rel}: {metric} reason `{expr}` [{skip}]")
            continue
        if metric not in describes:
            census.append(f"{rel}: {metric} has no describe_counter! [no-describe]")
            continue
        for v in values:
            if v not in describes[metric]:
                fails.append(f"{rel}: {metric} reason \"{v}\" absent from its describe HELP")
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

    describes, emissions = scan(src_root)
    fails, census = check(describes, emissions)
    print(f"metric-reason-help-sync: {len(emissions)} reason-labeled emissions, " f"{len(describes)} describes, {len(census)} censused (out of scope)")
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
