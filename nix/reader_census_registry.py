#!/usr/bin/env python3
"""The R31 reader-census REGISTRY (WO-S8-14(i); see nix/misc-checks.nix).

Argv: <src-root> [--mint-union NAME].

R31 (round-12 banner): every sealed quantity's READER SET is
MACHINE-DERIVED — a generator walks the tree enumerating every consult
site of the type/field, and each reader carries a measure-compatibility
witness. The round's two highs were COUPLED-READER misses inside sealed
measures (merged_bug_005's builder trim arm; bug_155's global
realisations insert): "known" readers no hand list named. This registry
is the framework face:

  (a) ENROLLMENT IS TOTAL over censuses the registry can see: every
      in-crate [GEN-SET] reader census carries a same-line registry tag
      (`reader-census: <name>`) and every tag must have a REGISTRY row
      (unregistered census = red) AND every row's tag must resolve
      (registry rot = red). Claim strength stated honestly: "registered"
      is total over tagged censuses — the discovery grammar is the tag,
      and the slot WOs mint the tags with their censuses (the
      merged_bug_148 absence-face lesson applied to the framework
      itself: a census the registry cannot see is exactly why the tag
      is part of the census-rider contract).
  (b) the TWO CROSS-CRATE UNION ROWS (PD-1): an in-crate census walks
      ITS OWN crate's sites only (the (zzzzz) dev-green/gate-red
      lesson: cross-crate member sets are never walked from a sibling
      crate's nextest sandbox). The workspace-UNION completeness face
      of the two cross-crate censuses discharges HERE, same-wave:

        durable-ack-readers   — every consult site of
                                `durable_through_line` across the
                                workspace (rio-store producers +
                                rio-builder trim + rio-proto surface);
        dispatched-cells      — every consult site of
                                `dispatched_cells` across the
                                workspace (rio-scheduler writers +
                                rio-controller re-ack consumers).

      Each union row's generator RE-RUNS the consult-site grammar over
      the STAGED workspace and diffs against the committed [GEN-SET]
      expectation file (nix/census/<name>.union) — re-minted via
      `--mint-union <name>` at integration trees (the (wwwww) regen
      ritual, never hand-edited). A NEW consult site (a new coupled
      reader — the exact high channel) reds until the census that owns
      it enrolls it and the expectation re-mints.
  (c) the JURISDICTION planted-red, the registry-diff oracle (WS-4
      corrected form): the union generators' population derives from
      the workspace jurisdiction (the WO-S8-4 derivation,
      jurisdiction_crates); the selftest plants a strawman hand-list
      population and the REGISTRY's jurisdiction diff goes red against
      the registered derivation — never the generator's own
      completeness pin, which structurally cannot see a member outside
      its derivation ("absence of hits produces absence of evidence").
  (d) POPULATION floors: the union walks ride the WO-S8-3 floor law
      (per-crate scanned>0 via the shared derivation); the nine
      in-crate generators SELF-FLOOR per their WOs (the CE-1 honest
      scoping) and this registry consumes their committed [GEN-SET]
      population counts read-only at the wave-close tree.

Self-test arms run first (the house pattern): a registry that cannot
fail its planted fixtures does not gate.
"""

import pathlib
import re
import sys

import census_corpora
import rust_strip

# --- the union rows (PD-1) ---------------------------------------------
#
# (field, expectation file). The grammar: every PRODUCTION consult site
# of the field token — reads, writes, struct fields, wire accessors —
# cfg(test)-resolved (WO-S8-6) and comment/string-stripped, keyed
# `crate/file<TAB>line-kind`. Kind is the consult shape: `field` (a
# `.field` / `field:` access) or `ident` (any other token position —
# declarations, locals named after the field; over-approximation is the
# fail-closed direction for a reader census: a phantom row forces a
# human look, an absent row hides a coupled reader).
UNION_ROWS = {
    "durable-ack-readers": "durable_through_line",
    "dispatched-cells": "dispatched_cells",
}
UNION_DIR = "nix/census"


