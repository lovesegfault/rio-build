# Emit GitHub Actions matrix outputs for ci.yml from a single
# nix-eval-jobs pass over `.#githubActions`, eliding entries already
# in the binary cache and grouping the survivors into clusters that
# share a runner.
#
# Replaces .github/scripts/gen-matrix.sh (bash+jq). The clustering
# and warm-stage logic pushed that script past what jq-in-bash can
# carry maintainably.
#
# Eval-once / realise-everywhere: this is the only job in the
# pipeline that evaluates the flake. Every downstream job receives
# DERIVATION PATHS and runs `nix build /nix/store/...drv^*`,
# substituting the .drv closure from a run-scoped workflow artifact
# this script exports (a local file:// binary cache of the build
# graph). No flake checkout-eval in the build jobs.
#
# Outputs written to $GITHUB_OUTPUT (one `key=json` line each):
#   warm            {"checks": "/nix/store/a.drv /nix/store/b.drv",
#                    "fuzz": "", ...}
#                   Per kind, the drvs needed by >=2 of that kind's
#                   clusters -- the shared trunk the warm-<kind> job
#                   realises before the kind's matrix fans out. Empty
#                   string -> nothing shared -> warm job skipped.
#   checks          [{"name": "rio-store",
#                     "targets": "clippy-rio-store nextest-rio-store",
#                     "drvs": "/nix/store/x.drv /nix/store/y.drv"}]
#                   Clusters whose members need something from the
#                   warm set -- they gate on warm-checks.
#   checks-nowait   Same shape; clusters with NO overlap with the
#                   warm set. They start as soon as gen-matrix
#                   finishes (treefmt feedback does not wait for the
#                   rust trunk). Only `checks` gets this split: every
#                   fuzz/vm/coverage entry always needs its kind's
#                   trunk, so a nowait partition there would always
#                   be empty.
#   fuzz            singleton clusters, same object shape
#   vm-test         singleton clusters, same object shape
#   coverage        one "unit" cluster + vm-* singletons
#   coverage-paths  {"unit-rio-store": "/nix/store/...", ...}
#
# Matrix entries are OBJECTS: `targets` (attr names, for display and
# failure attribution) and `drvs` (parallel list of drv paths, what
# the job actually builds). One `nix build --keep-going` per cluster:
# the drvs share a store and a scheduler, so their common
# dependencies build once.
#
# Cache-filter granularity stays per-derivation: a cluster only
# contains its UNCACHED members.
#
# The warm set is computed across CLUSTERS, not entries: two checks
# of the same crate share that crate's dep closure, but they run in
# the same job, so warming their shared deps would only serialize
# that cluster behind a warm job doing work the cluster could do
# itself in parallel with the other clusters.
#
# Dry-run locally:
#   GITHUB_OUTPUT=/dev/stdout nix run .#gen-matrix
# Self-test (no nix needed):
#   python3 nix/gen_matrix.py --self-test
import json
import os
import re
import subprocess
import sys
import unittest
from collections import defaultdict

# Substituted by pkgs.replaceVars in flake.nix. Running the file
# straight from the repo leaves the literal in place; only main()
# dereferences it.
NIX_EVAL_JOBS = "@nix_eval_jobs@"

# ARC pods are CFS-quota'd, not cpuset-pinned, so nproc reports the
# host core count (often 32+). Each nix-eval-jobs worker is a full
# evaluator (~500MB-1GB peak for NixOS module evals); uncapped that
# OOM-thrashes a 4-8GB pod. 8 keeps the ~50 NixOS-config evals
# saturated without blowing memory.
DEFAULT_WORKERS = 8

# Per-member check kinds produced by nix/checks.nix. Anything in the
# `checks` matrix matching `<kind>-<member>` clusters under <member>;
# everything else lands in the single `misc` cluster. Longest prefix
# first so `clippy-test-rio-store` does not match `clippy` with a
# bogus `test-rio-store` member. A misc check whose name happens to
# collide with this pattern would be harmlessly grouped under that
# crate's cluster -- it still gets built.
CHECK_KIND_RE = re.compile(r"^(clippy-test|clippy|doc|nextest)-(.+)$")

# All matrix kinds, in stable emission order.
MATRIX_KINDS = ("checks", "fuzz", "vm-test", "coverage")


