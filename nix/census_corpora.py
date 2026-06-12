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
    ("quantifier-lexicon", "nix/quantifier_lexicon.py", r"self-?test", {"tier", "scope"}, set(), r"LEXICON = \(|CLAIM_TIERS = \("),
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
    # bw11 S8 (WO-S8-11(i)): the R29 duration census — population
    # grammar GENERATED from the duration-idiom product (five cells,
    # one finder-vector plant each; enrolled seed rows are mandatory
    # finder vectors), rows carry consumer clock + conversion witness
    # from the closed R29 alphabet; un-rowed constants grandfathered
    # shrink-only at nix/duration-census-grandfather.txt.
    ("duration-census", "nix/census_corpora.py", r"DURATION_IDIOM_CELLS", {"scope", "alias"}, set(), r"DURATION_IDIOM_CELLS = \["),
    # bw11 S8 (WO-S8-11(ii)): the R30 exit-edge census — population =
    # enrolled R14 seed rows UNION the latch-idiom grep grammar
    # (give-up predicates through the qualification product,
    # retain-with-latch, ON CONFLICT enqueues fail-closed on
    # unresolvable tables, GIVE_UP/MAX_ATTEMPTS/BUDGET const
    # families); un-rowed hits grandfathered shrink-only at
    # nix/exit-edge-grandfather.txt.
    ("exit-edge-census", "nix/census_corpora.py", r"EXIT_EDGE_GIVEUP", {"scope", "fold-site"}, set(), r"EXIT_EDGE_GIVEUP = re\.compile"),
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


# --- the R29 duration census (WO-S8-11(i)) -----------------------------
#
# Every quantitative envelope (TTL, margin, watermark, pin, backoff
# bound, retention floor) is DENOMINATED IN THE CONSUMER'S EXECUTION
# DOMAIN — fold executions not wall ticks, durable-progress clocks
# not occupancy clocks, committed-stamp age not producer cadence,
# paced beats not pass counts, commit order not value-timestamp order
# — or carries an explicit CONVERSION WITNESS (R29). This census
# makes the duty standing: every duration/envelope CONSTANT the
# finder locates must carry a row naming its consumer clock (from
# the closed R29 alphabet) and its conversion witness where
# producer != consumer domain; un-named rows are census-red,
# grandfathered at mint (nix/duration-census-grandfather.txt,
# shrink-only — the standing debt visible, never silent).
#
# The FINDER's population grammar is itself R22″-derived, never a
# needle list (the bug_091 shape would otherwise re-mint at this
# census's birth): the idiom-product table below over the
# workspace's duration-constant idioms — Duration-ctor consts,
# bare-integer *_SECS/*_TICKS/*_MILLIS consts, f64 *_SECS consts
# (SQL-bind AND arithmetic), serde-duration config fields, and
# Backoff struct-literal envelope consts (rio-common/backoff.rs's
# documented convention) — one finder-vector plant per idiom cell,
# and the ENROLLED SEED ROWS double as MANDATORY FINDER VECTORS:
# the finder must locate every seed by grammar, not special-case.
DURATION_IDIOM_CELLS = [
    ("duration-ctor", re.compile(r"\bconst\s+(\w+)\s*:\s*(?:std::time::)?Duration\b")),
    (
        "int-units",
        # FOLDS/PASSES joined at the wave close: S4 renamed
        # TOMBSTONE_TTL_TICKS -> _FOLDS (the consumer clock made
        # nominal) and S7 pinned the withhold interval in _PASSES
        # with a beat conversion witness — the suffix alphabet
        # follows the landed R29 denomination convention, never a
        # frozen needle list.
        re.compile(
            r"\bconst\s+(\w+_(?:SECS|TICKS|MILLIS|FOLDS|PASSES))\s*:\s*(?:u64|u32|usize|i64|u16|u8)\b"
        ),
    ),
    ("f64-secs", re.compile(r"\bconst\s+(\w+_SECS)\s*:\s*f64\b")),
    (
        "config-field",
        re.compile(r'#\[serde\([^)\]]*duration[^)\]]*\)\]\s*(?:pub(?:\([^)]*\))?\s+)?(\w+)\s*:'),
    ),
    ("backoff-struct", re.compile(r"\bconst\s+(\w+)\s*:\s*Backoff\b")),
]

