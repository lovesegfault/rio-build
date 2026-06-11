#!/usr/bin/env python3
"""census-corpora meta-lint (R22 tier-2; see nix/misc-checks.nix).

Argv: <src-root>. The round-9 corpus proved the census GENERATORS are
the evasion surface: every author-census finding was a LINT-GAP
mutation (alias-blind generators, src-only tier scope, hardcoded
label keys, never-enrollable fold-site families). R22's answer:
every census generator declares its UNIVERSE and ships a PLANTED-RED
corpus per evasion axis; this meta-lint makes the generator set
itself censused.

THE REGISTRY below is the closed enrollment set. Per row it pins:
  - where the generator lives (file must exist at the staged tree);
  - the self-test/plant pattern that proves its corpus runs FIRST
    (the house pattern: a check that cannot fail its planted
    fixtures does not gate) - for in-crate generators the plants are
    EMBEDDED fixtures (the registration-census precedent: a check
    quantifying over in-crate fixtures embeds them in the check
    input, never path-references the dev tree);
  - the evasion axes the corpus covers, from the closed vocabulary
    AXES = {alias, scope, label-key, fold-site, reverse-direction,
    tier};
  - axis GAPS, named per row under burn-down semantics: a gap row is
    a RULED record (the axis named, the trigger = the next lint-gap
    finding on that generator upgrades the corpus in the same close,
    per R22); gaps only ever SHRINK - removing a gap requires the
    plant, growing one requires editing this registry (review
    surface).

A generator-shaped file outside the registry is a FAILURE (the
reverse direction over the enrollment set itself); a registry row
whose file or plant pattern is gone is a FAILURE (rot).

Two enforcement arms ride along (their plants embedded here):
  - MODEL-DIVERGENCE grammar (the model-tier drift grep): every
    `MODEL-DIVERGENCE(` token in docs/spec/models MUST match the
    landed grammar `MODEL-DIVERGENCE(law=<id>; tree=<file:anchor>):
    <text> - retarget-by: <trigger>` (single line). A malformed
    header is invisible to the grep that gives the grammar its
    teeth.
  - the NEGATIVE REFUSAL CENSUS (merged_bug_059's class kill):
    `matches!` folds over tonic `Code::` values are BANNED outside
    rio_proto/src/refusal.rs (the one adjudication authority) -
    matches! desugars to `_ => false`, excluding the site from the
    authority's compile-error census. Documented extension sites
    carry `refusal-census: allow(<why>)` within the 6 lines above.
"""

import pathlib
import re
import sys

AXES = {"alias", "scope", "label-key", "fold-site", "reverse-direction", "tier"}

# name, anchor file, plant/self-test pattern (regex over the anchor
# file), axes covered, axis gaps (burn-down rows, named).
REGISTRY = [
    # nix-side generators (self-test arms run first, in-file).
    ("census-enrollment", "nix/census_enrollment.py", r"self-?test", {"scope"}, {"alias", "tier"}),
    ("metric-reason-help-sync", "nix/metric_reason_help_sync.py", r"Self-test.*label-key|label-key self-test", {"label-key", "scope"}, {"alias"}),
    ("rule-citation-versions", "nix/rule_citation_versions.py", r"self-?test", {"tier", "scope"}, set()),
    ("exposure-producer-census", "nix/exposure_producer_census.py", r"self-test arm", {"reverse-direction", "scope"}, set()),
    ("reason-alert-sync", "nix/tests/helm/42-reason-alert-sync.sh", r"Self-test arms run FIRST|self-test arm", {"reverse-direction", "scope"}, set()),
    ("cilium-labels-filter", "nix/cilium-render.nix", r"share-pin|labels", {"scope"}, {"reverse-direction"}),
    ("string-interior-spaces", "nix/string_interior_spaces.py", r"planted red", {"scope", "fold-site"}, {"alias"}),
    ("streaming-open-ban", "nix/streaming_open_ban.py", r"selftest", {"scope"}, {"alias"}),
    ("quint-policy", "nix/quint_policy.py", r"planted RED per rule arm|selftest", {"scope", "fold-site"}, set()),
    ("fixture-provenance", "nix/fixture_provenance.py", r"selftest|planted red", {"scope", "label-key"}, set()),
    # in-crate generators (EMBEDDED corpora per the registration-census
    # precedent; the pattern pins the embed form, not a dev-tree path).
    ("timeout-census", "rio-controller/tests/timeout_census.rs", r"CORPUS_SOURCES|include_str!", {"alias", "scope", "label-key"}, set()),
    ("cap-reader-census", "rio-controller/src/reconcilers/nodeclaim_pool/cover.rs", r"CAP_ALIASES", {"alias", "scope", "tier"}, set()),
    ("vanish-census", "rio-controller/src/reconcilers/nodeclaim_pool/health.rs", r"axis[- ]omission|96-row|provenance.*launched", {"scope"}, {"alias"}),
    ("await-genset", "rio-store/src/substitute.rs", r"genset|GEN-SET", {"fold-site", "scope"}, set()),
    ("cleanup-posture-fold", "rio-store/src/substitute.rs", r"CleanupPosture", {"fold-site"}, set()),
    ("registration-writer-census", "rio-scheduler/src/db/live_pins.rs", r"registration_writer_census", {"scope"}, {"reverse-direction"}),
    ("registration-writer-census-store", "rio-store/src/grpc/put_path/common.rs", r"registration_writer_census", {"scope"}, {"reverse-direction"}),
    ("cell-emission-arm-product", "rio-scheduler/src/actor/snapshot.rs", r"classify_cell_emission", {"scope"}, {"fold-site"}),
    ("subst-dep-eta-disposition", "rio-scheduler/src/actor/tests/misc.rs", r"subst_dep_eta_disposition_census", {"scope"}, set()),
    ("refusal-agreement-census", "rio-builder/src/runtime/pull.rs", r"fatal_set_agrees_with_the_authority", {"scope"}, set()),
]