def parse_results(lines):
    """Parse nix-eval-jobs JSONL into a list of dicts.

    Blank lines are skipped. Raises ValueError on malformed JSON --
    a truncated nix-eval-jobs stream must not silently drop matrix
    entries (a dropped entry means a check silently never runs and
    ci-gate still goes green).
    """
    results = []
    for line in lines:
        if not line.strip():
            continue
        try:
            results.append(json.loads(line))
        except json.JSONDecodeError as exc:
            raise ValueError(
                f"malformed nix-eval-jobs output: {line!r}"
            ) from exc
    return results


def eval_errors(results):
    """Return [(attr, error), ...] for every per-attr eval failure."""
    return [(r["attr"], r["error"]) for r in results if "error" in r]


def split_attr(attr):
    """'checks.clippy-rio-nix' -> ('checks', 'clippy-rio-nix').

    Splits on the FIRST dot only so a name containing a dot survives
    round-tripping into a flake attr path.
    """
    kind, _, name = attr.partition(".")
    return kind, name


def pending(results):
    """Uncached entries as {kind: {name: meta}}.

    meta is {"drv": <drvPath>, "needed": frozenset(<neededBuilds>)}.
    `needed` is the entry's uncached build closure (what nix would
    have to build to realise it) -- the input to the shared-trunk
    analysis. `drv` is what the matrix job realises.

    'local' and 'notBuilt' cache statuses are both kept: CI runners
    have an empty store so 'local' never appears there, and keeping
    it makes local dry-runs reflect what a never-pushed branch would
    build.
    """
    out = defaultdict(dict)
    for r in results:
        if r.get("cacheStatus") == "cached":
            continue
        kind, name = split_attr(r["attr"])
        out[kind][name] = {
            "drv": r["drvPath"],
            "needed": frozenset(r.get("neededBuilds", [])),
        }
    return dict(out)


def cluster_needed(cluster, meta):
    """Union of a cluster's members' needed-sets."""
    needed = set()
    for target in cluster["targets"].split():
        needed |= meta[target]["needed"]
    return needed


def shared_drvs(clusters, meta):
    """Drvs appearing in >=2 clusters' needed-sets, sorted.

    This is the kind's warm set: build these once in the warm job and
    every cluster that needs them substitutes instead of rebuilding.
    Drvs needed by only ONE cluster are left to that cluster's job --
    warming them would serialize the cluster behind the warm job for
    no dedup benefit. A kind with a single uncached cluster therefore
    has an empty warm set.
    """
    counts = defaultdict(int)
    for cluster in clusters:
        for drv in cluster_needed(cluster, meta):
            counts[drv] += 1
    return sorted(drv for drv, n in counts.items() if n >= 2)


def attach_drvs(clusters, meta):
    """Return clusters with a `drvs` field parallel to `targets`."""
    return [
        {
            **c,
            "drvs": " ".join(
                meta[t]["drv"] for t in c["targets"].split()
            ),
        }
        for c in clusters
    ]


def partition_gated(clusters, meta, shared):
    """Split clusters into (gated, nowait) by warm-set overlap.

    A cluster whose needed-set intersects the warm set must wait for
    the warm job to finish (otherwise it would race it on the shared
    drvs and rebuild them). A cluster with no overlap starts
    immediately -- this is what lets treefmt report a formatting
    error in ~3min while the rust trunk is still compiling.

    A MIXED cluster (some members need the trunk, some do not -- in
    practice only `misc`, where golden-* needs the conformance binary
    but treefmt needs nothing) is split in two so its fast members do
    not wait for its slow ones' dependencies.
    """
    gated, nowait = [], []
    for c in clusters:
        targets = c["targets"].split()
        hot = [t for t in targets if meta[t]["needed"] & shared]
        cold = [t for t in targets if not (meta[t]["needed"] & shared)]
        if hot:
            gated.append({**c, "targets": " ".join(hot)})
        if cold:
            nowait.append({**c, "targets": " ".join(cold)})
    return gated, nowait


def cluster_checks(names):
    """Group uncached check names into matrix entry objects.

    Per-member check kinds (CHECK_KIND_RE) cluster by member; the
    rest form a single 'misc' cluster. Returns a list of
    {"name": ..., "targets": "space separated"} sorted by name, with
    targets sorted within each cluster, so the output is stable
    across runs (stable matrix JSON -> identical re-runs reuse the
    GHA UI's job ordering).
    """
    groups = defaultdict(list)
    for name in names:
        m = CHECK_KIND_RE.match(name)
        groups[m.group(2) if m else "misc"].append(name)
    return [
        {"name": group, "targets": " ".join(sorted(members))}
        for group, members in sorted(groups.items())
    ]


