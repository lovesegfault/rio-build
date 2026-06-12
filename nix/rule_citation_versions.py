#!/usr/bin/env python3
"""Versioned-rule citation lint (bug_017's class close).

THE DEFECT CLASS: `tracey bump` re-versions a spec rule (`foo.bar` ->
`foo.bar+2`) and the bump ceremony's cross-tier sweep is an
UNVERIFIABLE manual act for every tier tracey does not scan — VM-test
scenario comments, prose cites in nix wiring, shell fragments, ops
docs. bug_017's specimens: `store.materialize.gate-share` cited
un-versioned in substitute-scale.nix and nix/tests/default.nix after
the rule bumped to +1; tracey-validate is structurally blind to both
(config.styx test_include narrows to the wiring markers). A stale
citation is licensed-looking prose pointing at superseded normative
text.

THE KILL: every full dotted rule-ID token anywhere in the tree must
cite the version the spec currently defines. Bump sweeps become
TOTAL instead of rg-scope-dependent: the bump changes the spec, the
lint fails every stale citation by path:line, the sweep is the fix.

MECHANICS:
  1. Parse docs/spec/**/*.typ for `#r("<id>")` mints: base-id ->
     defined version (absent suffix == v1; `+N` == vN). A base
     defined at multiple versions simultaneously is itself an error.
  2. Scan the tree (text files; see EXCLUDES) for dotted-token
     candidates and check every token whose base is a KNOWN rule:
     cited version must equal the defined version. Unknown bases are
     ignored (precision over recall — this lint never guesses).
  3. Mint-shaped substrings (`#r("…")`) are masked at the SPAN, not
     the line (WO-S8-4/bug_150: mints live only in docs/spec .typ,
     which is not a scanned tier, so a mint here is always a
     QUOTATION — the old whole-line skip was a one-token evasion).
     `#rref(...)` citations are checked (cheap, and it catches a
     stale rref before the docs build does).

SELF-TEST ARMS (planted at runtime; a lint that cannot fail its
fixtures does not gate — the census_enrollment.py pattern):
  (A) planted stale MARKER-FORM cite (r[id] of a +2 rule) -> fail;
  (B) planted stale VERSIONED cite (+1 of a +2 rule)       -> fail;
  (C) planted exact versioned cite                          -> pass;
  (D) planted unknown-base token                            -> pass;
  plus: bare prose mention passes (stable-name convention) and an
  exact marker-form cite passes.

Exit: 0 clean; 1 violations (each printed path:line); 2 setup error.
"""

import re
import sys
from pathlib import Path

# Tiers scanned. Generated trees and vendored/lock material excluded:
# a generated file's citations are the generator's job to refresh, and
# failing on them would point the fix at the wrong file.
# .typ is DELIBERATELY absent: the docs tier cites rules through
# #rref(), whose anchor strips the version by design (lib/rio.typ
# _rid — links are stable across bumps), and the rule definitions
# live there; treating un-versioned typst citations as stale would
# fight the tier's own convention. The blind tiers this lint exists
# for are everything tracey's scanner and the docs build never see.
SCAN_SUFFIXES = {
    ".rs",
    ".nix",
    ".py",
    ".sh",
    ".md",
    ".ts",
    ".svelte",
    ".toml",
    ".yaml",
    ".yml",
    ".qnt",
}
# Exclusion semantics are PATH-COMPONENT, not substring (merged_bug_001
# hole 2: substring matching silently excluded
# rio-builder/src/runtime/result.rs via "result", every
# fuzz/*/fuzz_targets/ file via "target", and the whole .github/ tree
# via ".git"). An entry WITHOUT "/" must equal a whole path component;
# an entry WITH "/" is a repo-relative path prefix (exact file or
# directory subtree).
EXCLUDE_PARTS = {
    ".git",
    "target",
    "result",
    "node_modules",
    "docs/gen",
    "nix/census-grandfather.txt",
    "infra/helm/rio-build/generated",
    ".sqlx",
    "corpus",
    # The formal-model tier (.qnt + the records archives) version-pins
    # its citations as CALIBRATION PROVENANCE ("derived against rule+N")
    # — a different discipline with its own machinery: the model
    # divergence-header grammar owns staleness there (the
    # supply-model re-target workstream mints it this round; the
    # tier joins the census-generator axis list once that grammar
    # lands). Enforcing live-version equality here would erase the
    # provenance the headers exist to carry.
    "docs/spec/models",
    # Instructional examples (the repo guide quotes marker syntax
    # with historical ids), not live citations.
    "CLAUDE.md",
}


