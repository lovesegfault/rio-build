#!/usr/bin/env python3
"""The R33 duplicate-derivation lint + the rationale-rot sweep
(WO-S8-14(iii)).

Argv: <src-root> [--mint-rot-grandfather].

R33 (round-12 banner): every derived quantity has exactly one
producing fn; consumers import, never re-derive; a second formula for
a named quantity is a lint violation — the list-mirrors-list rule
extended to formulas. Dual derivations diverge exactly under the
lag/pressure regime where consumers act destructively (bug_103's two
"held" inventories; merged_bug_052's re-encoded mint law;
merged_bug_136's five horizon copies; bug_014's two avg_cores
domains).

REGISTRY (the PD-7 binding seed: the H1''/H5''/H6'' relayed producer
lists union — this table is the checksum; rows flip pending->anchored
as the slots land, the wave-close --verify-landed flips or fails):

  contiguous_durable_frontier (S1) · TICK_BODY_BOUND (S1) ·
  avg_cores (S5) · sample_weight (S5) · the fence datum (S5) ·
  demand-holding truth source (S6 — the row the prior enumeration
  dropped; its omission was the dual-inventory class unlinted) ·
  mintable(head, class) (S6) · wait_envelope(intent) (S6) ·
  disk-sizing-input (A2 live060-e — LIVE from birth: a sizing-path
  consult of `disk_used_bytes` reds; the sizing input is
  peak_disk_bytes, the node gauge is observability surface).

The duplicate-formula grammar lands per-producer at the wave-close
flip (the operand/operation signature derives from the landed
producer's BODY — deriving it from a body that does not exist yet
would be author-assertion, the exact mode the banner kills). The
disk-sizing row's grammar is live now (the field name IS the
signature).

THE RATIONALE-ROT SWEEP (the OQ-14 latitude, recorded): hard-red on
the NAMED CLAIM GRAMMARS only — a comment asserting a dataflow
relation `<verb> by <symbol>()` whose symbol resolves NOWHERE in the
workspace is fix-adjacent rot (the bug_026/bug_097/merged_bug_100
mode: the close renamed the mechanism, the sibling comment kept the
old name). Pre-existing rot is grandfathered SHRINK-ONLY at mint; the
wave's own closes are the first corpus (their commit-body R4 sweep
lines assert clean). If the grammar cannot hold <5% FP on the wave
corpus the check demotes to an RC-2 review row carrying its corpus
(RULED to round-13) — the recorded fallback, attack at review.
"""

import pathlib
import re
import sys

import census_corpora
import rust_strip

# Landed rows: (state, slot, file, producer-anchor). Flipped at the
# wave-close --verify-landed (bw12, dfd3afb2b+19); every anchor
# grep-verified at the composed tree. The frontier producer landed in
# rio-log-kernel (the CF-2 delegation: CoverageMap::contiguous_prefix_end
# is the one formula; store + builder consume). The duplicate-formula
# grammar per producer is the standing extension surface: a second
# textual derivation of a registered producer's formula reds at the
# next census growth (the rows make the producers NAMED; the
# disk-sizing live arm below is the enforcement exemplar).
R33_ROWS = {
    "contiguous-durable-frontier": ("landed", "S1", "rio-log-kernel/src/lib.rs", r"contiguous_prefix_end|contiguous_durable_frontier"),
    "tick-body-bound": ("landed", "S1", "rio-store/src/logs/sessions.rs", r"TICK_BODY_BOUND"),
    "avg-cores": ("landed", "S5", "rio-scheduler/src/sla/ingest.rs", r"fn avg_cores"),
    "sample-weight": ("landed", "S5", "rio-scheduler/src/sla/ingest.rs", r"sample_weight"),
    "fence-datum": ("landed", "S5", "rio-scheduler/src/sla/cost.rs", r"fence"),
    "demand-holding": ("landed", "S6", "rio-controller/src/reconcilers/pool/jobs.rs", r"demand_lane|held_job_demand|HeldThreaded"),
    "mintable": ("landed", "S6", "rio-controller/src/reconcilers/nodeclaim_pool/cover.rs", r"mintable"),
    "wait-envelope": ("landed", "S6", "rio-controller/src/reconcilers/pool/pod.rs", r"wait_envelope"),
}
# The LIVE row (A2 live060-e): the sizing input is peak_disk_bytes;
# `disk_used_bytes` (node statvfs — the observability gauge) consulted
# in the scheduler's sizing path is the wrong-field confusion; the
# population is check_sizing_field's own walk.
SIZING_PATHS = ("rio-scheduler/src/sla/",)
SIZING_FIELD = re.compile(r"(?<![A-Za-z0-9_])disk_used_bytes(?![A-Za-z0-9_])")