MODEL_DIVERGENCE = re.compile(
    r"MODEL-DIVERGENCE\(law=[\w.+-]+; tree=[^)]+\): .+ — retarget-by: .+"
)
# An INSTANCE is the token followed by a CONCRETE law id; the grammar
# spec line and prose mentions carry placeholders (`<rule-or-law id>`)
# or a bare token and are not headers.
MD_INSTANCE = re.compile(r"MODEL-DIVERGENCE\(law=[\w.+-]+;")
MD_TOKEN = "MODEL-DIVERGENCE("

REFUSAL_ALLOW = re.compile(r"refusal-census:\s*allow\(")
# A matches! invocation whose body names tonic codes. Conservative
# textual window: from `matches!(` forward ~400 chars within the same
# statement.
MATCHES_CODE = re.compile(r"matches!\s*\(\s*[^;]{0,400}?\bCode::", re.S)


def strip_comments(text: str) -> str:
    return "\n".join(line.split("//")[0] if "//" in line and '"//' not in line else line for line in text.splitlines())


def check_registry(src_root: pathlib.Path):
    fails = []
    seen_axes = set()
    for name, rel, plant_pat, axes, gaps in REGISTRY:
        f = src_root / rel
        if not axes <= AXES or not gaps <= AXES:
            fails.append(f"{name}: axes outside the closed vocabulary {sorted(AXES)}")
            continue
        if axes & gaps:
            fails.append(f"{name}: axis listed both covered and gapped: {sorted(axes & gaps)}")
            continue
        if not f.is_file():
            fails.append(f"{name}: anchor file {rel} missing — registry rot or an unrecorded retirement")
            continue
        if not re.search(plant_pat, f.read_text()):
            fails.append(f"{name}: plant/self-test pattern /{plant_pat}/ not found in {rel} — the corpus or its embed form rotted")
        seen_axes |= axes
    if seen_axes != AXES:
        fails.append(f"axis vocabulary not exercised by any enrolled corpus: {sorted(AXES - seen_axes)}")
    return fails


def check_model_divergence(src_root: pathlib.Path, models_dir="docs/spec/models"):
    fails = []
    count = 0
    for f in sorted((src_root / models_dir).rglob("*")):
        if not f.is_file() or f.suffix not in {".md", ".qnt", ".typ"}:
            continue
        for i, line in enumerate(f.read_text().splitlines(), 1):
            if MD_TOKEN in line and MD_INSTANCE.search(line):
                count += 1
                if not MODEL_DIVERGENCE.search(line):
                    fails.append(
                        f"{f.relative_to(src_root)}:{i}: MODEL-DIVERGENCE header does not match the "
                        f"landed grammar `MODEL-DIVERGENCE(law=<id>; tree=<file:anchor>): <what> — retarget-by: <trigger>`"
                    )
    return fails, count


def scan_refusal_folds(files):
    """files: iterable of (rel, text). Returns violation list."""
    fails = []
    for rel, raw in files:
        lines = raw.splitlines()
        stripped = strip_comments(raw)
        for m in MATCHES_CODE.finditer(stripped):
            lineno = stripped[: m.start()].count("\n") + 1
            window = "\n".join(lines[max(0, lineno - 7) : lineno])
            if REFUSAL_ALLOW.search(window):
                continue
            fails.append(
                f"{rel}:{lineno}: open-coded matches! fold over tonic Code values — refusal "
                f"adjudication lives in rio_proto::refusal (exhaustive match or judge_refusal); "
                f"a documented extension site carries `refusal-census: allow(<why>)` within 6 lines above"
            )
    return fails


# The founding plant (merged_bug_059): the pre-fix builder fold,
# quoted verbatim from the re-point commit. EMBEDDED (the in-crate
# precedent: the plant rides the check input, never a dev-tree path).
FOUNDING_PLANT = """
fn is_fatal_rejection(code: tonic::Code) -> bool {
    matches!(
        code,
        tonic::Code::PermissionDenied
            | tonic::Code::Unauthenticated
            | tonic::Code::Unimplemented
            | tonic::Code::InvalidArgument
    )
}
"""