def excluded(rel: str) -> bool:
    parts = rel.split("/")
    for e in EXCLUDE_PARTS:
        if "/" in e:
            if rel == e or rel.startswith(e + "/"):
                return True
        elif e in parts:
            return True
    return False


# WO-S8-4 (bug_150, R22″): the scanner's OWN CONTROL FLOW is the
# second grammar axis — every early-continue in scan_tree is
# enumerated here and carries co-location plants in self_test (the
# self-test cross-checks that every arm has executed plant cases on
# BOTH sides of its boundary where one exists), so a skip arm cannot
# carry zero reds. The wave-10 lane-closing pass audited three known
# holes on this file and missed the MINT_RE line-skip because nothing
# derived coverage from the skip arms themselves.
EARLY_CONTINUES = (
    "suffix-filter",  # f.suffix not in SCAN_SUFFIXES
    "path-exclusion",  # excluded(rel)
    "read-failure",  # UnicodeDecodeError/OSError skip
    "mint-span-mask",  # MINT_RE spans masked (was: whole-line skip)
    "tracey-scope",  # tracey_scanned tiers: markers are tracey's
    "bare-prose",  # blind tier, bare mention: stable-name convention
)


MINT_RE = re.compile(r'#r\(\s*"([a-z][a-z0-9_.+-]*)"')
# A dotted rule-shaped token: >=3 dot-separated lowercase segments,
# optional +N version suffix, hard word boundaries on both sides.
# The version alternative carries its OWN trailing guard (merged_bug_001
# hole 1): a versioned cite is closed by anything but [\w+] — sentence
# punctuation included — so `id+3.` matches VERSIONED instead of
# backtracking to an exempt bare match of `id`. The bare alternative
# keeps the hard guard (`.`/`-`/`+` continue a longer token).
TOKEN_RE = re.compile(
    r"(?<![\w.+-])([a-z][a-z0-9_-]*(?:\.[a-z0-9_-]+){2,})"
    r"(?:(\+\d+)(?![\w+])|(?![\w.+-]))"
)


def parse_id(raw: str):
    """Literal version identity: `foo` and `foo+1` are DISTINCT tokens
    in this repo's convention (a bump from un-versioned mints `+N`,
    and the campaign's bump sweeps re-stamp citations to the literal
    suffix). Returns (base, version-token-or-None)."""
    m = re.fullmatch(r"([a-z][a-z0-9_.-]*?)(?:\+(\d+))?", raw)
    if not m:
        return None, None
    return m.group(1), int(m.group(2)) if m.group(2) else None


def collect_defined(spec_root: Path):
    defined = {}
    errors = []
    for f in sorted(spec_root.rglob("*.typ")):
        for line in f.read_text(encoding="utf-8").splitlines():
            for raw in MINT_RE.findall(line):
                base, ver = parse_id(raw)
                if base is None:
                    continue
                if base in defined and defined[base] != ver:
                    errors.append(
                        f"{f}: rule {base} defined at BOTH v{defined[base]} "
                        f"and v{ver} — duplicate mint"
                    )
                defined[base] = ver
    return defined, errors


# Locations tracey's scanner DOES read (config.styx include +
# test_include, abridged to the classes that matter here): markers in
# these files are tracey's domain — its bump/stale-review flow owns
# un-versioned markers there, and this lint re-adjudicating them
# would contradict the scanner's own semantics. Everything else is a
# BLIND tier: marker-form tokens there look load-bearing but nothing
# validates them (bug_017's substitute-scale.nix specimen).
TRACEY_SCANNED_SUFFIXES = {".rs", ".ts"}
TRACEY_SCANNED_NIX = {
    "nix/base-runtime-spec.nix",
    "nix/docker.nix",
    "nix/tests/default.nix",
    "nix/kani.nix",
    "nix/quint.nix",
}


def tracey_scanned(rel: str, suffix: str) -> bool:
    if suffix in TRACEY_SCANNED_SUFFIXES:
        return True
    if rel in TRACEY_SCANNED_NIX or rel.startswith("nix/nixos-node/"):
        return True
    return False


