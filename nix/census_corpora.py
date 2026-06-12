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
    surface);
  - DERIVED_FROM (R22', bug_151/merged_bug_090): the production
    table / refusal predicate INSIDE the generator that its coverage
    is computed from (a pattern that must resolve in the anchor
    file). A row claiming ZERO gaps without a derived_from is a
    FAILURE - the round-10 corpus carried two self-certified
    zero-gap rows whose axes were demonstrably unplanted (the
    timeout census's import-grammar forms; quint-policy's alias
    forms): a registry row whose covered/gap set the generator
    cannot itself compute is exactly the self-reporting this field
    kills. Burn-down rows (named gaps) may predate the discipline;
    CLOSING the last gap requires deriving.

A generator-shaped file outside the registry is a FAILURE (the
reverse direction over the enrollment set itself); a registry row
whose file, plant pattern, or derived_from anchor is gone is a
FAILURE (rot).

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

Lexing is STRUCTURAL, not conventional (merged_bug_009): this scanner
consumed the exact modes it polices — a naive per-line comment split
and a truncate-at-first-`#[cfg(test)]` scope prune that left the bulk
of jobs.rs/substitute.rs/actor mod.rs production bodies unswept by
the refusal census. Both lanes now route through the shared exact
lexer (nix/rust_strip.py: comment/string blanking + the
attribute-position cfg(test) pruner), and a third enforcement arm
rides along: open-coded `split` on the line-comment delimiter in ANY
nix/*.py scanner is BANNED (the shadow-stripper negative census) —
canonicalization is enforced, not asked for.
"""

import pathlib
import re
import sys

import rust_strip

AXES = {"alias", "scope", "label-key", "fold-site", "reverse-direction", "tier"}

# name, anchor file, plant/self-test pattern (regex over the anchor
# file), axes covered, axis gaps (burn-down rows, named), derived_from
# (regex naming the in-generator production table / refusal predicate
# coverage is computed from; REQUIRED for zero-gap rows, None only on
# burn-down rows that predate the discipline).
REGISTRY = [
    # nix-side generators (self-test arms run first, in-file).
    ("census-enrollment", "nix/census_enrollment.py", r"self-?test", {"scope"}, {"alias", "tier"}, r"CLAIM_SHAPES"),
    ("metric-reason-help-sync", "nix/metric_reason_help_sync.py", r"Self-test.*label-key|label-key self-test", {"label-key", "scope"}, {"alias"}, r"LABEL_KEYS"),
    ("rule-citation-versions", "nix/rule_citation_versions.py", r"self-?test", {"tier", "scope"}, set(), r"productions = \["),
    ("exposure-producer-census", "nix/exposure_producer_census.py", r"self-test arm", {"reverse-direction", "scope"}, set(), r"DISPOSITIONS = \{"),
    ("reason-alert-sync", "nix/tests/helm/42-reason-alert-sync.sh", r"Self-test arms run FIRST|self-test arm", {"reverse-direction", "scope"}, set(), r"INTENT_DROP_REASONS"),
    ("cilium-labels-filter", "nix/cilium-render.nix", r"share-pin|labels", {"scope"}, {"reverse-direction"}, None),
    ("string-interior-spaces", "nix/string_interior_spaces.py", r"planted red", {"scope", "fold-site"}, {"alias"}, None),
    ("streaming-open-ban", "nix/streaming_open_ban.py", r"selftest", {"scope"}, {"alias"}, None),
    ("quint-policy", "nix/quint_policy.py", r"planted RED per rule arm|selftest", {"scope", "fold-site"}, set(), r"def conj_expansion"),
    ("quantifier-lexicon", "nix/quantifier_lexicon.py", r"self-?test", {"tier", "scope"}, set(), r"LEXICON = \("),
    ("fixture-provenance", "nix/fixture_provenance.py", r"selftest|planted red", {"scope", "label-key"}, set(), r"LANES = \{"),
    # in-crate generators (EMBEDDED corpora per the registration-census
    # precedent; the pattern pins the embed form, not a dev-tree path).
    ("timeout-census", "rio-controller/tests/timeout_census.rs", r"CORPUS_SOURCES|include_str!", {"alias", "scope", "label-key"}, set(), r"USE_GRAMMAR"),
    ("cap-reader-census", "rio-controller/src/reconcilers/nodeclaim_pool/cover.rs", r"CAP_ALIASES", {"alias", "scope", "tier"}, set(), r"CAP_ALIASES"),
    ("vanish-census", "rio-controller/src/reconcilers/nodeclaim_pool/health.rs", r"axis[- ]omission|96-row|provenance.*launched", {"scope"}, {"alias"}, None),
    ("await-genset", "rio-store/src/substitute.rs", r"genset|GEN-SET", {"fold-site", "scope"}, set(), r"acquire-site census"),
    ("cleanup-posture-fold", "rio-store/src/substitute.rs", r"CleanupPosture", {"fold-site"}, set(), r"enum CleanupPosture"),
    ("registration-writer-census", "rio-scheduler/src/db/live_pins.rs", r"registration_writer_census", {"scope"}, {"reverse-direction"}, None),
    ("registration-writer-census-store", "rio-store/src/grpc/put_path/common.rs", r"registration_writer_census", {"scope"}, {"reverse-direction"}, None),
    ("cell-emission-arm-product", "rio-scheduler/src/actor/snapshot.rs", r"classify_cell_emission", {"scope"}, {"fold-site"}, None),
    ("subst-dep-eta-disposition", "rio-scheduler/src/actor/tests/misc.rs", r"subst_dep_eta_disposition_census", {"scope"}, set(), r"SubstDepEta"),
    ("refusal-agreement-census", "rio-builder/src/runtime/pull.rs", r"fatal_set_agrees_with_the_authority", {"scope"}, set(), r"judge_refusal"),
    # Riding arm of THIS file (the refusal-census precedent): the
    # WireSecs pacing-seam census — raw `from_secs` over `*_seconds`
    # proto fields is banned in production code; proto→sleep seams
    # mint through WireSecs::pacing(domain_ceiling) (merged_bug_156).
    # WO-S8-3 (bug_091): the plant set derives from the GENERATED
    # idiom×site product (WIRE_SECS_IDIOMS × WIRE_SECS_SITES — the
    # host language's conversion idioms, not the observed spellings),
    # so the row's claim is grammar⊇product BY DERIVATION with the
    # coarse backstop tier as the over-approximation belt. The former
    # fold-site burn-down gap RETIRED at this close (CE-7): the
    # backstop is its machine-bound compensating control — whole-file
    # beyond-window binding consumption + bare `*_seconds`-ident
    # arguments (the same-file helper-fn face), firing predicate
    # pinned by WIRE_SECS_BACKSTOP_VECTORS in the self-test.
    # Cross-file consumption is outside the file-local scanner's
    # jurisdiction by construction — disclosed, not silently claimed.
    ("wire-secs-pacing-seams", "nix/census_corpora.py", r"WIRE_SECS_GRAMMAR", {"alias", "scope", "fold-site"}, set(), r"WIRE_SECS_IDIOMS = \["),
    # bw10 wave enrollments (the integrator's close): the round-10
    # slots' new R22'-derived censuses, each row computed from the
    # named in-generator refusal predicate / production table.
    ("destructive-lane-census", "rio-store/src/gc/lane.rs", r"planted red", {"scope"}, set(), r"reaches-delete-sink"),
    ("witnessed-disposition-product", "rio-scheduler/src/actor/floor.rs", r"witnessed_disposition_product_census", {"scope"}, set(), r"WITNESSED_LETTERS"),
    ("cell-emission-wire-injectivity", "rio-scheduler/src/actor/tests/sla_contract.rs", r"w10z_cell_emission_wire_image_injectivity", {"scope"}, set(), r"classify_cell_emission"),
    ("pool-demand-view-consumers", "rio-controller/src/reconcilers/pool/jobs.rs", r"W10-AH census", {"scope"}, set(), r"iter_page"),
    ("leader-edges-census", "rio-scheduler/src/observability.rs", r"Every LEADER_EDGES row is named and total", {"scope"}, set(), r"LEADER_EDGES"),
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


# The shadow-stripper ban (the negative census over the scanner set
# itself): an open-coded `.split` on the line-comment delimiter is the
# exact lexer this meta-lint exists to forbid — every nix/*.py scanner
# consumes rust_strip instead. The needle is built by concatenation so
# this file's own source never carries the banned token.
SHADOW_STRIPPER = re.compile(r"\.split\(\s*['\"]/" + r"/['\"]\s*\)")


def strip_production(text: str, source: str = "<input>") -> str:
    """The shared production-scan pipeline (merged_bug_009): the
    attribute-position cfg(test) pruner, then comments AND string
    bodies blanked — newline-preserving throughout, so violation line
    numbers are stable. Mid-file test modules are pruned in place;
    production code after them stays in the scan.

    WO-S8-1 (R22″): the pruner FAILS CLOSED — a `StripError` (depth
    underflow, unmatched delimiter, unclassifiable extent) propagates
    to the per-file scan loop, which converts it into a named
    violation instead of skipping the file or scanning a mis-pruned
    population."""
    pruned = rust_strip.strip_cfg_test(text, source=source)
    out, _ = rust_strip.lex(pruned, blank_string_bodies=True)
    return out


def check_registry(src_root: pathlib.Path, registry=None):
    registry = REGISTRY if registry is None else registry
    fails = []
    seen_axes = set()
    for name, rel, plant_pat, axes, gaps, derived_from in registry:
        f = src_root / rel
        if not axes <= AXES or not gaps <= AXES:
            fails.append(f"{name}: axes outside the closed vocabulary {sorted(AXES)}")
            continue
        if axes & gaps:
            fails.append(f"{name}: axis listed both covered and gapped: {sorted(axes & gaps)}")
            continue
        # R22' (bug_151/merged_bug_090): a zero-gap claim must name the
        # computable production surface it derives from — a registry
        # row whose covered/gap set the generator cannot itself
        # compute is the self-certification this check kills.
        if not gaps and derived_from is None:
            fails.append(
                f"{name}: SELF-CERTIFIED zero-gap row — name the generator's "
                f"production table / refusal predicate in derived_from, or "
                f"record the real gaps as burn-down rows"
            )
            continue
        if not f.is_file():
            fails.append(f"{name}: anchor file {rel} missing — registry rot or an unrecorded retirement")
            continue
        text = f.read_text()
        if not re.search(plant_pat, text):
            fails.append(f"{name}: plant/self-test pattern /{plant_pat}/ not found in {rel} — the corpus or its embed form rotted")
        if derived_from is not None and not re.search(derived_from, text):
            fails.append(
                f"{name}: derived_from anchor /{derived_from}/ not found in {rel} — "
                f"the production table the coverage claim derives from rotted"
            )
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
        try:
            stripped = strip_production(raw, rel)
        except rust_strip.StripError as e:
            # R22″ fail-closed: an unclassifiable extent is a NAMED
            # census failure, never a silent skip.
            fails.append(f"{e} [refusal census: file not classifiable]")
            continue
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

# --- the WireSecs pacing-seam census (merged_bug_156, S6) -------------
#
# Proto seconds fields (`*_seconds` by the proto naming convention)
# must cross the proto→domain seam through the WireSecs constructors
# (`from_wire` / `.pacing(domain_ceiling)`), never raw
# `Duration::from_secs` — the clamp law was re-minted-around twice
# (wave-8 store seam, pre-campaign builder seam) because the forbidden
# shape still compiled at any new seam.
#
# WO-S8-3 (bug_091, R22″): the grammar table is GENERATED from the
# host language's conversion-idiom product — idioms × consumption
# sites, never the observed spellings (the old four-production hand
# table proved plants⊇grammar while the registry row read it as
# grammar⊇language; conversion-at-binding `u64::from(f)` and
# cast-at-binding `f as u64` were both invisible). Two tiers:
#
#   PRECISE (the grammar): a structural pass — every `from_secs(…)`
#   call whose paren-matched argument extent mentions a `*_seconds`
#   field access (any idiom, any qualification), plus every
#   `let`-binding whose RHS reads a `*_seconds` field (any idiom,
#   multi-line RHS included) consumed by a `from_secs` whose argument
#   names the binding within WIRE_SECS_LET_WINDOW lines.
#
#   BACKSTOP (coarse, the over-approximation belt): the SAME binding
#   consumed by a from_secs BEYOND the window — whole-file — and any
#   from_secs argument naming a BARE `*_seconds`-suffixed ident (the
#   helper-fn face: a parameter carrying the proto naming convention
#   into a same-file callee). The backstop is the MACHINE-BOUND
#   compensating control for the retired fold-site gap row (CE-7):
#   its firing predicate is pinned by the beyond-window vector in the
#   self-test below. Cross-FILE consumption is outside a file-local
#   scanner's jurisdiction by construction — disclosed, not silently
#   claimed (the mac-census jurisdiction form).
#
# WIRE_SECS_GRAMMAR is the generated product (idioms × sites) plus
# the qualification vector; the completeness meta-pin (W11-BS form)
# asserts every product cell has a vector and every vector fires —
# a silently dropped cell (e.g. try-from-at-inline) is a red.
WIRE_SECS_ALLOW = re.compile(r"wire-secs-census:\s*allow\(")
WIRE_SECS_LET_WINDOW = 30
# A *_seconds FIELD READ: the proto naming-convention suffix, NOT
# followed by `(` — a method call (`span.get_seconds()`, the jiff
# local-clock idiom at componentscaler/mod.rs:430) is a different
# grammatical production carrying no wire data. The leniency is
# PLANTED (method-call-read vector below) and MACHINE-BOUND: prost
# only mints `*_seconds()` getter reads for `optional` proto fields,
# and the proto-source trigger arm (scan_proto_optional_seconds)
# fails the census the moment an `optional … *_seconds` field
# appears — the getter production must join the grammar in the same
# close (the named trigger, mechanically watched).
WIRE_SECS_FIELD = re.compile(r"\.\w*_seconds\b(?!\s*\()")
WIRE_SECS_BARE_IDENT = re.compile(r"\b(?<!\.)[a-z]\w*_seconds\b(?!\s*\()")
WIRE_SECS_CALL = re.compile(r"\bfrom_secs\s*\(")
# Any-idiom binding: `let [mut] NAME [: ty] = <RHS containing a
# *_seconds field read> ;` — RHS spans lines (DOTALL via [^;]).
WIRE_SECS_LET_ANY = re.compile(
    r"\blet\s+(?:mut\s+)?(\w+)\s*(?::[^=;]*)?=\s*([^;]*?\.\w*_seconds\b(?!\s*\()[^;]*?);"
)
WIRE_SECS_PROTO_OPTIONAL = re.compile(r"\boptional\s+\w+\s+\w*_seconds\s*=")


def scan_proto_optional_seconds(src_root):
    """The method-call leniency's TRIGGER arm (machine-bound): an
    `optional … *_seconds` proto field would make prost mint a
    `*_seconds()` getter — a wire read in the method-call form the
    field-read grammar excludes. Zero such fields exist; the first
    one fails the census until the getter production joins the
    grammar."""
    fails = []
    proto_dir = src_root / "rio-proto" / "proto"
    for f in sorted(proto_dir.glob("*.proto")) if proto_dir.is_dir() else []:
        for i, line in enumerate(f.read_text().splitlines(), 1):
            if WIRE_SECS_PROTO_OPTIONAL.search(line):
                fails.append(
                    f"rio-proto/proto/{f.name}:{i}: `optional … *_seconds` proto field — "
                    f"prost mints a `*_seconds()` getter, a wire read in the method-call "
                    f"form WIRE_SECS_FIELD excludes; add the getter production to the "
                    f"wire-secs grammar (with its plant) in the same change"
                )
    return fails

# The conversion-idiom × consumption-site PRODUCT ([GEN-SET] — the
# table is generated, the cells are the derivation source for the
# plant set; the two historical-escape cells are exactly bug_091's
# named productions).
WIRE_SECS_IDIOMS = [
    ("bare", "{f}"),
    ("from", "u64::from({f})"),
    ("cast", "{f} as u64"),
    ("try-from", "u64::try_from({f}).unwrap()"),
    ("into", "{f}.into()"),
]
WIRE_SECS_SITES = [
    ("inline", "let d = Duration::from_secs({expr});\n"),
    (
        "binding",
        "let hint = {expr};\nlet d = Duration::from_secs(u64::from(hint));\n",
    ),
]


def _wire_secs_product():
    f = "resp.retry_after_seconds"
    for iname, itmpl in WIRE_SECS_IDIOMS:
        for sname, stmpl in WIRE_SECS_SITES:
            yield (f"{iname}-at-{sname}", stmpl.format(expr=itmpl.format(f=f)))


WIRE_SECS_GRAMMAR = list(_wire_secs_product()) + [
    # The qualification axis of the call itself (one historical
    # production kept beside the product).
    ("qualified-call", "let d = std::time::Duration::from_secs(resp.retry_after_seconds);\n"),
]

# The backstop's own firing-predicate vectors (CE-7: the fold-site
# bind — the census test asserts the backstop FIRES on these).
WIRE_SECS_BACKSTOP_VECTORS = [
    (
        "beyond-window-binding",
        "let hint = resp.retry_after_seconds;\n"
        + "let _pad = 0;\n" * (WIRE_SECS_LET_WINDOW + 5)
        + "let d = Duration::from_secs(u64::from(hint));\n",
    ),
    (
        "helper-fn-param",
        "fn pace(retry_after_seconds: u64) -> Duration {\n"
        "    Duration::from_secs(retry_after_seconds)\n"
        "}\n",
    ),
]


def scan_wire_secs_seams(files):
    """files: iterable of (rel, text). Returns violation list (both
    tiers; one report per site, backstop deduped against precise)."""
    fails = []
    for rel, raw in files:
        lines = raw.splitlines()
        # Seam reconciliation (bw10 close): the arm was authored
        # against the pre-merged_bug_009 stripper; it now rides the
        # shared production pipeline (attribute-position cfg(test)
        # prune + comment/string blanking) like every arm here.
        try:
            stripped = strip_production(raw, rel)
        except rust_strip.StripError as e:
            # R22″ fail-closed (same arm as the refusal census).
            fails.append(f"{e} [wire-secs census: file not classifiable]")
            continue

        seen = set()

        def flag(lineno, what):
            if lineno in seen:
                return
            window = "\n".join(lines[max(0, lineno - 7) : lineno])
            if WIRE_SECS_ALLOW.search(window):
                return
            seen.add(lineno)
            fails.append(
                f"{rel}:{lineno}: {what} — proto seconds cross the seam through "
                f"rio_common::clamped::WireSecs (from_wire / .pacing(domain_ceiling)); "
                f"a documented exception carries `wire-secs-census: allow(<why>)` within 6 lines above"
            )

        # The from_secs call extents, paren-matched structurally (the
        # idiom-blind pass: ANY conversion idiom inside the argument
        # is caught, `::`-qualified or cast or method-chained).
        calls = []  # (call_lineno, arg_text)
        for m in WIRE_SECS_CALL.finditer(stripped):
            po = m.end() - 1
            pe = rust_strip._match_delim(stripped, po)
            calls.append((stripped[: m.start()].count("\n") + 1, stripped[po + 1 : pe - 1]))
        for lineno, arg in calls:
            if WIRE_SECS_FIELD.search(arg):
                flag(lineno, "raw from_secs over a `*_seconds` proto field")
        # The binding arm (precise within the window) + the
        # beyond-window backstop (whole file).
        for lm in WIRE_SECS_LET_ANY.finditer(stripped):
            binding = lm.group(1)
            let_line = stripped[: lm.start()].count("\n") + 1
            pat = re.compile(rf"\b{re.escape(binding)}\b")
            for call_line, arg in calls:
                if call_line >= let_line and pat.search(arg):
                    if call_line - let_line <= WIRE_SECS_LET_WINDOW:
                        flag(
                            let_line,
                            f"a `*_seconds` read let-bound to `{binding}` then raw from_secs'd",
                        )
                    else:
                        flag(
                            let_line,
                            f"a `*_seconds` read let-bound to `{binding}` then raw "
                            f"from_secs'd beyond the {WIRE_SECS_LET_WINDOW}-line window "
                            f"(backstop tier)",
                        )
        # The helper-fn-face backstop: a from_secs argument naming a
        # BARE `*_seconds` ident (a parameter/local carrying the proto
        # naming convention — the same-file helper-fn seam).
        for lineno, arg in calls:
            if WIRE_SECS_BARE_IDENT.search(arg):
                flag(
                    lineno,
                    "raw from_secs over a bare `*_seconds` ident (backstop tier: "
                    "a parameter/local carrying the proto seconds convention)",
                )
    return fails


# --- the retired-knob-phrase arm (WO-S8-9, merged_bug_055) -------------
#
# store_pool_cpu_limit's three doc tiers (deploy.rs fn doc, the
# derive_store_ceiling doc, values.yaml's backstop comment) kept
# naming the retired rio-general pool after wave-9's D1 rename moved
# the read to the rio-store pool — the identifier-driven sweep missed
# prose naming the knob by its VALUES-PATH phrase. The phrases below
# are RETIRED knob paths: zero live occurrences (this close swept
# them), and any reintroduction fails tree-wide — the phrase-driven
# belt the identifier sweep lacked.
RETIRED_KNOB_PHRASES = [
    # (phrase, replacement guidance)
    (
        "nodePools[rio-general].limits.cpu",
        "the store scale knob is karpenter.nodePools[rio-store].limits.cpu (D1)",
    ),
]
KNOB_PHRASE_SUFFIXES = {".rs", ".nix", ".py", ".sh", ".md", ".yaml", ".yml", ".toml"}


def scan_retired_knob_phrases(src_root):
    fails = []
    for f in sorted(src_root.rglob("*")):
        if not f.is_file() or f.suffix not in KNOB_PHRASE_SUFFIXES:
            continue
        rel = f.relative_to(src_root).as_posix()
        parts = rel.split("/")
        if any(p in (".git", "target", "result", "node_modules") for p in parts):
            continue
        if rel == "nix/census_corpora.py":
            continue  # this table names the phrases as data
        try:
            text = f.read_text(encoding="utf-8")
        except (UnicodeDecodeError, OSError):
            continue
        for phrase, guidance in RETIRED_KNOB_PHRASES:
            for i, line in enumerate(text.splitlines(), 1):
                if phrase in line:
                    fails.append(
                        f"{rel}:{i}: retired knob phrase `{phrase}` — {guidance}"
                    )
    return fails


# --- the retention-registry numeric-claim arm (WO-S8-7) ---------------
#
# merged_bug_081: the retention registry lints SYMBOL linkage (xtask
# RetentionTruth) while its free-prose note fields rot — the 24h
# fence-horizon claim survived two re-derivations of the constant it
# narrated. Numeric duration figures are BANNED in the registry's
# string fields (derived-or-banned: the describe scrape is
# literal-only, so figures cannot interpolate — prose cites the
# deriving SYMBOL instead and the figure lives once, at the const).
RETENTION_REGISTRY = "rio-migrations/src/retention.rs"
DURATION_FIGURE = re.compile(
    r"\b\d+(?:\.\d+)?\s*(?:h|hr|hrs|hour|hours|d|day|days|min|mins|"
    r"minute|minutes|s|sec|secs|second|seconds|ms)\b"
)


def scan_retention_notes(src_root):
    """Duration figures inside ANY string literal of the retention
    registry are reds (the lexer's string spans — comments are
    commentary, code symbols carry no figures)."""
    f = src_root / RETENTION_REGISTRY
    if not f.is_file():
        return [f"{RETENTION_REGISTRY} missing — the retention registry moved; re-anchor this arm"]
    text = f.read_text()
    _, spans, _ = rust_strip.lex_full(text, blank_string_bodies=False)
    fails = []
    for a, b, _raw in spans:
        m = DURATION_FIGURE.search(text[a:b])
        if m:
            lineno = text.count("\n", 0, a + m.start()) + 1
            fails.append(
                f"{RETENTION_REGISTRY}:{lineno}: numeric duration figure "
                f"`{m.group(0)}` in a registry string — registry figures rot "
                f"(the 24h fence-horizon lesson, merged_bug_081); cite the "
                f"deriving symbol and keep the figure at its const"
            )
    return fails


REFUSAL_SCAN_CRATES = ["rio-builder", "rio-store", "rio-gateway", "rio-scheduler", "rio-controller"]


def main() -> int:
    src_root = pathlib.Path(sys.argv[1])

    # A broken shared lexer fails closed before any scan may gate.
    lexer_err = rust_strip.selftest()
    if lexer_err:
        print(f"FAIL: shared lexer self-test — {lexer_err}", file=sys.stderr)
        return 1

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
    # Arm E2 (W10-CC — the R22' recursion close): a strawman row
    # claiming ZERO gaps with NO derived_from production table is the
    # bug_151/merged_bug_090 self-certification shape; the registry
    # check itself reds on it.
    strawman = [("strawman-zero-gap", "nix/census_corpora.py", r"REGISTRY", {"scope"}, set(), None)]
    f_e2 = check_registry(pathlib.Path("/nonexistent-root"), strawman)
    if len(f_e2) < 1 or "SELF-CERTIFIED" not in f_e2[0]:
        print(f"FAIL: self-test arm E2 (zero-gap self-certification) expected the strawman row red, got {f_e2}", file=sys.stderr)
        return 1
    # Arm E3: a derived_from anchor that no longer resolves in the
    # generator is rot, same as a dead plant pattern. (The needle is
    # concatenation-built so this file never carries it.)
    rotted = [("rotted-derivation", "nix/census_corpora.py", r"REGISTRY", {"scope"}, set(), r"NO_SUCH_" + r"PRODUCTION_TABLE")]
    f_e3 = check_registry(src_root, rotted)
    if not any("derived_from anchor" in x for x in f_e3):
        print(f"FAIL: self-test arm E3 (derived_from rot) expected the rotted anchor red, got {f_e3}", file=sys.stderr)
        return 1
    # Arm F (merged_bug_009, the SCOPE axis, planted at the outermost
    # layer — raw source in, violations out): a production fold AFTER
    # an early cfg(test) module. The old truncate-at-first-marker prune
    # ended the scan at the test module, so this arm was a silent miss
    # (the red the wave-9 corpus never planted); the attribute-position
    # pruner resumes after the item and the fold fires.
    scope_plant = (
        "#[cfg(test)]\nmod tests {\n    fn t() { let _ = 1; }\n}\n"
        "fn classify(code: tonic::Code) -> bool {\n"
        "    matches!(code, tonic::Code::Internal)\n}\n"
    )
    f_f = scan_refusal_folds([("planted/scope.rs", scope_plant)])
    if len(f_f) != 1:
        print(f"FAIL: self-test arm F (production fold after early cfg(test)) expected 1 violation, got {f_f}", file=sys.stderr)
        return 1
    # ... and the cfg(test)-INTERIOR fold stays out of the population.
    interior = "#[cfg(test)]\nmod tests {\n    fn t() { assert!(matches!(c, tonic::Code::Internal)); }\n}\n"
    if scan_refusal_folds([("planted/interior.rs", interior)]):
        print("FAIL: self-test arm F' — a cfg(test)-interior fold entered the production census", file=sys.stderr)
        return 1
    # Arm F″ (WO-S8-1, R22″ fail-closed): a file whose cfg(test)
    # extent the pruner cannot classify is a NAMED census failure
    # (file:line in the message), never a silent skip or a scan over
    # a mis-pruned population.
    refused = scan_refusal_folds([("planted/refused.rs", "#[cfg(test)]\nconst X: u8 = 1")])
    if len(refused) != 1 or "planted/refused.rs:1" not in refused[0]:
        print(f"FAIL: self-test arm F″ (fail-closed pruner refusal) expected the named refusal, got {refused}", file=sys.stderr)
        return 1
    # Arm G (merged_bug_009, the LEXICAL axis): a block-comment fold
    # must not fire, and a `//`-bearing URL inside a string must not
    # eat the rest of the line (the naive per-line split truncated
    # here — string-blind). The real fold beneath them must fire
    # EXACTLY once.
    lexical_plant = (
        "/* commented out:\n   matches!(code, tonic::Code::Internal)\n*/\n"
        'const DOCS: &str = "https://example.com/refusals"; // prose: matches!(code, tonic::Code::Aborted)\n'
        "fn classify(code: tonic::Code) -> bool {\n"
        "    matches!(code, tonic::Code::Unavailable)\n}\n"
    )
    f_g = scan_refusal_folds([("planted/lexical.rs", lexical_plant)])
    if len(f_g) != 1 or "planted/lexical.rs:6" not in f_g[0]:
        print(f"FAIL: self-test arm G (block-comment + url-in-string) expected exactly the line-6 violation, got {f_g}", file=sys.stderr)
        return 1
    # Arm H (the shadow-stripper ban's own red, W10-BY): a strawman
    # scanner open-coding the line-comment split is flagged. The
    # needle is concatenation-built so this file never carries it.
    strawman = "def strip(text):\n    return text.split(" + '"/' + '/")[0]\n'
    if not SHADOW_STRIPPER.search(strawman):
        print("FAIL: self-test arm H (shadow-stripper ban) — the strawman open-coded stripper did not match", file=sys.stderr)
        return 1
    if SHADOW_STRIPPER.search('text.split("|")[0] + "//"'):
        print("FAIL: self-test arm H' — the ban matched a non-stripper split", file=sys.stderr)
        return 1

    # Arm F: the WireSecs pacing-seam plants — DERIVED from the
    # idiom×site PRODUCT (WO-S8-3, R22″): one red per generated cell.
    # W11-BV red-first: conversion-at-binding (`u64::from(f)` at the
    # let) and cast-at-binding (`f as u64`) were invisible to the old
    # hand grammar (0 violations pre-fix, transcripts in the commit);
    # both are product cells now and must fire like every other.
    for production, snippet in WIRE_SECS_GRAMMAR:
        f_f = scan_wire_secs_seams([(f"planted/{production}.rs", snippet)])
        if len(f_f) != 1:
            print(
                f"FAIL: self-test arm F (wire-secs plant `{production}`) expected 1 violation, got {f_f}",
                file=sys.stderr,
            )
            return 1
    # The W11-BS-form completeness META-PIN (WS-8): the grammar table
    # is the GENERATED product — every idiom×site cell has exactly one
    # entry (plus the qualification vector); a table that silently
    # drops a cell (e.g. try-from-at-inline) is red HERE, not at the
    # next escape.
    want_cells = {
        f"{i}-at-{s}" for i, _ in WIRE_SECS_IDIOMS for s, _ in WIRE_SECS_SITES
    } | {"qualified-call"}
    got_cells = {name for name, _ in WIRE_SECS_GRAMMAR}
    if got_cells != want_cells:
        print(
            f"FAIL: wire-secs completeness meta-pin — table cells != idiom×site "
            f"product: missing {sorted(want_cells - got_cells)}, extra "
            f"{sorted(got_cells - want_cells)}",
            file=sys.stderr,
        )
        return 1
    # The BACKSTOP's firing predicate (CE-7: the retired fold-site gap
    # row MACHINE-BINDS here — the compensating control demonstrably
    # fires on the beyond-window and helper-fn-param classes).
    for vec_name, snippet in WIRE_SECS_BACKSTOP_VECTORS:
        f_b = scan_wire_secs_seams([(f"planted/{vec_name}.rs", snippet)])
        if len(f_b) != 1 or "backstop tier" not in f_b[0]:
            print(
                f"FAIL: backstop firing-predicate vector `{vec_name}` expected 1 "
                f"backstop-tier violation, got {f_b}",
                file=sys.stderr,
            )
            return 1
    # The method-call LENIENCY plant (one plant per leniency point,
    # R22″): a `*_seconds()` METHOD call is local-clock arithmetic
    # (jiff get_seconds — componentscaler/mod.rs:430), not a wire
    # field read — it must NOT flag; its wire-side trigger is the
    # proto-source arm below.
    method_call = (
        "let secs = span.get_seconds();\n"
        "let d = Duration::from_secs(secs as u64);\n"
    )
    if scan_wire_secs_seams([("planted/method_call.rs", method_call)]):
        print(
            "FAIL: the method-call leniency plant flagged — local-clock "
            "`.get_seconds()` reads are not wire fields",
            file=sys.stderr,
        )
        return 1
    # … and the trigger arm fires on a strawman optional proto field.
    import tempfile

    with tempfile.TemporaryDirectory() as td:
        straw_root = pathlib.Path(td)
        (straw_root / "rio-proto" / "proto").mkdir(parents=True)
        (straw_root / "rio-proto" / "proto" / "x.proto").write_text(
            "message M { optional uint32 retry_after_seconds = 3; }\n"
        )
        f_t = scan_proto_optional_seconds(straw_root)
        if len(f_t) != 1 or "getter production" not in f_t[0]:
            print(
                f"FAIL: the optional-*_seconds trigger arm did not fire on the strawman: {f_t}",
                file=sys.stderr,
            )
            return 1
    # W11-BZ (WO-S8-7): a planted stale figure in a retention-registry
    # note field is a census red — and a figure-free symbol-citing
    # note passes.
    with tempfile.TemporaryDirectory() as td:
        straw_root = pathlib.Path(td)
        (straw_root / "rio-migrations" / "src").mkdir(parents=True)
        reg = straw_root / RETENTION_REGISTRY
        reg.write_text('const N: &str = "rows older than 24h are swept";\n')
        f_z = scan_retention_notes(straw_root)
        if len(f_z) != 1 or "24h" not in f_z[0]:
            print(f"FAIL: W11-BZ — the planted 24h note figure did not red: {f_z}", file=sys.stderr)
            return 1
        reg.write_text(
            'const N: &str = "rows older than the credential-derived horizon CONFIRM_FENCE_GC_SECS";\n'
        )
        if scan_retention_notes(straw_root):
            print("FAIL: W11-BZ — the symbol-citing figure-free note flagged", file=sys.stderr)
            return 1
    # The retired-knob-phrase belt's plant (WO-S8-9): a strawman file
    # reintroducing the rio-general knob path is a named red.
    with tempfile.TemporaryDirectory() as td:
        straw_root = pathlib.Path(td)
        (straw_root / "doc.md").write_text(
            "raise karpenter." + "nodePools[rio-general].limits.cpu to scale\n"
        )
        f_k = scan_retired_knob_phrases(straw_root)
        if len(f_k) != 1 or "retired knob phrase" not in f_k[0]:
            print(f"FAIL: the retired-knob-phrase plant did not red: {f_k}", file=sys.stderr)
            return 1
    # Arm F-allow: the allow grammar admits a documented exception.
    allowed_ws = "// wire-secs-census: allow(test fixture builds its own clock)\n" + WIRE_SECS_GRAMMAR[0][1]
    if scan_wire_secs_seams([("planted/allowed.rs", allowed_ws)]):
        print("FAIL: self-test arm F-allow — the documented site still flagged", file=sys.stderr)
        return 1
    # Arm F-mint: the sanctioned constructor shape does NOT flag.
    minted = "let d = WireSecs::from_wire(u64::from(resp.retry_after_seconds)).pacing(CEILING);\n"
    if scan_wire_secs_seams([("planted/minted.rs", minted)]):
        print("FAIL: self-test arm F-mint — the sanctioned WireSecs mint flagged", file=sys.stderr)
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
    registered = {rel for _, rel, *_ in REGISTRY}
    registered.add("nix/census_corpora.py")  # self
    # The shared exact lexer is a LIBRARY consumed by the scanners
    # (one grammar, one place) — not itself a generator with a corpus.
    registered.add("nix/rust_strip.py")
    for f in sorted((src_root / "nix").glob("*.py")):
        rel = f"nix/{f.name}"
        text = f.read_text()
        # Generators SELF-DESCRIBE in their module docstring (the
        # house pattern); body mentions (a CI matrix fixture naming a
        # check) are not self-description.
        head = text[:1500].lower()
        if ("census" in head or "lint" in head or "scanner" in head) and rel not in registered:
            fails.append(f"{rel}: generator-shaped scanner not enrolled in the census-corpora registry")
        # The shadow-stripper ban (merged_bug_009): every scanner
        # consumes the shared lexer; an open-coded line-comment split
        # is the per-scanner lexing this meta-lint exists to kill.
        for m in SHADOW_STRIPPER.finditer(text):
            lineno = text[: m.start()].count("\n") + 1
            fails.append(
                f"{rel}:{lineno}: open-coded line-comment split — route through "
                f"nix/rust_strip.py (lex/strip_cfg_test); shadow strippers are banned"
            )

    md_fails, md_count = check_model_divergence(src_root)
    fails += md_fails

    refusal_files = []
    for crate in REFUSAL_SCAN_CRATES:
        for f in sorted((src_root / crate / "src").rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            # Production folds only: test dirs and in-file test mods
            # assert specific codes lawfully (the adjudication law
            # governs production classification sites). In-file
            # cfg(test) items are pruned ATTRIBUTE-POSITION inside
            # scan_refusal_folds (rust_strip.strip_cfg_test) — the
            # old truncate-at-first-marker walk left everything after
            # an early test module unswept (merged_bug_009).
            if "/tests/" in rel or rel.endswith("test_helpers.rs"):
                continue
            refusal_files.append((rel, f.read_text()))
    fails += scan_refusal_folds(refusal_files)
    fails += scan_wire_secs_seams(refusal_files)
    # The method-call leniency's machine-bound trigger (WO-S8-3): no
    # `optional … *_seconds` proto field may exist while the grammar
    # excludes the prost-getter read form.
    fails += scan_proto_optional_seconds(src_root)
    # The retention-registry numeric-claim arm (WO-S8-7).
    fails += scan_retention_notes(src_root)
    # The retired-knob-phrase belt (WO-S8-9, merged_bug_055).
    fails += scan_retired_knob_phrases(src_root)

    gaps = sorted(f"{name}:{ax}" for name, _, _, _, g, _ in REGISTRY for ax in g)
    derived = sum(1 for *_, d in REGISTRY if d is not None)
    print(
        f"census-corpora: {len(REGISTRY)} generators enrolled ({derived} with derived_from "
        f"production anchors; every zero-gap row derived), axes {sorted(AXES)} all exercised, "
        f"{len(gaps)} grandfathered axis gaps (burn-down: {', '.join(gaps)}), "
        f"{md_count} MODEL-DIVERGENCE headers grammar-checked, "
        f"{len(refusal_files)} files swept by the negative refusal census and the wire-secs pacing-seam census"
    )
    if fails:
        print("FAIL: census-corpora violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