REFUSAL_SCAN_CRATES = ["rio-builder", "rio-store", "rio-gateway", "rio-scheduler", "rio-controller"]


def main() -> int:
    src_root = pathlib.Path(sys.argv[1])

    # --- self-test arms (planted, must fail) ---------------------------
    # Arm A: the founding refusal plant reds under the negative census.
    f_a = scan_refusal_folds([("planted/pull.rs", FOUNDING_PLANT)])
    if len(f_a) != 1:
        print(f"FAIL: self-test arm A (founding refusal plant) expected 1 violation, got {f_a}", file=sys.stderr)
        return 1
    # Arm B: the allow grammar admits a documented extension site.
    allowed = "// refusal-census: allow(store-classed metadata extension)\n" + FOUNDING_PLANT
    if scan_refusal_folds([("planted/allowed.rs", allowed)]):
        print("FAIL: self-test arm B (allow grammar) — the documented site still flagged", file=sys.stderr)
        return 1
    # Arm C: a comment-lane matches! cannot fire (string/comment safety).
    commented = "// matches!(code, tonic::Code::Internal)\nfn x() {}\n"
    if scan_refusal_folds([("planted/comment.rs", commented)]):
        print("FAIL: self-test arm C (comment lane) — a commented fold fired", file=sys.stderr)
        return 1
    # Arm D: a malformed MODEL-DIVERGENCE header reds the grammar arm.
    bad = "MODEL-DIVERGENCE(law=ctrl.x; missing-the-tree): drifty"
    if MODEL_DIVERGENCE.search(bad):
        print("FAIL: self-test arm D — the malformed header matched the grammar", file=sys.stderr)
        return 1
    good = "MODEL-DIVERGENCE(law=ctrl.nodeclaim.mint-deficit-proportional; tree=nodeclaimLifecycle.qnt:1): sizing arithmetic abstracted — retarget-by: the next mint-law change"
    if not MODEL_DIVERGENCE.search(good):
        print("FAIL: self-test arm D — the well-formed header did not match", file=sys.stderr)
        return 1
    # Arm E: a registry row with a dead anchor reds.
    f_e = check_registry(pathlib.Path("/nonexistent-root"))
    if not any("missing" in x for x in f_e):
        print(f"FAIL: self-test arm E (registry rot) expected missing-anchor failures, got {f_e}", file=sys.stderr)
        return 1

    # --- the real scans -------------------------------------------------
    fails = check_registry(src_root)

    # Reverse direction over the enrollment set itself: every
    # generator-shaped nix-side scanner (a python file under nix/ that
    # self-describes as a census/lint/scanner) must be a registry row —
    # an unregistered generator is exactly the unenrolled-census shape
    # one level up. (In-crate generators are reachable only through
    # their registry rows; their discovery surface is the crate's own
    # census-enrollment lint.)
    registered = {rel for _, rel, _, _, _ in REGISTRY}
    registered.add("nix/census_corpora.py")  # self
    # The shared exact lexer is a LIBRARY consumed by the scanners
    # (one grammar, one place) — not itself a generator with a corpus.
    registered.add("nix/rust_strip.py")
    for f in sorted((src_root / "nix").glob("*.py")):
        rel = f"nix/{f.name}"
        # Generators SELF-DESCRIBE in their module docstring (the
        # house pattern); body mentions (a CI matrix fixture naming a
        # check) are not self-description.
        head = f.read_text()[:1500].lower()
        if ("census" in head or "lint" in head or "scanner" in head) and rel not in registered:
            fails.append(f"{rel}: generator-shaped scanner not enrolled in the census-corpora registry")

    md_fails, md_count = check_model_divergence(src_root)
    fails += md_fails

    refusal_files = []
    for crate in REFUSAL_SCAN_CRATES:
        for f in sorted((src_root / crate / "src").rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            # Production folds only: test dirs and in-file test mods
            # assert specific codes lawfully (the adjudication law
            # governs production classification sites).
            if "/tests/" in rel or rel.endswith("test_helpers.rs"):
                continue
            raw = f.read_text()
            cut = raw.find("#[cfg(test)]")
            if cut != -1:
                raw = raw[:cut]
            refusal_files.append((rel, raw))
    fails += scan_refusal_folds(refusal_files)

    gaps = sorted(f"{name}:{ax}" for name, _, _, _, g in REGISTRY for ax in g)
    print(
        f"census-corpora: {len(REGISTRY)} generators enrolled, axes {sorted(AXES)} all exercised, "
        f"{len(gaps)} grandfathered axis gaps (burn-down: {', '.join(gaps)}), "
        f"{md_count} MODEL-DIVERGENCE headers grammar-checked, "
        f"{len(refusal_files)} files swept by the negative refusal census"
    )
    if fails:
        print("FAIL: census-corpora violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