def scan_tree(root: Path, defined: dict):
    violations = []
    for f in sorted(root.rglob("*")):
        if not f.is_file() or f.suffix not in SCAN_SUFFIXES:
            continue
        rel = f.relative_to(root).as_posix()
        if excluded(rel):
            continue
        try:
            text = f.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        for lineno, line in enumerate(text.splitlines(), 1):
            # WO-S8-4 (bug_150): mint-shaped substrings are masked at
            # the SPAN, never the line. Mints exist only in
            # docs/spec/**/*.typ (collect_defined's domain) and .typ
            # is not a scanned tier — in every tier scanned here a
            # `#r("…")` is only ever a QUOTATION, so the old
            # line-scoped skip was a fail-open: one appended
            # `#r("x.y.z")` token silenced any line (the one-token
            # evasion). Masking the matched span keeps the mint's own
            # id out of TOKEN_RE while the REST of the line stays in
            # the scan.
            for mm in MINT_RE.finditer(line):
                line = line[: mm.start()] + " " * (mm.end() - mm.start()) + line[mm.end() :]
            for m in TOKEN_RE.finditer(line):
                base = m.group(1)
                if base not in defined:
                    continue
                # The enforced classes (derivation, recorded): a
                # MARKER-FORM token (`r[... id]`) claims tracey-grade
                # precision wherever it appears, and a VERSIONED
                # token (`id+N`) claims a specific version — both
                # must match the spec exactly. A bare prose mention
                # is the repo's stable-name convention (the #rref
                # anchor strips versions by design) and is NOT
                # flagged: blindly re-stamping prose without
                # re-deriving it against the rule text would be the
                # restated-divergence anti-pattern.
                versioned = m.group(2) is not None
                marker_form = bool(
                    re.search(
                        r"r\[(?:impl |verify )?" + re.escape(m.group(0)) + r"\]",
                        line,
                    )
                )
                if tracey_scanned(rel, f.suffix):
                    # Markers (any version state) belong to tracey's
                    # own stale-review flow in its scanned tiers; the
                    # residual load-bearing form here is a VERSIONED
                    # non-marker cite, which nothing else validates.
                    if marker_form or not versioned:
                        continue
                elif not versioned and not marker_form:
                    # Blind tier, bare prose mention: the stable-name
                    # convention (see SCAN_SUFFIXES note).
                    continue
                cited = int(m.group(2)[1:]) if m.group(2) else None
                want = defined[base]
                if cited != want:
                    cited_s = base if cited is None else f"{base}+{cited}"
                    want_s = base if want is None else f"{base}+{want}"
                    violations.append(
                        f"{rel}:{lineno}: stale rule citation `{cited_s}` — "
                        f"the spec defines `{want_s}`; re-derive the cite "
                        f"against the current rule text and re-stamp"
                    )
    return violations