def cluster_coverage(names):
    """unit-* entries form one 'unit' cluster; vm-* stay singletons.

    A unit-coverage failure is an infra signal (the real test already
    ran in checks.nextest-*), so per-crate fan-out buys nothing but
    a runner of checkout/install/eval overhead per crate. vm-*
    entries keep one-job-per-scenario for KVM runner selection and
    retry granularity.
    """
    unit = sorted(n for n in names if n.startswith("unit-"))
    rest = sorted(n for n in names if not n.startswith("unit-"))
    clusters = []
    if unit:
        clusters.append({"name": "unit", "targets": " ".join(unit)})
    clusters.extend({"name": n, "targets": n} for n in rest)
    return clusters


def singletons(names):
    """One cluster per name. Used for fuzz and vm-test, where the
    leaf work (a fixed-wall-time fuzz run, a VM boot) dominates the
    shared work and retry granularity matters."""
    return [{"name": n, "targets": n} for n in sorted(names)]


def coverage_paths(results):
    """{name: outPath} for every coverage.* entry, cached or not.

    coverage-upload substitutes each lcov from the cache and posts it
    to Codecov even when the build matrix was fully elided -- a cache
    hit must still produce a per-commit report.
    """
    paths = {}
    for r in results:
        kind, name = split_attr(r["attr"])
        if kind == "coverage" and "outputs" in r:
            paths[name] = r["outputs"]["out"]
    return paths


def build_outputs(results):
    """Assemble the full GITHUB_OUTPUT key->JSON-string map.

    Per kind: cluster the uncached entry names, attach drv paths,
    compute the cross-cluster shared set (the warm job's work list),
    and -- for checks only -- split the clusters into gated/nowait by
    whether they overlap the warm set.
    """
    pend = pending(results)
    clusterers = {
        "checks": cluster_checks,
        "fuzz": singletons,
        "vm-test": singletons,
        "coverage": cluster_coverage,
    }
    matrices = {}
    warm = {}
    for kind in MATRIX_KINDS:
        meta = pend.get(kind, {})
        clusters = clusterers[kind](sorted(meta))
        shared = shared_drvs(clusters, meta)
        warm[kind] = " ".join(shared)
        if kind == "checks":
            gated, nowait = partition_gated(clusters, meta, set(shared))
            matrices["checks"] = attach_drvs(gated, meta)
            matrices["checks-nowait"] = attach_drvs(nowait, meta)
        else:
            matrices[kind] = attach_drvs(clusters, meta)
    outputs = {"warm": json.dumps(warm, separators=(",", ":"))}
    for key, value in matrices.items():
        outputs[key] = json.dumps(value, separators=(",", ":"))
    outputs["coverage-paths"] = json.dumps(
        coverage_paths(results), separators=(",", ":")
    )
    return outputs


def push_paths(results):
    """Store paths whose closures must reach the binary cache for the
    realise-only jobs to work: every uncached entry's drvPath. The
    uploader expands each to its reference closure (input drvs +
    input sources, transitively), so downstream `nix build <drv>^*`
    can substitute the whole build graph. Cached entries' jobs never
    run, so their drvs are not needed."""
    return sorted(
        r["drvPath"]
        for r in results
        if r.get("cacheStatus") != "cached" and "drvPath" in r
    )


def write_outputs(outputs, fh):
    """Write key=value lines. Values must be single-line JSON."""
    for key, value in outputs.items():
        fh.write(f"{key}={value}\n")


def run_nix_eval_jobs(workers):
    """Stream-eval .#githubActions, echoing progress to stderr.

    Returns the parsed JSONL list. --force-recurse: the matrix kinds
    are plain attrsets with no recurseIntoAttrs marker.
    --check-cache-status: probe configured substituters per attr so
    cached entries can be elided.
    """
    cmd = [
        NIX_EVAL_JOBS,
        "--flake",
        ".#githubActions",
        "--force-recurse",
        "--check-cache-status",
        "--workers",
        str(workers),
    ]
    print(
        f"nix-eval-jobs: {workers} workers, ~2-5min cold, "
        "streaming attrs as they complete:",
        file=sys.stderr,
    )
    lines = []
    proc = subprocess.Popen(
        cmd, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL, text=True
    )
    for line in proc.stdout:
        line = line.rstrip("\n")
        if not line.strip():
            continue
        lines.append(line)
        try:
            attr = json.loads(line)
        except json.JSONDecodeError:
            attr = {}
        print(
            f"  {attr.get('attr', '?')} {attr.get('cacheStatus', 'ERROR')}",
            file=sys.stderr,
        )
    if proc.wait() != 0:
        sys.exit(f"nix-eval-jobs exited {proc.returncode}")
    return parse_results(lines)