# The R29 consumer-clock alphabet (closed; a row's clock must START
# with one of these tokens).
R29_CLOCKS = (
    "fold-executions",
    "durable-progress",
    "committed-stamp",
    "beats",
    "commit-order",
    "wall",
)

# Enrolled rows — (file, name) -> (consumer clock, conversion
# witness; "same-domain" when producer == consumer). Seeds at the
# t0 tree; the slot WOs landing this wave enroll their new constants
# at the wave-close tree (H6'''/H1''' name the resolved sets).
DURATION_CENSUS_ROWS = {
    ("rio-migrations/src/sql.rs", "SESSION_STALE_AFTER_SECS"): (
        "wall (PG now() at the make_interval bind)",
        "WO-S1-6: the certified margin term STALE >= 2*INTERVAL + RPC_BOUND + SLACK",
    ),
    ("rio-scheduler/src/sla/cost.rs", "STALE_CLAMP_AFTER_SECS"): (
        "wall-age against DB epoch stamps (the Epoch family's staleness clamp)",
        "WO-S6-2: the family decode-boundary seal",
    ),
    ("rio-controller/src/reconcilers/nodeclaim_pool/health.rs", "TOMBSTONE_TTL_FOLDS"): (
        "fold-executions (the WO-S4-2 close renamed the unit INTO the consumer clock)",
        "WO-S4-2: the fold-clock conversion made nominal at the landed rename",
    ),
    ("rio-store/src/logs/sessions.rs", "SESSION_MARGIN_SLACK"): (
        "wall (the session-staleness margin term over PG now() age)",
        "WO-S1-6 (the H1''' const family): compile-certified STALE >= 2*INTERVAL + RPC_BOUND + SLACK, SLACK > 0",
    ),
    ("rio-store/src/config.rs", "SCHEDULER_DEADLINE_CAP_SECS"): (
        "wall (the retention-floor validation against the scheduler deadline cap)",
        "WO-S1-7: config validation refuses retention <= the cap margin (merged_bug_071, the R29 boundary clause)",
    ),
    ("rio-store/src/materialize/client.rs", "FUTILE_RELIST_INTERVAL_PASSES"): (
        "beats (paced beats via the conversion witness; pass-denominated const)",
        "WO-S7-8 (H7'''): the (P-32)*0.8 >= 65 derivation + the W11-BN compile-tier pin — passes to worst-case beat time",
    ),
    ("rio-auth/src/hmac.rs", "MAX_HMAC_LIFETIME_SECS"): (
        "wall (unix-epoch seconds at the signer's own sample)",
        "WO-S8-6: the post-call re-sample law pin",
    ),
}
DURATION_GRANDFATHER = "nix/duration-census-grandfather.txt"


def duration_finder(files):
    """(rel, name, cell) for every duration-idiom constant/field in
    `files` (iterable of (rel, raw)); cfg(test) pruned, comments
    blanked, STRINGS KEPT (the serde attr cell needs them)."""
    out = []
    for rel, raw in files:
        try:
            pruned = rust_strip.strip_cfg_test(raw, source=rel)
        except rust_strip.StripError as e:
            out.append((rel, f"<refused: {e}>", "refusal"))
            continue
        text, _ = rust_strip.lex(pruned, blank_string_bodies=False)
        for cell, rx in DURATION_IDIOM_CELLS:
            for m in rx.finditer(text):
                out.append((rel, m.group(1), cell))
    return out