def union_consult_sites(src_root, field):
    """[(rel, kind, token-context)] for every production consult of
    `field` across the workspace jurisdiction (derived, never a hand
    crate-list)."""
    out = []
    field_re = re.compile(r"(?<![A-Za-z0-9_])" + re.escape(field) + r"(?![A-Za-z0-9_])")
    for crate in census_corpora.jurisdiction_crates(src_root):
        croot = src_root / crate / "src"
        test_files = rust_strip.cfg_test_reachable_files(croot)
        for f in sorted(croot.rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            if f.relative_to(croot).as_posix() in test_files:
                continue
            raw = f.read_text(encoding="utf-8")
            try:
                pruned = rust_strip.strip_cfg_test(raw, source=rel)
            except rust_strip.StripError:
                pruned = raw  # the owning scanners refuse; the census over-approximates
            lexed, _ = rust_strip.lex(pruned, blank_string_bodies=True)
            for m in field_re.finditer(lexed):
                before = lexed[max(0, m.start() - 1) : m.start()]
                after = lexed[m.end() : m.end() + 1]
                kind = "field" if before == "." or after == ":" else "ident"
                out.append((rel, kind, lexed.count("\n", 0, m.start()) + 1))
    return out


def union_keys(sites):
    """Content-keyed rows: one per (file, kind) — line numbers are
    diagnostics, never identity (the WO-S8-5 convention)."""
    return sorted({f"{rel}\t{kind}" for rel, kind, _line in sites})


def check_union_row(src_root, name, field):
    fails = []
    sites = union_consult_sites(src_root, field)
    keys = union_keys(sites)
    if not keys:
        fails.append(
            f"union row {name}: population floor — zero consult sites of "
            f"`{field}` in the workspace; the sealed quantity vanished or "
            f"the tree is mis-staged ((vvvvv))"
        )
        return fails
    exp_path = src_root / UNION_DIR / f"{name}.union"
    if not exp_path.is_file():
        fails.append(
            f"union row {name}: expectation file {UNION_DIR}/{name}.union "
            f"missing — mint it: reader_census_registry.py <root> "
            f"--mint-union {name}"
        )
        return fails
    expected = [x for x in exp_path.read_text().splitlines() if x.strip()]
    new = sorted(set(keys) - set(expected))
    gone = sorted(set(expected) - set(keys))
    for k in new:
        fails.append(
            f"union row {name}: NEW consult site {k.replace(chr(9), ' [')}] of "
            f"`{field}` — a new coupled reader (the merged_bug_005/bug_155 "
            f"channel); enroll it in the owning in-crate census, then re-mint "
            f"the union ({UNION_DIR}/{name}.union)"
        )
    for k in gone:
        fails.append(
            f"union row {name}: consult site {k.replace(chr(9), ' [')}] left the "
            f"tree — re-mint the union (shrink is healthy; a silent stale row "
            f"is rot)"
        )
    return fails


# --- the enrollment face (a): tags <-> registry rows ---------------------
#
# In-crate reader censuses carry `reader-census: <name>` same-line tags
# (the census-rider contract; H1''..H6'' relay the minted names at the
# slots' DONEs and the wave-close enrollment lists them here). The two
# union rows are registry-native.
REGISTRY_ROWS = {
    # name -> (kind, jurisdiction-derivation description)
    "durable-ack-readers": ("union", "workspace glob via jurisdiction_crates"),
    "dispatched-cells": ("union", "workspace glob via jurisdiction_crates"),
    # In-crate rows enroll at the wave-close tree from the H-packs
    # (the slots' [GEN-SET] censuses land with their WOs; OQ-11: no WO
    # waits on this framework).
}
TAG_RE = re.compile(r"reader-census:\s*([a-z][a-z0-9-]*)")


def check_enrollment(src_root, registry=None):
    registry = REGISTRY_ROWS if registry is None else registry
    fails = []
    tags = {}
    for crate in census_corpora.jurisdiction_crates(src_root):
        for f in sorted((src_root / crate / "src").rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            for m in TAG_RE.finditer(f.read_text(encoding="utf-8")):
                tags.setdefault(m.group(1), []).append(rel)
        tdir = src_root / crate / "tests"
        if tdir.is_dir():
            for f in sorted(tdir.rglob("*.rs")):
                rel = str(f.relative_to(src_root))
                for m in TAG_RE.finditer(f.read_text(encoding="utf-8")):
                    tags.setdefault(m.group(1), []).append(rel)
    for name, rels in sorted(tags.items()):
        if name not in registry:
            fails.append(
                f"{rels[0]}: reader census `{name}` is tagged but UNREGISTERED "
                f"— an unregistered census is itself a registry red (add its "
                f"REGISTRY_ROWS row naming the jurisdiction derivation)"
            )
    for name, (kind, _juris) in sorted(registry.items()):
        if kind == "in-crate" and name not in tags:
            fails.append(
                f"registry row `{name}` (in-crate) has no live "
                f"`reader-census: {name}` tag — the census rotted or was "
                f"never landed"
            )
    return fails


def main() -> int:
    args = sys.argv[1:]
    mint = None
    if "--mint-union" in args:
        i = args.index("--mint-union")
        mint = args[i + 1]
        del args[i : i + 2]
    src_root = pathlib.Path(args[0])

    lexer_err = rust_strip.selftest()
    if lexer_err:
        print(f"FAIL: shared lexer self-test — {lexer_err}", file=sys.stderr)
        return 1

    # --- self-test arms --------------------------------------------------
    # (c) the jurisdiction planted-red, REGISTRY-DIFF oracle: a strawman
    # hand-list population reds against the registered derivation — the
    # WO-S8-4 mechanism, asserted at the framework layer.
    derived = census_corpora.jurisdiction_crates(src_root)
    straw = [c for c in derived if c in ("rio-store",)]
    gaps = census_corpora.jurisdiction_gaps(straw, derived)
    if not gaps or not any("rio-builder" in g for g in gaps):
        print(
            f"FAIL: jurisdiction planted-red — the strawman hand-list did not "
            f"derive gap rows at the registry diff: {gaps}",
            file=sys.stderr,
        )
        return 1
    # (a) enrollment plants: an unregistered tag reds; a rotted in-crate
    # row reds.
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        t = pathlib.Path(td)
        (t / "rio-straw" / "src").mkdir(parents=True)
        (t / "rio-straw" / "Cargo.toml").write_text("[package]\n")
        (t / "rio-straw" / "src" / "lib.rs").write_text(
            "// [GEN-SET] reader-census: ghost-readers\nfn live() {}\n"
        )
        f_a = check_enrollment(t, registry=REGISTRY_ROWS)
        if not any("UNREGISTERED" in x for x in f_a):
            print(f"FAIL: the unregistered-tag plant did not red: {f_a}", file=sys.stderr)
            return 1
        f_b = check_enrollment(
            t, registry={"ghost-readers": ("in-crate", "x"), "dead-row": ("in-crate", "x")}
        )
        if not any("dead-row" in x and "no live" in x for x in f_b):
            print(f"FAIL: the rotted-row plant did not red: {f_b}", file=sys.stderr)
            return 1
        # (b) the union grammar locates planted consult shapes (field
        # and ident positions) and the diff oracle reds on a NEW reader.
        (t / "rio-straw" / "src" / "lib.rs").write_text(
            "struct A { durable_through_line: u64 }\n"
            "fn r(a: &A) -> u64 { a.durable_through_line }\n"
        )
        sites = union_consult_sites(t, "durable_through_line")
        kinds = {k for _r, k, _l in sites}
        if kinds != {"field"} or len(sites) != 2:
            print(f"FAIL: union grammar mis-located the planted consults: {sites}", file=sys.stderr)
            return 1
        (t / UNION_DIR).mkdir(parents=True)
        (t / UNION_DIR / "durable-ack-readers.union").write_text(
            "rio-straw/src/lib.rs\tfield\n"
        )
        if check_union_row(t, "durable-ack-readers", "durable_through_line"):
            print("FAIL: the matching union expectation still red", file=sys.stderr)
            return 1
        (t / "rio-straw" / "src" / "trim.rs").write_text(
            "fn trim(d: &D) { consume(d.durable_through_line); }\n"
        )
        f_u = check_union_row(t, "durable-ack-readers", "durable_through_line")
        if not any("NEW consult site" in x and "trim.rs" in x for x in f_u):
            print(f"FAIL: the new-coupled-reader plant did not red: {f_u}", file=sys.stderr)
            return 1

    # --- mint / real run --------------------------------------------------
    if mint is not None:
        if mint not in UNION_ROWS:
            print(f"unknown union row {mint}", file=sys.stderr)
            return 2
        keys = union_keys(union_consult_sites(src_root, UNION_ROWS[mint]))
        if not keys:
            print(f"mint refused: zero consult sites for {mint} (vacuous population)", file=sys.stderr)
            return 1
        out = src_root / UNION_DIR / f"{mint}.union"
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text("".join(k + "\n" for k in keys))
        print(f"minted {len(keys)} union rows at {UNION_DIR}/{mint}.union")
        return 0

    fails = []
    for name, field in sorted(UNION_ROWS.items()):
        fails += check_union_row(src_root, name, field)
    fails += check_enrollment(src_root)
    n_union = sum(
        1
        for name in UNION_ROWS
        if (src_root / UNION_DIR / f"{name}.union").is_file()
    )
    print(
        f"reader-census registry: {len(REGISTRY_ROWS)} rows "
        f"({n_union} union expectations staged), enrollment total over "
        f"tagged censuses, jurisdiction derived via jurisdiction_crates"
    )
    if fails:
        print("FAIL: reader-census registry violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