def export_drv_closures(paths, dest):
    """Write the drv closures to a local file:// binary cache for the
    realise-only jobs to substitute from.

    The cache directory is uploaded as a run-scoped workflow artifact;
    every build job downloads it and passes it as an extra
    substituter, so `nix build /nix/store/...drv^*` finds the .drv
    and its whole input graph without a flake eval. Everything in a
    drv closure is content-addressed (.drv files are text-CA,
    eval-time sources are source-CA), so the substituted paths verify
    by content and need no signature configuration.

    This deliberately does NOT go through the niks3 binary cache: its
    uploader describes paths via `nix path-info -- <path>`, which
    reinterprets a .drv argument as its outputs and so cannot upload
    derivation files. The drv graph is also run-scoped ephemeral data
    that has no business in the permanent cache.

    No-op when dest is empty (local dry-runs).
    """
    if not paths or not dest:
        if paths:
            print(
                "::notice::DRV_CACHE_DIR unset -- skipping the drv-closure "
                "export (fine locally; realise-only CI jobs need it)"
            )
        return
    subprocess.run(
        [
            "nix",
            "copy",
            "--derivation",
            "--to",
            f"file://{dest}?compression=none",
            *paths,
        ],
        check=True,
    )
    print(
        f"exported {len(paths)} drv closures to {dest}",
        file=sys.stderr,
    )


def main():
    output_path = os.environ.get("GITHUB_OUTPUT")
    if not output_path:
        sys.exit("GITHUB_OUTPUT must be set")
    workers = int(os.environ.get("NEJ_WORKERS", DEFAULT_WORKERS))
    results = run_nix_eval_jobs(workers)

    # Fail hard on any per-attr eval error. Surfacing it here (rather
    # than letting a downstream job's nix build rediscover it) means a
    # single red gen-matrix instead of N green jobs masking a missing
    # red one -- an eval error on a filtered matrix would otherwise
    # silently drop the attr and ci-gate would pass.
    errors = eval_errors(results)
    if errors:
        print("::error::nix-eval-jobs reported eval failures:")
        for attr, error in errors:
            print(f"  {attr}: {error}", file=sys.stderr)
        sys.exit(1)

    outputs = build_outputs(results)

    # Ship the build graph BEFORE emitting outputs: once this job
    # succeeds, downstream jobs assume their drvs are substitutable
    # from the drv-cache artifact.
    export_drv_closures(push_paths(results), os.environ.get("DRV_CACHE_DIR"))

    skipped = sorted(
        r["attr"] for r in results if r.get("cacheStatus") == "cached"
    )
    print(f"::notice::cached (skipped): {json.dumps(skipped)}")
    warm = json.loads(outputs["warm"])
    for kind, drvs in warm.items():
        n = len(drvs.split()) if drvs else 0
        print(f"::notice::warm {kind}: {n} shared drvs")
    for key in ("checks", "checks-nowait", "fuzz", "vm-test", "coverage"):
        print(f"::notice::{key}: {outputs[key]}")

    with open(output_path, "a") as fh:
        write_outputs(outputs, fh)


# ----------------------------------------------------------------------
# Self-tests: python3 nix/gen_matrix.py --self-test
# ----------------------------------------------------------------------


def _entry(attr, status="notBuilt", out=None, error=None, drv=None, needed=None):
    if error is not None:
        return {"attr": attr, "error": error}
    e = {"attr": attr, "cacheStatus": status}
    if out is not None:
        e["outputs"] = {"out": out}
    e["drvPath"] = drv if drv is not None else f"/d/{attr}.drv"
    if needed is not None:
        e["neededBuilds"] = needed
    return e