ROT_RE = re.compile(
    r"(?:bumped|cleared|reset|minted|produced|debited|enqueued) by\s+"
    r"(?:`)?([A-Za-z_][A-Za-z0-9_]*)(?:`)?\s*\(\)"
)
ROT_GRANDFATHER = "nix/rationale-rot-grandfather.txt"


def check_sizing_field(src_root):
    fails = []
    for crate in census_corpora.jurisdiction_crates(src_root):
        croot = src_root / crate / "src"
        for f in sorted(croot.rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            if not any(rel.startswith(p) for p in SIZING_PATHS):
                continue
            raw = f.read_text(encoding="utf-8")
            try:
                pruned = rust_strip.strip_cfg_test(raw, source=rel)
            except rust_strip.StripError:
                pruned = raw
            lexed, _ = rust_strip.lex(pruned, blank_string_bodies=True)
            for m in SIZING_FIELD.finditer(lexed):
                lineno = lexed.count("\n", 0, m.start()) + 1
                fails.append(
                    f"{rel}:{lineno}: `disk_used_bytes` consulted in the sizing "
                    f"path — the sizing input is peak_disk_bytes (one field per "
                    f"quantity, live060-e); the node gauge is observability "
                    f"surface, never a fit/derive input"
                )
    return fails


def workspace_fn_names(src_root):
    names = set()
    for crate in census_corpora.jurisdiction_crates(src_root):
        for f in sorted((src_root / crate / "src").rglob("*.rs")):
            lexed, _ = rust_strip.lex(f.read_text(encoding="utf-8"), blank_string_bodies=True)
            names |= {m.group(1) for m in rust_strip.FN_DECL.finditer(lexed)}
    return names


def scan_rationale_rot(src_root, fn_names):
    """[(key, message)] — comment-lane dataflow claims whose symbol
    resolves nowhere; content-keyed."""
    hits = []
    for crate in census_corpora.jurisdiction_crates(src_root):
        croot = src_root / crate / "src"
        for f in sorted(croot.rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            raw = f.read_text(encoding="utf-8")
            _, _, comments = rust_strip.lex_full(raw, blank_string_bodies=True)
            for a, b in comments:
                for m in ROT_RE.finditer(raw[a:b]):
                    sym = m.group(1)
                    if sym in fn_names:
                        continue
                    lineno = raw.count("\n", 0, a + m.start()) + 1
                    key = f"{rel}\t{sym}"
                    hits.append(
                        (
                            key,
                            f"{rel}:{lineno}: rationale cites `{sym}()` which "
                            f"resolves to no workspace fn — fix-adjacent rot "
                            f"(the close renamed the mechanism; this comment "
                            f"kept the old name); re-derive the comment from "
                            f"the live dataflow",
                        )
                    )
    return hits


def main() -> int:
    args = sys.argv[1:]
    mint = "--mint-rot-grandfather" in args
    verify_landed = "--verify-landed" in args
    args = [a for a in args if not a.startswith("--")]
    src_root = pathlib.Path(args[0])

    lexer_err = rust_strip.selftest()
    if lexer_err:
        print(f"FAIL: shared lexer self-test — {lexer_err}", file=sys.stderr)
        return 1

    # --- self-test arms --------------------------------------------------
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        t = pathlib.Path(td)
        (t / "rio-straw" / "src" / "sla").mkdir(parents=True)
        (t / "rio-straw" / "Cargo.toml").write_text("[package]\n")
        # The live sizing-row plant: a fit-path consult of the gauge
        # field reds (red-by-construction pre-S5; the row is live).
        straw_sizing = t / "rio-scheduler" / "src" / "sla"
        straw_sizing.mkdir(parents=True)
        (t / "rio-scheduler" / "Cargo.toml").write_text("[package]\n")
        (straw_sizing / "fit.rs").write_text("fn d(u: &U) -> u64 { u.disk_used_bytes }\n")
        f_s = check_sizing_field(t)
        if len(f_s) != 1 or "wrong" not in f_s[0] and "sizing" not in f_s[0]:
            print(f"FAIL: the sizing-field plant did not red: {f_s}", file=sys.stderr)
            return 1
        # Rationale-rot plants: a dead-symbol claim reds; a live-symbol
        # claim and a non-claim comment stay green.
        (t / "rio-straw" / "src" / "lib.rs").write_text(
            "// the floor is bumped by bump_resource_floor()\n"
            "// the cap is cleared by ghost_clearer()\n"
            "fn bump_resource_floor() {}\n"
        )
        names = workspace_fn_names(t)
        rot = scan_rationale_rot(t, names)
        if len(rot) != 1 or "ghost_clearer" not in rot[0][1]:
            print(f"FAIL: rationale-rot plants wrong: {rot}", file=sys.stderr)
            return 1

    # --- the real scan ----------------------------------------------------
    fails = check_sizing_field(src_root)
    fn_names = workspace_fn_names(src_root)
    if not fn_names:
        fails.append("population floor — zero workspace fns ((vvvvv))")
    hits = scan_rationale_rot(src_root, fn_names)
    gf_path = src_root / ROT_GRANDFATHER
    if mint:
        if fails:
            for x in fails:
                print(f"mint refused: {x}")
            return 1
        keys = sorted({k for k, _m in hits})
        gf_path.write_text("".join(k + "\n" for k in keys))
        print(f"minted {len(keys)} rationale-rot grandfather entries")
        return 0
    gf = set()
    if gf_path.is_file():
        gf = {x for x in gf_path.read_text().splitlines() if x.strip()}
    live_keys = {k for k, _m in hits}
    fails += [m for k, m in hits if k not in gf]
    for stale in sorted(gf - live_keys):
        fails.append(
            f"{stale.split(chr(9))[0]}: stale rationale-rot grandfather entry "
            f"({stale!r}) — remove it ({ROT_GRANDFATHER}, shrink-only)"
        )
    if verify_landed:
        for name, row in sorted(R33_ROWS.items()):
            if row[0] == "pending":
                fails.append(
                    f"R33 row `{name}` still pending:{row[1]} at the landed "
                    f"verify — the wave-close flips it with the producer's "
                    f"formula signature or the close fails"
                )
                continue
            rel, anchor = row[2], row[3]
            f = src_root / rel
            text = f.read_text(encoding="utf-8") if f.is_file() else ""
            if not re.search(anchor, text):
                fails.append(
                    f"R33 row `{name}`: producer anchor /{anchor}/ does not "
                    f"resolve in {rel} — the producer moved or rotted"
                )
    n_pending = sum(1 for r in R33_ROWS.values() if r[0] == "pending")
    print(
        f"duplicate-derivation lint: {len(R33_ROWS)} registered quantities "
        f"({n_pending} pending slot landings) + the live disk-sizing-input "
        f"row; rationale-rot sweep live ({len(gf)} grandfathered, "
        f"shrink-only; <5% FP latitude recorded, RULED-to-round-13 fallback)"
    )
    if fails:
        print("FAIL: duplicate-derivation/rationale violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