def self_test():
    import tempfile

    # --- the token grammar's own productions (R22′: the corpus rows
    # derive from TOKEN_RE's alternation arms — bare vs versioned —
    # crossed with the adjacency contexts each trailing guard owns;
    # adding an arm to TOKEN_RE without a row here is a review
    # surface, not a silent gap) -----------------------------------
    productions = [
        # (arm, specimen, expect: None | (base, version-or-None))
        ("bare/clean", "see dom.area.rule here", ("dom.area.rule", None)),
        ("bare/longer-token", "dom.area.rule.extra x", ("dom.area.rule.extra", None)),
        ("versioned/clean", "see dom.area.rule+2 here", ("dom.area.rule", 2)),
        # merged_bug_001 hole 1 (the adjacency axis): sentence
        # punctuation after the version token — the old single guard
        # backtracked this to an EXEMPT BARE match.
        ("versioned/punctuation", "see dom.area.rule+2.", ("dom.area.rule", 2)),
        ("versioned/comma", "per dom.area.rule+10, then", ("dom.area.rule", 10)),
        ("versioned/bracket", "r[verify dom.area.rule+2]", ("dom.area.rule", 2)),
        # Non-version trailing `+<alpha>` is not a citation; a bare
        # match here would be the same downgrade hole.
        ("versioned/alpha-tail", "dom.area.rule+2x", None),
        ("bare/two-segments", "dom.area only", None),
    ]
    for arm, specimen, want in productions:
        m = TOKEN_RE.search(specimen)
        if want is None:
            assert m is None, f"grammar arm {arm}: unexpected match {m and m.groups()}"
        else:
            base, ver = want
            assert m is not None, f"grammar arm {arm}: no match"
            got_ver = int(m.group(2)[1:]) if m.group(2) else None
            assert (m.group(1), got_ver) == (base, ver), f"grammar arm {arm}: {m.groups()}"

    with tempfile.TemporaryDirectory() as td:
        root = Path(td)
        spec = root / "docs" / "spec"
        spec.mkdir(parents=True)
        (spec / "x.typ").write_text('#r("dom.area.rule+2")[body]\n')
        defined, errs = collect_defined(spec)
        assert not errs and defined == {"dom.area.rule": 2}, "mint parse"
        cases = [
            # (file, body, must_fail, early-continue arm the case
            # co-locates with — None for the token-grammar arms)
            ("a.nix", "# r[dom.area.rule]\n", True, None),  # arm A: stale marker-form (blind tier)
            ("b.nix", "# see dom.area.rule+1\n", True, "suffix-filter"),  # arm B: stale versioned, scanned suffix
            ("c.nix", "# see dom.area.rule+2\n", False, None),  # arm C: exact versioned
            ("d.nix", "# some.unknown.token here\n", False, None),  # arm D: unknown base
            ("e.typ", "see the dom.area.rule law\n", False, "suffix-filter"),  # typ tier exempt (suffix boundary)
            ("f.nix", "# the dom.area.rule law\n", False, "bare-prose"),  # bare prose mention
            ("g.nix", "# r[verify dom.area.rule+2]\n", False, None),  # exact marker
            ("h.rs", "// r[impl dom.area.rule]\n", False, "tracey-scope"),  # tracey-scanned marker: tracey's domain
            ("i.rs", "// per dom.area.rule+1 above\n", True, "tracey-scope"),  # scanned tier, stale versioned prose
            # The adjacency plants at the SCAN layer (hole 1): a stale
            # versioned cite right before sentence punctuation must
            # fail — red against the old regex, which read it as an
            # exempt bare mention; the exact sibling must pass.
            ("j.nix", "# see dom.area.rule+1.\n", True, None),
            ("k.nix", "# see dom.area.rule+2.\n", False, None),
            # W11-BW (bug_150, the mint-span-mask arm): a STALE cite
            # co-located with a QUOTED mint on ONE line — red against
            # the old line-scoped skip (the one-token evasion: append
            # `#r("x.y.z")` to silence any line); the mask narrows the
            # shield to the matched span and the stale cite fails.
            ("l.nix", '# per dom.area.rule+1 (see the mint #r("other.rule.id")) — stale!\n', True, "mint-span-mask"),
            # … the quoted mint's OWN id is masked, never adjudicated
            # (a stale-looking id inside the quotation is not a cite).
            ("m.nix", '# the mint reads #r("dom.area.rule+1") verbatim\n', False, "mint-span-mask"),
            # An unscanned suffix never adjudicates (the suffix
            # filter's other side; the boundary pair is b.nix above).
            ("n.txt", "per dom.area.rule+1\n", False, "suffix-filter"),
        ]
        arms_planted = set()
        for name, body, must_fail, arm in cases:
            (root / name).write_text(body)
            got = scan_tree(root, defined)
            failed = any(name in v for v in got)
            assert failed == must_fail, f"self-test arm for {name}: {got}"
            (root / name).unlink()
            if arm:
                arms_planted.add(arm)
        # read-failure arm: undecodable bytes are skipped, never a
        # crash and never a phantom violation.
        (root / "q.nix").write_bytes(b"\xff\xfe per dom.area.rule+1\n")
        got = scan_tree(root, defined)
        assert not any("q.nix" in v for v in got), f"read-failure arm: {got}"
        (root / "q.nix").unlink()
        arms_planted.add("read-failure")
        # Path-scope plants (hole 2): a stale cite in a live file whose
        # path merely CONTAINS an excluded substring must be scanned —
        # red against the old substring semantics for all three
        # shipped escapes (runtime/result.rs, fuzz_targets/, .github/)
        # — while a true excluded COMPONENT/prefix stays excluded.
        scoped = [
            ("runtime/result.rs", "// per dom.area.rule+1 above\n", True),
            ("fuzz/fuzz_targets/wire.rs", "// per dom.area.rule+1\n", True),
            (".github/workflows/ci.yaml", "# per dom.area.rule+1\n", True),
            ("target/debug/gen.rs", "// per dom.area.rule+1\n", False),
            ("docs/gen/snippet.md", "per dom.area.rule+1\n", False),
        ]
        for relname, body, must_fail in scoped:
            p = root / relname
            p.parent.mkdir(parents=True, exist_ok=True)
            p.write_text(body)
            got = scan_tree(root, defined)
            failed = any(relname in v for v in got)
            assert failed == must_fail, f"path-scope arm for {relname}: {got}"
            p.unlink()
        arms_planted.add("path-exclusion")
        # The control-flow derivation pin (WO-S8-4): every enumerated
        # early-continue arm carried at least one executed plant — an
        # arm added to scan_tree without joining EARLY_CONTINUES (or
        # joining it without a plant) is a red here, never a silent
        # zero-red skip lane.
        assert arms_planted == set(EARLY_CONTINUES), (
            f"early-continue plants drifted from the arm table: "
            f"planted {sorted(arms_planted)} vs table {sorted(EARLY_CONTINUES)}"
        )
        # Error-lane plants (hole 3): a duplicate mint is DETECTED, and
        # both its one-colon error format and a path:line violation go
        # through the grandfather keyer without crashing (the old
        # 3-way split raised ValueError, burying the diagnostic).
        (spec / "y.typ").write_text('#r("dom.area.rule+3")[body]\n')
        _, errs = collect_defined(spec)
        assert errs and "duplicate mint" in errs[0], f"duplicate mint not detected: {errs}"
        assert grandfather_key(errs[0]) == errs[0].strip(), "error-format key not tolerant"
        assert "\t" in grandfather_key("a/b.nix:12: stale rule citation `x` — y"), "violation key shape"
        (spec / "y.typ").unlink()