class ParseTests(unittest.TestCase):
    def test_parses_jsonl_and_skips_blanks(self):
        lines = ['{"attr": "checks.a", "cacheStatus": "cached"}', "", " "]
        self.assertEqual(
            parse_results(lines),
            [{"attr": "checks.a", "cacheStatus": "cached"}],
        )

    def test_malformed_json_raises(self):
        with self.assertRaises(ValueError):
            parse_results(['{"attr": "checks.a"', ""])

    def test_eval_errors_collected(self):
        results = [_entry("checks.ok"), _entry("checks.bad", error="boom")]
        self.assertEqual(eval_errors(results), [("checks.bad", "boom")])

    def test_split_attr_first_dot_only(self):
        self.assertEqual(
            split_attr("checks.doc-rio-nix"), ("checks", "doc-rio-nix")
        )
        self.assertEqual(split_attr("warm.vm-test"), ("warm", "vm-test"))
        self.assertEqual(split_attr("coverage.a.b"), ("coverage", "a.b"))


class FilterTests(unittest.TestCase):
    def test_cached_dropped_local_and_notbuilt_kept(self):
        results = [
            _entry("checks.a", "cached"),
            _entry("checks.b", "local", needed=["/d/dep.drv"]),
            _entry("checks.c", "notBuilt"),
            _entry("fuzz.d", "notBuilt"),
        ]
        self.assertEqual(
            pending(results),
            {
                "checks": {
                    "b": {
                        "drv": "/d/checks.b.drv",
                        "needed": frozenset({"/d/dep.drv"}),
                    },
                    "c": {"drv": "/d/checks.c.drv", "needed": frozenset()},
                },
                "fuzz": {
                    "d": {"drv": "/d/fuzz.d.drv", "needed": frozenset()},
                },
            },
        )

    def test_push_paths_are_uncached_entry_drvs(self):
        results = [
            _entry("checks.a", "cached", drv="/d/a.drv"),
            _entry("checks.b", "notBuilt", drv="/d/b.drv"),
            _entry("coverage.c", "notBuilt", drv="/d/c.drv"),
        ]
        self.assertEqual(push_paths(results), ["/d/b.drv", "/d/c.drv"])


def _meta(**entries):
    """Shorthand: _meta(a=({"x"}, "/d/a.drv")) -> pending-style map."""
    return {
        name: {"drv": drv, "needed": frozenset(needed)}
        for name, (needed, drv) in entries.items()
    }


class SharedDrvsTests(unittest.TestCase):
    def test_drv_in_two_clusters_is_shared(self):
        clusters = [
            {"name": "a", "targets": "a"},
            {"name": "b", "targets": "b"},
        ]
        meta = _meta(a=({"/d/dep.drv"}, "/d/a.drv"), b=({"/d/dep.drv"}, "/d/b.drv"))
        self.assertEqual(shared_drvs(clusters, meta), ["/d/dep.drv"])

    def test_intra_cluster_sharing_is_not_warmed(self):
        # Both members of ONE cluster need dep.drv; no other cluster
        # does. The cluster's single nix invocation already builds it
        # once -- warming it would only serialize this cluster behind
        # the warm job.
        clusters = [
            {"name": "rio-store", "targets": "clippy-rio-store nextest-rio-store"},
            {"name": "rio-nix", "targets": "clippy-rio-nix"},
        ]
        meta = _meta(**{
            "clippy-rio-store": ({"/d/store-deps.drv"}, "/d/c.drv"),
            "nextest-rio-store": ({"/d/store-deps.drv"}, "/d/n.drv"),
            "clippy-rio-nix": ({"/d/nix-deps.drv"}, "/d/x.drv"),
        })
        self.assertEqual(shared_drvs(clusters, meta), [])

    def test_single_cluster_kind_has_empty_warm_set(self):
        clusters = [{"name": "a", "targets": "a"}]
        meta = _meta(a=({"/d/dep.drv"}, "/d/a.drv"))
        self.assertEqual(shared_drvs(clusters, meta), [])

    def test_entry_own_drv_can_be_shared(self):
        # vm tests embed the docker images; the image drv appears in
        # every vm entry's neededBuilds and must land in the warm set.
        clusters = [
            {"name": "vm-a", "targets": "vm-a"},
            {"name": "vm-b", "targets": "vm-b"},
            {"name": "vm-c", "targets": "vm-c"},
        ]
        meta = _meta(
            **{
                "vm-a": ({"/d/img.drv", "/d/a-only.drv"}, "/d/a.drv"),
                "vm-b": ({"/d/img.drv"}, "/d/b.drv"),
                "vm-c": (set(), "/d/c.drv"),
            }
        )
        self.assertEqual(shared_drvs(clusters, meta), ["/d/img.drv"])