def check_duration_census(src_root, mint=False):
    files = []
    for crate_src in sorted(src_root.glob("rio-*/src")):
        for f in sorted(crate_src.rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            if "/tests/" in rel or rel.endswith("test_helpers.rs"):
                continue
            files.append((rel, f.read_text()))
    found = duration_finder(files)
    fails = []
    refusals = [(r, n) for r, n, c in found if c == "refusal"]
    for r, n in refusals:
        fails.append(f"{r}: duration finder refused: {n}")
    found_keys = {(r, n) for r, n, c in found if c != "refusal"}
    # Row validation: clock from the closed alphabet; row resolves in
    # the live population (rot otherwise); seeds are MANDATORY finder
    # vectors — a seed the grammar cannot locate is a finder red.
    for (rel, name), (clock, witness) in sorted(DURATION_CENSUS_ROWS.items()):
        if not any(clock.startswith(c) for c in R29_CLOCKS):
            fails.append(
                f"duration census row {rel}:{name}: clock `{clock}` outside the "
                f"closed R29 alphabet {R29_CLOCKS}"
            )
        if (rel, name) not in found_keys:
            fails.append(
                f"duration census row {rel}:{name}: NOT located by the finder "
                f"grammar — seed rows are mandatory finder vectors (rot or a "
                f"finder hole; never special-case the seed)"
            )
        if not witness.strip():
            fails.append(f"duration census row {rel}:{name}: empty conversion witness")
    gf_path = src_root / DURATION_GRANDFATHER
    unrowed = sorted(
        f"{rel}\t{name}" for (rel, name) in found_keys if (rel, name) not in DURATION_CENSUS_ROWS
    )
    if mint:
        gf_path.write_text("".join(k + "\n" for k in unrowed))
        return [f"minted {len(unrowed)} duration-census grandfather entries"]
    grandfathered = set()
    if gf_path.is_file():
        grandfathered = {x for x in gf_path.read_text().splitlines() if x.strip()}
    for k in unrowed:
        if k not in grandfathered:
            rel, name = k.split("\t")
            fails.append(
                f"{rel}: duration constant `{name}` has no census row — name its "
                f"consumer clock ({'/'.join(R29_CLOCKS)}) and conversion witness "
                f"in DURATION_CENSUS_ROWS (R29)"
            )
    for stale in sorted(grandfathered - set(unrowed)):
        fails.append(
            f"{stale.split(chr(9))[0]}: stale duration-census grandfather entry "
            f"({stale.split(chr(9))[1]} was enrolled, renamed, or removed) — "
            f"remove it from {DURATION_GRANDFATHER} (shrink-only)"
        )
    return fails


# --- the R30 exit-edge census (WO-S8-11(ii)) ----------------------------
#
# Every absorbing or latched state ships its EXIT EDGE in the same
# commit that ships the latch (R30): the close names the reset event
# AND proves it REACHABLE from inside the latched state under that
# state's own invariants. This census is the standing enforcement:
# every latch/budget/cap/refusal row names its exit edge and its
# reachability witness.
#
# DETECTION PREDICATE (CE-3 — no self-certification; the census-red
# claim carries the predicate that finds new latches): population =
# the R14 typed-construction seed rows (enrolled at the wave-close
# tree as the slots land them: GaveUpReset, the expiring
# HoldClearance, the outbox reset edge, the per-plane refusal, the
# poison terminal) UNION the [GEN-SET] grep grammar below over latch
# idioms verified at 4ba130cf5:
#   give-up-pred    — `attempts/deaths >= <CONST>` predicates
#                     (candidate.rs blocks_respawn-class), const
#                     captured through the qualification product
#                     (`Self::`/module-qualified — the bug_091
#                     lesson applied at birth);
#   retain-latch    — `.retain(…)`/`.retain_rows(…)` whose closure
#                     consults a latch predicate (blocks_/gave_up —
#                     candidate.rs:946-class);
#   on-conflict-*   — `INSERT … ON CONFLICT DO NOTHING` / guarded
#                     `DO UPDATE` enqueues in SQL strings (the
#                     bug_111 swallow shape; gc/mod.rs-class), the
#                     target table the identity — an INSERT the
#                     classifier cannot resolve a table from REFUSES
#                     (fail-closed), never skips;
#   const-family    — GIVE_UP/MAX_ATTEMPTS/BUDGET const families.
# Pre-existing hits are grandfathered at mint (nix/exit-edge-
# grandfather.txt, SHRINK-ONLY — the standing debt visible; bug_151's
# gave-up latch IS the founding grandfather entry until S7's
# GaveUpReset row retires it at the wave close).
EXIT_EDGE_GIVEUP = re.compile(
    r"\b(?:deaths|attempts|failures|fails|strikes)\w*\s*(?:>=|>)\s*"
    r"(?:\w+::)*([A-Z][A-Z0-9_]{2,})\b"
)
EXIT_EDGE_CONSTFAM = re.compile(r"\bconst\s+(\w*(?:GIVE_UP|MAX_ATTEMPTS|_BUDGET)\w*)\s*:")
EXIT_EDGE_RETAIN = re.compile(r"\.retain(?:_rows)?\s*\(")
EXIT_EDGE_LATCHWORD = re.compile(r"blocks_|gave_up|give_up")
EXIT_EDGE_GRANDFATHER = "nix/exit-edge-grandfather.txt"

# Enrolled rows — (file, idiom, identity) -> (reset event,
# reachability witness). Seeds land with their slots this wave; the
# integrator enrolls them at the wave-close re-mint from the H-pack
# records (H3'''/H5'''/H7''' name the landed shapes).
EXIT_EDGE_ROWS = {
    # The gave-up decay (WO-S7-1, the H7''' record): GaveUpReset
    # receipt minted only by PoolStreaks::note_demand_epoch; exit
    # edge = strictly-newer SpawnIntent.resubmit_cycle at the
    # spawn-fold demand seam (evaluate_spawn_gate;
    # SpawnGateOutcome.decayed; Event RespawnGiveUpReset); re-latch
    # at full budget.
    ("rio-controller/src/reconcilers/pool/candidate.rs", "retain-latch", "L1020"): (
        "demand-epoch decay: a strictly-newer resubmit_cycle mints GaveUpReset (pod-free — reachable from inside the latch)",
        "W11-BE red + quint-respawn-giveup single-leaf + the decay-reachable witness + the relatch run (S7 c1)",
    ),
    # The outbox reset edge (WO-S5-3, the H5''' record): the
    # guarded DO UPDATE resets attempts/enqueued_at on re-decision
    # — the exhausted row exits its absorbing state.
    ("rio-store/src/gc/mod.rs", "on-conflict-do-update", "pending_s3_deletes"): (
        "re-decision resets the budget: the guarded ON CONFLICT DO UPDATE rewrites attempts/enqueued_at",
        "W11-AP incl. the WS-2 latch-face cell (duplicate-enqueue swallow red under unguarded DO UPDATE; S5 c3)",
    ),
}

# The R14 typed-construction seeds OUTSIDE the grep grammar (CE-3:
# population = seed list UNION grep): each anchors to its landed
# construction symbol — a dead anchor is rot (census red), the
# REGISTRY derived_from discipline one row down.
EXIT_EDGE_SEED_CONSTRUCTIONS = {
    "per-plane-refusal (WO-S7-2)": (
        "rio-scheduler/src/actor/command.rs",
        r"PlanesRefused",
        "per-plane apply: refused planes redeliver independently; idempotency discharged per plane (epoch gate / upsert / wholesale rebuild / age-keyed witnesses)",
        "W11-BF controller+scheduler reds + the apply-refuse-redeliver-apply x2 state-equality cell (PD-5)",
    ),
    "expiring-hold-clearance (WO-S5-1)": (
        "rio-store/src/gc/lane.rs",
        r"HoldClearance::authorize_batch",
        "clearance expiry + per-batch re-authorization: a drain-bound-aged clearance refuses with NO hold transition",
        "W11-AM incl. the WS-3 expiry face (S5 c1)",
    ),
    "poison-terminal (WO-S3-1)": (
        "rio-scheduler/src/actor/snapshot.rs",
        r"NoHostPoison",
        "the dead band reaches the DESIGNED bounded poison terminal instead of looping advisory-forever",
        "S3 gate-superset contract rows + the poison-terminal face (merged_bug_016, the R30 face)",
    ),
    "zero-core-heal (WO-S6-1)": (
        "rio-scheduler/src/sla/metrics.rs",
        r"zero_resource",
        "the zero-resource observation REFUSES at the merge seam (typed reason) instead of jamming keep-first-forever",
        "S6 merge-law reds + the refusal-counter registration ((lllll) verified, both HELP surfaces)",
    ),
}


def exit_edge_finder(files):
    """(rel, idiom, identity) hits over (rel, raw) files; fail-closed
    per R22″ — an ON CONFLICT insert without a resolvable table is a
    refusal row, never a skip."""
    out = []
    for rel, raw in files:
        try:
            pruned = rust_strip.strip_cfg_test(raw, source=rel)
        except rust_strip.StripError as e:
            out.append((rel, "refusal", str(e)))
            continue
        lexed, spans, _ = rust_strip.lex_full(pruned, blank_string_bodies=True)
        for m in EXIT_EDGE_GIVEUP.finditer(lexed):
            out.append((rel, "give-up-pred", m.group(1)))
        for m in EXIT_EDGE_CONSTFAM.finditer(lexed):
            out.append((rel, "const-family", m.group(1)))
        for m in EXIT_EDGE_RETAIN.finditer(lexed):
            ext = rust_strip._match_delim(lexed, m.end() - 1)
            if EXIT_EDGE_LATCHWORD.search(lexed[m.end() : ext]):
                out.append(
                    (rel, "retain-latch", f"L{lexed.count(chr(10), 0, m.start()) + 1}")
                )
        for a, b, _is_raw in spans:
            body = pruned[a:b]
            if "ON CONFLICT" not in body:
                continue
            tm = re.search(r"\bINSERT\s+INTO\s+(\w+)", body, re.I)
            if tm is not None:
                kind = "on-conflict-do-update" if "DO UPDATE" in body else "on-conflict-do-nothing"
                out.append((rel, kind, tm.group(1)))
                continue
            # ON CONFLICT without INSERT INTO in the same literal:
            # a SQL FRAGMENT (split query-builder string — the table
            # is elsewhere; unclassifiable, REFUSE) vs PROSE (metric
            # HELP narrating the dedup — no SQL keywords; skip, with
            # its co-located plant). The boundary is derived from the
            # string's own grammar, not the file.
            if re.search(r"\b(?:VALUES|SELECT|SET|WHERE|UNNEST|UPDATE)\b", body):
                out.append(
                    (
                        rel,
                        "refusal",
                        f"{rel}:{pruned.count(chr(10), 0, a) + 1}: ON CONFLICT "
                        f"SQL fragment without a resolvable INSERT INTO target "
                        f"— refusing (fail-closed; split builder strings hide "
                        f"the enqueue identity)",
                    )
                )
    return out


def check_exit_edge_census(src_root, mint=False):
    files = []
    for crate in REFUSAL_SCAN_CRATES:
        for f in sorted((src_root / crate / "src").rglob("*.rs")):
            rel = str(f.relative_to(src_root))
            if "/tests/" in rel or rel.endswith("test_helpers.rs"):
                continue
            files.append((rel, f.read_text()))
    found = exit_edge_finder(files)
    fails = [ident for rel, idiom, ident in found if idiom == "refusal"]
    found_keys = {(r, i, n) for r, i, n in found if i != "refusal"}
    for (rel, idiom, name), (reset, witness) in sorted(EXIT_EDGE_ROWS.items()):
        if (rel, idiom, name) not in found_keys:
            fails.append(
                f"exit-edge row {rel}:{idiom}:{name}: NOT located by the finder "
                f"grammar — rot or a finder hole (rows are mandatory vectors)"
            )
        if not reset.strip() or not witness.strip():
            fails.append(f"exit-edge row {rel}:{idiom}:{name}: empty reset event or witness")
    for seed, (rel, anchor, reset, witness) in sorted(EXIT_EDGE_SEED_CONSTRUCTIONS.items()):
        sf = src_root / rel
        stext = sf.read_text() if sf.is_file() else ""
        if not re.search(anchor, stext):
            fails.append(
                f"exit-edge seed `{seed}`: anchor /{anchor}/ does not resolve in "
                f"{rel} — the typed construction moved or rotted (re-derive the row)"
            )
        if not reset.strip() or not witness.strip():
            fails.append(f"exit-edge seed `{seed}`: empty reset event or witness")
    gf_path = src_root / EXIT_EDGE_GRANDFATHER
    unrowed = sorted(
        f"{r}\t{i}\t{n}" for (r, i, n) in found_keys if (r, i, n) not in EXIT_EDGE_ROWS
    )
    if mint:
        gf_path.write_text("".join(k + "\n" for k in unrowed))
        return [f"minted {len(unrowed)} exit-edge grandfather entries"]
    grandfathered = set()
    if gf_path.is_file():
        grandfathered = {x for x in gf_path.read_text().splitlines() if x.strip()}
    for k in unrowed:
        if k not in grandfathered:
            r, i, n = k.split("\t")
            fails.append(
                f"{r}: latch idiom `{i}:{n}` has no exit-edge row — name its reset "
                f"event and reachability witness in EXIT_EDGE_ROWS (R30: every "
                f"latch ships its exit edge same-commit)"
            )
    for stale in sorted(grandfathered - set(unrowed)):
        fails.append(
            f"{stale.split(chr(9))[0]}: stale exit-edge grandfather entry "
            f"({stale!r}) — remove it from {EXIT_EDGE_GRANDFATHER} (shrink-only)"
        )
    return fails


# --- the R22″ self-coverage gate (WO-S8-11(ii), rides the same
# commit) ----------------------------------------------------------------
#
# (a) scanner→corpus mapping: every census ARM of this file registers
#     its own REGISTRY row — an unregistered census is itself a gate
#     red (the existing reverse-direction check covers generator-
#     shaped FILES; this covers the in-file arms);
# (b) residual pricing is SCHEMA-TYPED FIRST: a census row that
#     prices a residual against a compensating control declares it in
#     REGISTRY_RESIDUALS with the control's firing-predicate anchor,
#     and the anchor must resolve (the machine-bind — bug_132's
#     channel killed at registered-census strength: the mapping in
#     (a) is what makes "registered" total);
# (c) the prose backstop (DEMOTED tier — word-shape detection alone
#     is the R23′ failure mode): pricing phrases in the registry
#     block's comments adjacent to a gaps-bearing row without a
#     REGISTRY_RESIDUALS entry are reds;
# (d) zero undispositioned priced-gap rows: no row may carry BOTH a
#     non-empty gaps set AND pricing prose (the CE-7 disposition —
#     the wire-secs fold-site row retired its gap WITH the bind).
SELF_COVERAGE_ARMS = {
    "wire-secs-pacing-seams",
    "duration-census",
    "exit-edge-census",
}
REGISTRY_RESIDUALS = {
    # (census, residual) -> firing-predicate anchor (regex over the
    # census's anchor file; the compensating control's own test).
    ("wire-secs-pacing-seams", "fold-site"): r"WIRE_SECS_BACKSTOP_VECTORS",
}
PRICING_PROSE = re.compile(r"\bcovered by\b|\bcompensat\w+\b|\bsuffices\b|\bprice[sd]?\b", re.I)


def check_self_coverage(src_root, arms=None, residuals=None):
    arms = SELF_COVERAGE_ARMS if arms is None else arms
    residuals = REGISTRY_RESIDUALS if residuals is None else residuals
    fails = []
    names = {row[0] for row in REGISTRY}
    missing = arms - names
    if missing:
        fails.append(
            f"self-coverage: census arm(s) {sorted(missing)} have no REGISTRY row — "
            f"an unregistered census is a gate red (the scanner→corpus mapping)"
        )
    self_text = (src_root / "nix" / "census_corpora.py").read_text()
    for (census, residual), anchor in sorted(residuals.items()):
        row = next((r for r in REGISTRY if r[0] == census), None)
        if row is None:
            fails.append(f"self-coverage: residual entry for unregistered census {census}")
            continue
        anchor_file = src_root / row[1]
        text = anchor_file.read_text() if anchor_file.is_file() else ""
        if not re.search(anchor, text):
            fails.append(
                f"self-coverage: {census}:{residual} residual bind anchor /{anchor}/ "
                f"does not resolve in {row[1]} — the compensating control's firing "
                f"predicate rotted; the residual is UNBOUND (census red)"
            )
    # (c)+(d): walk the REGISTRY source block; for each row line,
    # inspect the comment block above it.
    lines = self_text.splitlines()
    for idx, line in enumerate(lines):
        m = re.match(r'\s*\("([\w-]+)",\s*"[^"]+",.*\{([^}]*)\}\s*,', line)
        if not m:
            continue
        census = m.group(1)
        row = next((r for r in REGISTRY if r[0] == census), None)
        if row is None or not row[4]:
            continue  # no gaps — nothing to disposition
        comment = []
        j = idx - 1
        while j >= 0 and lines[j].lstrip().startswith("#"):
            comment.append(lines[j])
            j -= 1
        blob = "\n".join(reversed(comment))
        if PRICING_PROSE.search(blob):
            for ax in row[4]:
                if (census, ax) not in residuals:
                    fails.append(
                        f"self-coverage: registry row `{census}` carries gaps "
                        f"{sorted(row[4])} WITH pricing prose but no "
                        f"REGISTRY_RESIDUALS bind for `{ax}` — fix the gap or "
                        f"machine-bind the compensating control (R22″: prose "
                        f"pricing is the bug_132 channel)"
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
    args = [a for a in sys.argv[1:]]
    mint_duration = "--mint-duration-grandfather" in args
    mint_exit_edge = "--mint-exit-edge-grandfather" in args
    args = [a for a in args if not a.startswith("--mint-")]
    src_root = pathlib.Path(args[0])

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
    # --- the R29 duration census plants (WO-S8-11(i)) -----------------
    # One finder-vector per idiom cell (the leniency plants: every
    # population-grammar cell demonstrably catches its form).
    cell_vectors = {
        "duration-ctor": "pub const POLL: Duration = Duration::from_secs(5);\n",
        "int-units": "const RETRY_TTL_TICKS: u32 = 7;\n",
        "f64-secs": "pub(crate) const DRAIN_SECS: f64 = 1.5;\n",
        "config-field": '#[serde(with = "duration_secs")]\n    pub poll_interval: Duration,\n',
        "backoff-struct": "const PUSH_BACKOFF: Backoff = Backoff { base_ms: 50, cap_ms: 1000 };\n",
    }
    for cell, _rx in DURATION_IDIOM_CELLS:
        got = duration_finder([("planted/cell.rs", cell_vectors[cell])])
        if len(got) != 1 or got[0][2] != cell:
            print(
                f"FAIL: duration-census finder vector for cell `{cell}` not located: {got}",
                file=sys.stderr,
            )
            return 1
    # … a cfg(test)-gated constant stays OUT of the census population.
    gated = "#[cfg(test)]\nconst FAKE_TTL_SECS: u64 = 1;\nfn live() {}\n"
    if duration_finder([("planted/gated.rs", gated)]):
        print("FAIL: a cfg(test) duration const entered the census population", file=sys.stderr)
        return 1
    # The strawman violating row: an unrowed, ungrandfathered constant
    # is a named census red (the new-census plant).
    with tempfile.TemporaryDirectory() as td:
        straw_root = pathlib.Path(td)
        (straw_root / "rio-straw" / "src").mkdir(parents=True)
        (straw_root / "rio-straw" / "src" / "lib.rs").write_text(
            "pub const ORPHAN_WINDOW_SECS: u64 = 30;\n"
        )
        f_d = check_duration_census(straw_root)
        named = [x for x in f_d if "ORPHAN_WINDOW_SECS" in x and "no census row" in x]
        if len(named) != 1:
            print(f"FAIL: the unrowed duration const did not red: {f_d}", file=sys.stderr)
            return 1
    # --- the R30 exit-edge census plants (WO-S8-11(ii)) ----------------
    # One finder-vector per latch-idiom cell, incl. the qualification
    # product on the give-up predicate (the bug_091 lesson at birth).
    ee_vectors = [
        ("give-up-pred", "fn f(&self) -> bool { self.deaths >= RESPAWN_GIVE_UP_DEATHS }\n", "RESPAWN_GIVE_UP_DEATHS"),
        ("give-up-pred", "fn f(&self) -> bool { self.attempts > Self::ZERO_BACKOFF_STREAK }\n", "ZERO_BACKOFF_STREAK"),
        ("retain-latch", "fn f(&mut self) { self.rows.retain(|r| r.blocks_respawn(now)); }\n", "L1"),
        ("on-conflict-do-nothing", 'const Q: &str = "INSERT INTO outbox (id) VALUES ($1) ON CONFLICT DO NOTHING";\n', "outbox"),
        ("on-conflict-do-update", 'const Q: &str = "INSERT INTO state (k) VALUES ($1) ON CONFLICT (k) DO UPDATE SET k = $1";\n', "state"),
        ("const-family", "const REPORT_RETRY_BUDGET: u32 = 3;\n", "REPORT_RETRY_BUDGET"),
    ]
    for idiom, snippet, ident in ee_vectors:
        got = [h for h in exit_edge_finder([("planted/ee.rs", snippet)]) if h[1] != "refusal"]
        if len(got) != 1 or got[0][1] != idiom or got[0][2] != ident:
            print(f"FAIL: exit-edge finder vector ({idiom}/{ident}) not located: {got}", file=sys.stderr)
            return 1
    # The fail-closed refusal plant: an ON CONFLICT SQL fragment whose
    # INSERT INTO target lives in another string REFUSES, never skips.
    refused_ee = exit_edge_finder(
        [("planted/frag.rs", 'const TAIL: &str = "ON CONFLICT (k) DO UPDATE SET v = $1";\n')]
    )
    if not any(h[1] == "refusal" and "INSERT INTO target" in h[2] for h in refused_ee):
        print(f"FAIL: the split ON CONFLICT fragment did not refuse: {refused_ee}", file=sys.stderr)
        return 1
    # … the format!-dynamic table refuses through the same arm.
    refused_dyn = exit_edge_finder(
        [("planted/dyn.rs", 'fn q(t: &str) -> String { format!("ON CONFLICT DO NOTHING WHERE {t}") }\n')]
    )
    if not any(h[1] == "refusal" for h in refused_dyn):
        print(f"FAIL: the dynamic ON CONFLICT did not refuse: {refused_dyn}", file=sys.stderr)
        return 1
    # … and PROSE narrating ON CONFLICT (metric HELP — the
    # admin/mod.rs:166 class) is NOT SQL: the leniency plant.
    prose_ee = exit_edge_finder(
        [("planted/prose.rs", 'const H: &str = "inserts absorbed by the M_047 ON CONFLICT dedup, by kind";\n')]
    )
    if prose_ee:
        print(f"FAIL: HELP prose mentioning ON CONFLICT classified as an enqueue: {prose_ee}", file=sys.stderr)
        return 1
    # The strawman violating row: an unrowed latch is a named red.
    with tempfile.TemporaryDirectory() as td:
        straw_root = pathlib.Path(td)
        (straw_root / "rio-store" / "src").mkdir(parents=True)
        (straw_root / "rio-store" / "src" / "lib.rs").write_text(
            "fn f(&self) -> bool { self.attempts >= ROGUE_GIVE_UP_MAX }\n"
        )
        f_e = check_exit_edge_census(straw_root)
        if len([x for x in f_e if "ROGUE_GIVE_UP_MAX" in x and "no exit-edge row" in x]) != 1:
            print(f"FAIL: the unrowed latch did not red: {f_e}", file=sys.stderr)
            return 1
    # --- the R22'' self-coverage gate plants ---------------------------
    # (a) an unregistered census arm is a gate red;
    f_sc = check_self_coverage(src_root, arms=SELF_COVERAGE_ARMS | {"ghost-census"})
    if not any("ghost-census" in x and "no REGISTRY row" in x for x in f_sc):
        print(f"FAIL: the unregistered-census plant did not red: {f_sc}", file=sys.stderr)
        return 1
    # (b) a residual whose firing-predicate anchor does not resolve is
    # an UNBOUND residual — census red (the bug_132 channel).
    f_sc = check_self_coverage(
        src_root,
        residuals={("wire-secs-pacing-seams", "fold-site"): r"NO_SUCH_" + r"FIRING_PREDICATE"},
    )
    if not any("does not resolve" in x for x in f_sc):
        print(f"FAIL: the dead residual-bind anchor did not red: {f_sc}", file=sys.stderr)
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
    # The R29 duration census (WO-S8-11(i)).
    if mint_duration:
        for line in check_duration_census(src_root, mint=True):
            print(line)
        return 0
    if mint_exit_edge:
        for line in check_exit_edge_census(src_root, mint=True):
            print(line)
        return 0
    fails += check_duration_census(src_root)
    # The R30 exit-edge census + the R22'' self-coverage gate
    # (WO-S8-11(ii)).
    fails += check_exit_edge_census(src_root)
    fails += check_self_coverage(src_root)

    gaps = sorted(f"{name}:{ax}" for name, _, _, _, g, _ in REGISTRY for ax in g)
    derived = sum(1 for *_, d in REGISTRY if d is not None)
    print(
        f"census-corpora: {len(REGISTRY)} generators enrolled ({derived} with derived_from "
        f"production anchors; every zero-gap row derived), axes {sorted(AXES)} all exercised, "
        f"{len(gaps)} grandfathered axis gaps (burn-down: {', '.join(gaps)}), "
        f"{md_count} MODEL-DIVERGENCE headers grammar-checked, "
        f"{len(refusal_files)} files swept by the negative refusal census and the wire-secs pacing-seam census, "
        f"duration census {len(DURATION_CENSUS_ROWS)} rows enrolled, "
        f"exit-edge census {len(EXIT_EDGE_ROWS)} rows enrolled, "
        f"self-coverage gate over {len(SELF_COVERAGE_ARMS)} arms + {len(REGISTRY_RESIDUALS)} bound residual(s)"
    )
    if fails:
        print("FAIL: census-corpora violations —", file=sys.stderr)
        for x in fails:
            print(f"  {x}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
