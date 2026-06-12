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
values, dynamic metric names, metrics with no describe at all, and
helper fns whose body extent cannot be derived ([helper-truncated]).
A planted-sample self-test runs first.

EXTRACTION IS SPAN-DERIVED (merged_bug_019): emissions and describes
are walked via the shared lexer's macro-call extents with a top-level
argument split — EVERY `"<key>" => <expr>` binding in a call is
extracted (a two-key emission's second key is checked, not invisible
— the old single-anchor regexes captured only the first binding);
helper-fn arm collection runs over the fn's WHOLE body via fn extents
(the old fixed 2500-char window returned PARTIAL vals with skip=None,
violating this file's own censused-never-dropped contract); and the
value-in-HELP check is WORD-BOUNDARY, not substring (a value cannot
ride a longer sibling's documentation). Plants enter at the
raw-source layer through the same extraction the production scan
uses.
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

# Heads of a macro-argument walk (span-derived; see the module
# docstring): a literal metric name, or — merged_bug_189 — a FIELD
# PATH (`hooks.stale_reclaimed_metric`) resolved through
# `*_metric: "lit"` struct-literal inits collected across the scanned
# crates, so the hooks-indirected emissions are checked against EVERY
# metric name the field can carry; unresolvable shapes are CENSUSED,
# never dropped.
NAME_LIT = re.compile(r'^"([\w]+)"$')
FIELD_PATH = re.compile(r"^[A-Za-z_][\w]*(?:\.[A-Za-z_][\w]*)+$")
BINDING = re.compile(rf'^"({_KEYS_ALT})"\s*=>\s*(.+)$', re.S)
FIELD_INIT = re.compile(r'([a-z_][\w]*_metric)\s*:\s*"([\w]+)"')
CONST_STR = re.compile(r'const\s+([A-Z_][A-Z0-9_]*)\s*:\s*&\s*str\s*=\s*"([\w]+)"')
LIT = re.compile(r'^"((?:[^"\\]|\\.)*)"$')
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


def resolve_reasons(expr: str, file_text: str, const_strs=None, const_collisions=None):
    """Returns (values, skip_class). values is a list of reason strings."""
    const_strs = const_strs or {}
    expr = expr.strip()
    m = LIT.match(expr)
    if m:
        return [m.group(1)], None
    if "(" in expr and not expr.startswith("if "):
        name = expr.split("(")[0].strip()
        if re.fullmatch(r"[A-Za-z_][\w]*", name):
            # Same-file helper fn: its `=> "lit"` arms over the WHOLE
            # body, span-derived (merged_bug_019: the old fixed
            # 2500-char window returned PARTIAL vals with skip=None —
            # an arm past the window was silently undocumented).
            for fname, _start, b0, b1 in rust_strip.fn_extents(file_text):
                if fname == name:
                    body = file_text[b0:b1]
                    vals = ARM_LIT.findall(body)
                    # WO-S8-8 (merged_bug_094): a resolution returning
                    # non-empty values PROVES arm-exhaustiveness (every
                    # `=>` arm yielded a literal) or the helper is
                    # CENSUSED — the old partial-with-skip=None return
                    # silently undocumented the non-literal arms (the
                    # merged_bug_019 PARTIAL shape one axis over).
                    # Arrow counting is conservative: nested arrows
                    # over-count and route to the census, never to a
                    # silent pass.
                    arms = len(re.findall(r"=>", body))
                    if vals and len(vals) == arms:
                        return vals, None
                    if vals:
                        return [], "helper-mixed-arms"
                    return [], "helper-unresolved"
            if re.search(rf"\bfn {name}\s*\(", file_text):
                # Declared but no derivable body extent (bodyless decl
                # or malformed item): CENSUSED, never partial.
                return [], "helper-truncated"
            return [], "helper-unresolved"
        return [], "method-call"
    # Const path (`STALE_RECLAIM_HEARTBEAT`, `crate::ingest::…`):
    # resolved through the cross-file `const NAME: &str = "lit"` index
    # (merged_bug_189 — these are exactly the hooks-indirected reason
    # exprs the old scanner could not see).
    if CONST_PATH.match(expr):
        leaf = expr.rsplit("::", 1)[-1]
        if const_collisions and leaf in const_collisions:
            # WO-S8-8: the leaf-keyed global index is first-wins by
            # construction; a leaf bound to DIFFERENT values across
            # the scanned crates cannot be resolved honestly — census.
            return [], "const-collision"
        if leaf in const_strs:
            return [const_strs[leaf]], None
        return [], "const-unresolved"
    return [], "non-literal"


def documented(value: str, help_str: str) -> bool:
    """WORD-BOUNDARY value-in-HELP (merged_bug_019: substring
    containment let a value ride a longer sibling's documentation —
    `stale` passed against HELP naming only `stale_resolved`)."""
    return re.search(rf"(?<![A-Za-z0-9_]){re.escape(value)}(?![A-Za-z0-9_])", help_str) is not None


def extract_from_source(rel: str, text: str):
    """The extraction layer for ONE file's comment-blanked text:
    macro-call extents -> top-level argument split -> one row per
    `\"<key>\" => <expr>` binding (ALL bindings — a two-key emission
    yields two rows). Returns (describes, emissions, dyn_emissions).
    Self-test plants enter HERE — the outermost derivation layer the
    production scan itself uses."""
    describes: dict[str, str] = {}
    emissions, dyn_emissions = [], []
    for name, a, b in rust_strip.macro_call_extents(
        text,
        (
            # WO-S8-8 (merged_bug_094): the INSTRUMENT axis is closed —
            # histogram/gauge families join the walk (the live
            # termination_reason_label helper feeds histogram!
            # emissions the counter-only walk never saw, masking its
            # own mixed-arm hole).
            "describe_counter",
            "counter",
            "describe_histogram",
            "histogram",
            "describe_gauge",
            "gauge",
        ),
    ):
        pieces = [squash(text[pa:pb]).strip() for pa, pb in rust_strip.split_top_level(text, a, b)]
        if not pieces:
            continue
        head = pieces[0]
        if name.startswith("describe_"):
            hm = NAME_LIT.match(head)
            if hm and len(pieces) >= 2:
                describes[hm.group(1)] = help_text(" ".join(pieces[1:]))
            continue
        nm = NAME_LIT.match(head)
        dyn = None if nm else FIELD_PATH.match(head)
        if not nm and not dyn:
            continue
        for piece in pieces[1:]:
            pm = BINDING.match(piece)
            if not pm:
                continue
            key, expr = pm.group(1), pm.group(2).strip()
            if nm:
                emissions.append((rel, nm.group(1), key, expr, text))
            else:
                dyn_emissions.append((rel, head, key, expr, text))
    return describes, emissions, dyn_emissions


def floor_fails(src_root: pathlib.Path) -> list[str]:
    """Population floor (WO-S8-3, merged_bug_028): every declared
    crate must resolve and stage at least one .rs file, and the scan
    must yield a non-vacuous describe AND emission population --
    pathlib rglob fails open at zero matches, so a mis-staged tree
    previously synced zero emissions against zero describes, green.
    On a correctly staged tree the floors cannot false-positive."""
    fails = []
    for crate in CRATES:
        croot = src_root / crate / "src"
        if not croot.is_dir():
            fails.append(
                f"population floor -- declared crate root {crate}/src does "
                f"not resolve ((vvvvv))"
            )
        elif not any(croot.rglob("*.rs")):
            fails.append(f"population floor -- zero .rs files under {crate}/src")
    return fails


def scan(src_root: pathlib.Path):
    describes: dict[str, str] = {}
    emissions = []  # (file, metric, key, expr, file_text)
    dyn_emissions = []  # (file, field_path, key, expr, file_text)
    field_inits: dict[str, set] = defaultdict(set)
    const_strs: dict[str, str] = {}
    const_seen: dict[str, set] = {}
    for crate in CRATES:
        croot = src_root / crate / "src"
        if not croot.is_dir():
            continue  # floor_fails reports it
        for f in sorted(croot.rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            if "/tests/" in rel or rel.endswith("test_helpers.rs"):
                continue
            raw = strip_comments(f.read_text())
            d, e, dyn = extract_from_source(rel, raw)
            describes.update(d)
            emissions.extend(e)
            dyn_emissions.extend(dyn)
            flat = squash(raw)
            for m in FIELD_INIT.finditer(flat):
                field_inits[m.group(1)].add(m.group(2))
            for m in CONST_STR.finditer(flat):
                const_strs.setdefault(m.group(1), m.group(2))
                const_seen.setdefault(m.group(1), set()).add(m.group(2))
    const_collisions = {k for k, v in const_seen.items() if len(v) > 1}
    return describes, emissions, dyn_emissions, field_inits, const_strs, const_collisions


def check(describes, emissions, dyn_emissions=(), field_inits=None, const_strs=None, const_collisions=None):
    field_inits = field_inits or {}
    const_strs = const_strs or {}
    const_collisions = const_collisions or set()
    fails, census = [], []
    for rel, metric, key, expr, file_text in emissions:
        values, skip = resolve_reasons(expr, file_text, const_strs, const_collisions)
        if skip:
            census.append(f"{rel}: {metric} {key} `{expr}` [{skip}]")
            continue
        if metric not in describes:
            census.append(f"{rel}: {metric} has no describe_counter! [no-describe]")
            continue
        for v in values:
            if not documented(v, describes[metric]):
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
        values, skip = resolve_reasons(expr, file_text, const_strs, const_collisions)
        if skip:
            census.append(f"{rel}: dynamic metric `{field_path}` {key} `{expr}` [{skip}]")
            continue
        for name in names:
            if name not in describes:
                census.append(f"{rel}: {name} (via {field_path}) has no describe_counter! [no-describe]")
                continue
            for v in values:
                if not documented(v, describes[name]):
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
    # reason-hardcoded scanner shipped past. Runs through the LIVE
    # extraction layer (raw source in) so a regression that
    # re-hardcodes the key set reds HERE, at the layer that had the
    # gap.
    planted_src = 'describe_counter!("rio_p_total", "Probe sweeps (exit: all_masked = every rung masked).");\n' "counter!(\"rio_p_total\", \"exit\" => \"stale_resolved\").increment(1);"
    d4, e4, _dyn4 = extract_from_source("planted.rs", strip_comments(planted_src))
    if len(e4) != 1:
        print(f"FAIL: label-key self-test expected the exit emission extracted, got {e4}", file=sys.stderr)
        return 1
    f4, _ = check(d4, e4)
    if len(f4) != 1 or '"stale_resolved"' not in f4[0]:
        print(f"FAIL: label-key self-test expected the planted exit drift to fire, got {f4}", file=sys.stderr)
        return 1
    # Self-test (merged_bug_019 axis 1, the TWO-KEY plant, raw-source
    # layer): the SECOND LABEL_KEYS binding on one emission must be
    # extracted and checked — red against the old single-anchor
    # regexes, which captured only the first binding (the second key
    # neither checked nor censused).
    twokey_src = (
        'describe_counter!("rio_q_total", "Queue events (reason: parked = waiting; exit: drained = clean).");\n'
        'counter!("rio_q_total", "reason" => "parked", "exit" => "undocumented_exit").increment(1);'
    )
    d5, e5, _ = extract_from_source("planted.rs", strip_comments(twokey_src))
    if len(e5) != 2:
        print(f"FAIL: two-key self-test expected BOTH bindings extracted, got {e5}", file=sys.stderr)
        return 1
    f5, _ = check(d5, e5)
    if len(f5) != 1 or '"undocumented_exit"' not in f5[0]:
        print(f"FAIL: two-key self-test expected exactly the second-key drift, got {f5}", file=sys.stderr)
        return 1
    # --- W12-BF (WO-S8-8, merged_bug_094): the branch-derived plant
    # corpus [GEN-SET] -- the skip-class alphabet is DERIVED from
    # resolve_reasons' own source (a new resolution path cannot ship
    # without a plant: the derivation reds here first), and every
    # class is driven through the production pipeline.
    import inspect

    derived_classes = set(re.findall(r'return \[\], "([a-z-]+)"', inspect.getsource(resolve_reasons)))
    class_plants = {
        # class -> (helper file_text, expr, const_strs, const_collisions)
        "helper-mixed-arms": (
            'fn lbl(r: R) -> &str { match r { R::A => "a", R::B => other(r) } }',
            "lbl(r)", {}, set(),
        ),
        "helper-unresolved": (
            "fn lbl(r: R) -> &str { unimplemented() }",
            "lbl(r)", {}, set(),
        ),
        "helper-truncated": (
            "fn lbl(r: R) -> &str;",
            "lbl(r)", {}, set(),
        ),
        "method-call": ("", "r.as_label()", {}, set()),
        "const-unresolved": ("", "MISSING_CONST", {}, set()),
        "const-collision": ("", "DUP_REASON", {"DUP_REASON": "first"}, {"DUP_REASON"}),
        "non-literal": ("", "*reason", {}, set()),
    }
    if derived_classes != set(class_plants):
        print(
            f"FAIL: W12-BF — resolver branch census drifted from the plant corpus: "
            f"derived {sorted(derived_classes)} vs planted {sorted(class_plants)}",
            file=sys.stderr,
        )
        return 1
    for cls, (ftext, expr, cstrs, ccoll) in class_plants.items():
        vals, skip = resolve_reasons(expr, ftext, cstrs, ccoll)
        if skip != cls or vals:
            print(f"FAIL: W12-BF — plant for `{cls}` resolved ({vals}, {skip})", file=sys.stderr)
            return 1
    # The exhaustive helper still RESOLVES (the boundary's green side:
    # every arm a literal, or-patterns sharing one arrow included).
    ok_helper = 'fn lbl(r: R) -> &str { match r { R::A | R::B => "ab", R::C => "c" } }'
    vals, skip = resolve_reasons("lbl(r)", ok_helper)
    if skip is not None or sorted(vals) != ["ab", "c"]:
        print(f"FAIL: W12-BF — the exhaustive helper did not resolve: ({vals}, {skip})", file=sys.stderr)
        return 1
    # The live masked shape (hole 3 masking hole 1): a mixed-arm
    # helper feeding histogram! is EXTRACTED (instrument axis) and
    # CENSUSED (exhaustiveness) -- pre-fix it was invisible to the
    # counter-only walk and would have returned partial values.
    masked_src = (
        'fn term_lbl(r: R) -> &str { match r { R::Oom => dynamic(r), R::Other => "other" } }\n'
        'metrics::histogram!("rio_t_seconds", "reason" => term_lbl(reason)).record(1.0);\n'
        'describe_histogram!("rio_t_seconds", "Terminal latency (reason: other = fallback).");\n'
    )
    d6, e6, _ = extract_from_source("planted.rs", strip_comments(masked_src))
    if len(e6) != 1:
        print(f"FAIL: W12-BF — the histogram emission was not extracted (instrument axis): {e6}", file=sys.stderr)
        return 1
    f6, c6 = check(d6, e6)
    if f6 or len(c6) != 1 or "[helper-mixed-arms]" not in c6[0]:
        print(f"FAIL: W12-BF — the masked mixed-arm case did not census: fails={f6} census={c6}", file=sys.stderr)
        return 1
    # A gauge-family drift FAILS like a counter's (the axis is closed,
    # not merely walked): an undocumented literal on gauge!.
    gauge_src = (
        'describe_gauge!("rio_g", "Pool gauge (phase: warm = ready).");\n'
        'metrics::gauge!("rio_g", "phase" => "draining").set(1.0);\n'
    )
    d7, e7, _ = extract_from_source("planted.rs", strip_comments(gauge_src))
    f7, _ = check(d7, e7)
    if len(f7) != 1 or '"draining"' not in f7[0]:
        print(f"FAIL: W12-BF — the gauge-family drift did not fail: {f7}", file=sys.stderr)
        return 1
    # Self-test (merged_bug_019 axis 2, the WINDOW-OVERFLOW plant): a
    # helper fn whose LAST arm sits past the old 2500-char window —
    # span-derived arm collection sees it; red against the old fixed
    # window (partial vals, skip=None, silent pass).
    pad = "\n".join(f'        Variant{i:03} => "arm_{i:03}",' for i in range(80))
    overflow_src = (
        'describe_counter!("rio_r_total", "Routing (reason: arm_000 = first).");\n'
        "fn route_label(k: Kind) -> &'static str {\n    match k {\n"
        + pad
        + '\n        Tail => "overflow_tail",\n    }\n}\n'
        'counter!("rio_r_total", "reason" => route_label(kind)).increment(1);'
    )
    assert len(overflow_src) > 2600, "overflow plant must exceed the old window"
    d6, e6, _ = extract_from_source("planted.rs", strip_comments(overflow_src))
    f6, c6 = check(d6, e6)
    if not any('"overflow_tail"' in x for x in f6):
        print(f"FAIL: window-overflow self-test expected the tail arm checked (and red), got fails={f6} census={c6}", file=sys.stderr)
        return 1
    # Self-test (merged_bug_019 axis 3, WORD-BOUNDARY): a value that is
    # a strict substring of a documented sibling must still red — the
    # old containment check passed `stale` against `stale_resolved`.
    d7 = {"rio_s_total": "Sweeps (reason: stale_resolved = re-resolved)."}
    f7, _ = check(d7, [("planted.rs", "rio_s_total", "reason", '"stale"', "")])
    if len(f7) != 1:
        print(f"FAIL: word-boundary self-test expected the substring value to red, got {f7}", file=sys.stderr)
        return 1
    # ... and the helper-truncated census arm: a DECLARED but bodyless
    # helper is censused, never resolved partial.
    trunc_src = (
        'describe_counter!("rio_t_total", "T (reason: a = b).");\n'
        "fn ghost_label(k: Kind) -> &'static str;\n"
        'counter!("rio_t_total", "reason" => ghost_label(k)).increment(1);'
    )
    d8, e8, _ = extract_from_source("planted.rs", strip_comments(trunc_src))
    f8, c8 = check(d8, e8)
    if f8 or not any("[helper-truncated]" in c for c in c8):
        print(f"FAIL: helper-truncated self-test expected a census row, got fails={f8} census={c8}", file=sys.stderr)
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

    # The floor's own plant (WO-S8-3 / W12-BA): an empty root REDS.
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        ff = floor_fails(pathlib.Path(td))
        if len(ff) != len(CRATES):
            print(f"FAIL: population-floor plant did not red per crate: {ff}", file=sys.stderr)
            return 1

    describes, emissions, dyn_emissions, field_inits, const_strs, const_collisions = scan(src_root)
    fails, census = check(describes, emissions, dyn_emissions, field_inits, const_strs, const_collisions)
    fails = floor_fails(src_root) + fails
    if not describes or not emissions:
        # The vacuity face of the scan itself: a staged tree that
        # yields zero describes or zero labeled emissions means the
        # extraction (not the code) broke -- never sync nothing green.
        fails.append(
            f"population floor -- vacuous scan ({len(describes)} describes, "
            f"{len(emissions)} labeled emissions); extraction or staging rot"
        )
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