class AttachPartitionTests(unittest.TestCase):
    def test_attach_drvs_parallel_to_targets(self):
        clusters = [{"name": "rio-nix", "targets": "clippy-rio-nix doc-rio-nix"}]
        meta = _meta(
            **{
                "clippy-rio-nix": (set(), "/d/clippy.drv"),
                "doc-rio-nix": (set(), "/d/doc.drv"),
            }
        )
        self.assertEqual(
            attach_drvs(clusters, meta),
            [
                {
                    "name": "rio-nix",
                    "targets": "clippy-rio-nix doc-rio-nix",
                    "drvs": "/d/clippy.drv /d/doc.drv",
                }
            ],
        )

    def test_partition_by_warm_overlap(self):
        clusters = [
            {"name": "rio-nix", "targets": "clippy-rio-nix"},
            {"name": "misc", "targets": "treefmt"},
        ]
        meta = _meta(
            **{
                "clippy-rio-nix": ({"/d/trunk.drv"}, "/d/c.drv"),
                "treefmt": (set(), "/d/t.drv"),
            }
        )
        gated, nowait = partition_gated(clusters, meta, {"/d/trunk.drv"})
        self.assertEqual([c["name"] for c in gated], ["rio-nix"])
        self.assertEqual([c["name"] for c in nowait], ["misc"])

    def test_empty_warm_set_gates_nothing(self):
        clusters = [{"name": "misc", "targets": "treefmt"}]
        meta = _meta(treefmt=(set(), "/d/t.drv"))
        gated, nowait = partition_gated(clusters, meta, set())
        self.assertEqual(gated, [])
        self.assertEqual([c["name"] for c in nowait], ["misc"])

    def test_mixed_cluster_splits_so_fast_checks_do_not_wait(self):
        # treefmt and golden-conformance share the misc cluster.
        # golden needs the rust trunk; treefmt does not. The cluster
        # must split so a formatting error surfaces while the trunk
        # is still compiling.
        clusters = [
            {"name": "misc", "targets": "golden-conformance treefmt"}
        ]
        meta = _meta(
            **{
                "golden-conformance": ({"/d/trunk.drv"}, "/d/g.drv"),
                "treefmt": (set(), "/d/t.drv"),
            }
        )
        gated, nowait = partition_gated(clusters, meta, {"/d/trunk.drv"})
        self.assertEqual(
            gated, [{"name": "misc", "targets": "golden-conformance"}]
        )
        self.assertEqual(nowait, [{"name": "misc", "targets": "treefmt"}])


class ClusterTests(unittest.TestCase):
    def test_per_member_checks_cluster_by_member(self):
        got = cluster_checks(
            [
                "nextest-rio-store",
                "clippy-rio-store",
                "doc-rio-store",
                "clippy-test-rio-store",
                "clippy-rio-nix",
            ]
        )
        self.assertEqual(
            got,
            [
                {"name": "rio-nix", "targets": "clippy-rio-nix"},
                {
                    "name": "rio-store",
                    "targets": "clippy-rio-store clippy-test-rio-store "
                    "doc-rio-store nextest-rio-store",
                },
            ],
        )

    def test_clippy_test_prefix_not_eaten_by_clippy(self):
        got = cluster_checks(["clippy-test-rio-nix"])
        self.assertEqual(
            got, [{"name": "rio-nix", "targets": "clippy-test-rio-nix"}]
        )

    def test_non_member_checks_form_misc_cluster(self):
        got = cluster_checks(["treefmt", "helm-lint", "docs-data-fresh"])
        self.assertEqual(
            got,
            [{"name": "misc", "targets": "docs-data-fresh helm-lint treefmt"}],
        )

    def test_mixed_members_and_misc(self):
        got = cluster_checks(["treefmt", "nextest-rio-cli"])
        self.assertEqual(
            got,
            [
                {"name": "misc", "targets": "treefmt"},
                {"name": "rio-cli", "targets": "nextest-rio-cli"},
            ],
        )

    def test_empty_input_empty_output(self):
        self.assertEqual(cluster_checks([]), [])

    def test_coverage_unit_collapses_vm_stays(self):
        got = cluster_coverage(
            ["unit-rio-store", "unit-rio-nix", "vm-chaos-standalone"]
        )
        self.assertEqual(
            got,
            [
                {"name": "unit", "targets": "unit-rio-nix unit-rio-store"},
                {
                    "name": "vm-chaos-standalone",
                    "targets": "vm-chaos-standalone",
                },
            ],
        )

    def test_singletons(self):
        got = singletons(["fuzz-refscan", "fuzz-nar_parsing"])
        self.assertEqual(
            got,
            [
                {"name": "fuzz-nar_parsing", "targets": "fuzz-nar_parsing"},
                {"name": "fuzz-refscan", "targets": "fuzz-refscan"},
            ],
        )