GRANDFATHER = "nix/rule-citation-grandfather.txt"


def grandfather_key(violation: str) -> str:
    # path:line: message → content-keyed `path<TAB>message-tail` so
    # line drift evicts nothing while editing the cited line evicts
    # the entry (the census_enrollment.py burn-down semantics).
    # TOLERANT split (merged_bug_001 hole 3): a violation without the
    # path:line:message shape (the duplicate-mint error format has one
    # colon) keys as its own full text instead of crashing the keyer —
    # check mode and --mint-grandfather both stay diagnostic-bearing.
    parts = violation.split(":", 2)
    if len(parts) < 3:
        return violation.strip()
    path, _lineno, msg = parts
    return f"{path}\t{msg.strip()}"


def main():
    args = sys.argv[1:]
    mint = "--mint-grandfather" in args
    args = [a for a in args if a != "--mint-grandfather"]
    if len(args) != 1:
        print(
            "usage: rule_citation_versions.py [--mint-grandfather] <repo-root>",
            file=sys.stderr,
        )
        return 2
    self_test()
    root = Path(args[0])
    spec_root = root / "docs" / "spec"
    if not spec_root.is_dir():
        print(f"setup: {spec_root} missing", file=sys.stderr)
        return 2
    defined, errors = collect_defined(spec_root)
    if not defined:
        print("setup: zero rules parsed from docs/spec — extractor rotted", file=sys.stderr)
        return 2
    # Duplicate-mint errors are SPEC defects, not citations: they are
    # surfaced as diagnostics and fail both modes directly — never
    # grandfathered, never run through the citation keyer
    # (merged_bug_001 hole 3: the one-colon error format crashed the
    # 3-way split, burying the diagnostic under a traceback).
    if errors:
        for e in errors:
            print(f"DUPLICATE MINT: {e}", file=sys.stderr)
        print(
            f"\n{len(errors)} duplicate rule mint(s) in docs/spec — resolve the "
            "spec before citations can be adjudicated.",
            file=sys.stderr,
        )
        return 1
    violations = scan_tree(root, defined)

    gf_path = root / GRANDFATHER
    if mint:
        # R15: the grandfather is THIS scanner's own output, never
        # hand-authored. Minted once at the lint's landing tree (the
        # pre-existing-debt burn-down ledger) and re-minted shrink-only
        # at integration trees.
        keys = sorted({grandfather_key(v) for v in violations})
        gf_path.write_text("".join(k + "\n" for k in keys))
        print(f"minted {len(keys)} grandfather entr(ies) at {GRANDFATHER}")
        return 0

    grandfathered = set()
    if gf_path.is_file():
        grandfathered = {
            line for line in gf_path.read_text().splitlines() if line.strip()
        }
    live_keys = {grandfather_key(v) for v in violations}
    out = [v for v in violations if grandfather_key(v) not in grandfathered]
    # Burn-down face: every grandfather entry must still match a live
    # violation — a fixed/edited cite must leave the ledger.
    for entry in sorted(grandfathered - live_keys):
        out.append(
            f"{entry.split(chr(9))[0]}: stale grandfather entry (the cite was "
            f"fixed or edited) — remove it from {GRANDFATHER}: {entry!r}"
        )
    if out:
        for v in out:
            print(v, file=sys.stderr)
        print(
            f"\n{len(out)} stale rule citation(s)/ledger entr(ies). A citation "
            "must name the literal version token the spec defines.",
            file=sys.stderr,
        )
        return 1
    print(
        f"rule-citation-versions: {len(defined)} rules, tree clean "
        f"({len(grandfathered)} grandfathered, burn-down)"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