class OutputTests(unittest.TestCase):
    def test_coverage_paths_includes_cached_entries(self):
        results = [
            _entry("coverage.unit-rio-nix", "cached", out="/nix/store/aaa"),
            _entry(
                "coverage.vm-chaos-standalone",
                "notBuilt",
                out="/nix/store/bbb",
            ),
            _entry("checks.treefmt", "notBuilt", out="/nix/store/ccc"),
        ]
        self.assertEqual(
            coverage_paths(results),
            {
                "unit-rio-nix": "/nix/store/aaa",
                "vm-chaos-standalone": "/nix/store/bbb",
            },
        )

    def test_build_outputs_end_to_end(self):
        trunk = "/d/rio-common-build.drv"
        results = [
            # Two crates' clippy both need the trunk drv -> it is
            # shared across two clusters -> warm set for checks.
            _entry(
                "checks.clippy-rio-nix",
                "notBuilt",
                drv="/d/clippy-rio-nix.drv",
                needed=[trunk, "/d/clippy-rio-nix.drv"],
            ),
            _entry(
                "checks.clippy-rio-store",
                "notBuilt",
                drv="/d/clippy-rio-store.drv",
                needed=[trunk, "/d/clippy-rio-store.drv"],
            ),
            # treefmt needs nothing from the trunk -> nowait.
            _entry(
                "checks.treefmt",
                "notBuilt",
                drv="/d/treefmt.drv",
                needed=["/d/treefmt.drv"],
            ),
            _entry("checks.doc-rio-nix", "cached"),
            # Single uncached fuzz entry -> nothing shared -> no warm.
            _entry("fuzz.fuzz-refscan", "notBuilt", drv="/d/refscan.drv"),
            _entry("vm-test.vm-chaos-standalone", "cached"),
            _entry("coverage.unit-rio-nix", "cached", out="/nix/store/aaa"),
        ]
        out = build_outputs(results)
        self.assertEqual(
            json.loads(out["warm"]),
            {"checks": trunk, "fuzz": "", "vm-test": "", "coverage": ""},
        )
        self.assertEqual(
            json.loads(out["checks"]),
            [
                {
                    "name": "rio-nix",
                    "targets": "clippy-rio-nix",
                    "drvs": "/d/clippy-rio-nix.drv",
                },
                {
                    "name": "rio-store",
                    "targets": "clippy-rio-store",
                    "drvs": "/d/clippy-rio-store.drv",
                },
            ],
        )
        self.assertEqual(
            json.loads(out["checks-nowait"]),
            [{"name": "misc", "targets": "treefmt", "drvs": "/d/treefmt.drv"}],
        )
        self.assertEqual(
            json.loads(out["fuzz"]),
            [
                {
                    "name": "fuzz-refscan",
                    "targets": "fuzz-refscan",
                    "drvs": "/d/refscan.drv",
                }
            ],
        )
        self.assertEqual(json.loads(out["vm-test"]), [])
        self.assertEqual(json.loads(out["coverage"]), [])
        self.assertEqual(
            json.loads(out["coverage-paths"]),
            {"unit-rio-nix": "/nix/store/aaa"},
        )

    def test_outputs_are_single_line(self):
        results = [_entry("checks.clippy-rio-nix", "notBuilt")]
        for key, value in build_outputs(results).items():
            self.assertNotIn(
                "\n", value, f"{key} output must be single-line"
            )

    def test_write_outputs_format(self):
        import io

        fh = io.StringIO()
        write_outputs({"checks": "[]", "warm": '["a"]'}, fh)
        self.assertEqual(fh.getvalue(), 'checks=[]\nwarm=["a"]\n')


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        sys.argv = [sys.argv[0]]
        unittest.main()
    else:
        main()
