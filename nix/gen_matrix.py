#!/usr/bin/env python3
# Emit GitHub Actions matrix outputs for ci.yml from a single
# nix-eval-jobs pass over `.#githubActions`, eliding entries already
# in the binary cache and grouping the survivors into clusters that
# share a runner.
#
# Replaces .github/scripts/gen-matrix.sh (bash+jq). The clustering
# and warm-stage logic pushed that script past what jq-in-bash can
# carry maintainably.
#
# Outputs written to $GITHUB_OUTPUT (one `key=json` line each):
#   warm            ["checks", ...]  kinds whose warm.<kind> trunk
#                                    aggregate is NOT cached -> the
#                                    matching warm-* job must run.
#   checks          [{"name": "rio-store",
#                     "targets": "clippy-rio-store nextest-rio-store"},
#                    {"name": "misc", "targets": "treefmt helm-lint"}]
#   fuzz            singleton clusters, same object shape
#   vm-test         singleton clusters, same object shape
#   coverage        one "unit" cluster + vm-* singletons
#   coverage-paths  {"unit-rio-store": "/nix/store/...", ...}
#
# Matrix entries are OBJECTS (name + space-separated targets) so a
# job can build several flake attrs in one `nix build --keep-going`
# invocation: the attrs share a store and a scheduler, so their
# common dependencies build once. `name` is the GHA display name and
# the runs-on selector (vm-* prefix -> KVM runner).
#
# Cache-filter granularity stays per-derivation: a cluster only
# contains its UNCACHED members.
#
# v2 (deferred): the nix-eval-jobs output also carries neededBuilds
# (the per-target uncached drv closure). Computing the exact shared
# set from it -- and dispatching drv paths instead of attr names --
# would replace the static warm.<kind> aggregates and kill the
# per-job flake eval. See the `::notice::redundancy` line main()
# emits: it quantifies what the static trunk misses.
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

# All matrix kinds, in stable emission order. `warm` is the
# pseudo-kind holding the trunk aggregates and is emitted as its own
# output rather than as a build matrix.
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


def uncached(results):
    """Entries not already in a substituter, as {kind: [name, ...]}.

    'local' and 'notBuilt' are both kept: CI runners have an empty
    store so 'local' never appears there, and keeping it makes local
    dry-runs reflect what a never-pushed branch would build.
    """
    out = defaultdict(list)
    for r in results:
        if r.get("cacheStatus") == "cached":
            continue
        kind, name = split_attr(r["attr"])
        out[kind].append(name)
    return dict(out)


def warm_kinds(results):
    """Kinds whose warm.<kind> aggregate is uncached, sorted."""
    return sorted(uncached(results).get("warm", []))


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
    """Assemble the full GITHUB_OUTPUT key->JSON-string map."""
    pending = uncached(results)
    clusterers = {
        "checks": cluster_checks,
        "fuzz": singletons,
        "vm-test": singletons,
        "coverage": cluster_coverage,
    }
    outputs = {"warm": json.dumps(warm_kinds(results))}
    for kind in MATRIX_KINDS:
        outputs[kind] = json.dumps(
            clusterers[kind](pending.get(kind, [])), separators=(",", ":")
        )
    outputs["coverage-paths"] = json.dumps(
        coverage_paths(results), separators=(",", ":")
    )
    return outputs


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

    # Visibility: what was elided, what runs, and what the static warm
    # trunks DON'T cover (drvs needed by >=2 uncached entries that the
    # warm aggregates would not have built first). The last number is
    # the case for graduating to dynamic shared-set dispatch (v2).
    skipped = sorted(
        r["attr"] for r in results if r.get("cacheStatus") == "cached"
    )
    print(f"::notice::cached (skipped): {json.dumps(skipped)}")
    print(f"::notice::warm: {outputs['warm']}")
    for kind in MATRIX_KINDS:
        print(f"::notice::{kind}: {outputs[kind]}")
    needed = defaultdict(int)
    for r in results:
        kind, _ = split_attr(r["attr"])
        if kind == "warm" or r.get("cacheStatus") == "cached":
            continue
        for drv in r.get("neededBuilds", []):
            needed[drv] += 1
    shared = sum(1 for n in needed.values() if n > 1)
    print(
        f"::notice::redundancy: {shared} uncached drvs are needed by >=2 "
        "matrix entries (the static warm trunks should cover most; a "
        "persistently high number is the case for dynamic drv dispatch)"
    )

    with open(output_path, "a") as fh:
        write_outputs(outputs, fh)


# ----------------------------------------------------------------------
# Self-tests: python3 nix/gen_matrix.py --self-test
# ----------------------------------------------------------------------


def _entry(attr, status="notBuilt", out=None, error=None):
    if error is not None:
        return {"attr": attr, "error": error}
    e = {"attr": attr, "cacheStatus": status}
    if out is not None:
        e["outputs"] = {"out": out}
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
            _entry("checks.b", "local"),
            _entry("checks.c", "notBuilt"),
            _entry("fuzz.d", "notBuilt"),
        ]
        self.assertEqual(
            uncached(results),
            {"checks": ["b", "c"], "fuzz": ["d"]},
        )

    def test_warm_kinds_only_uncached(self):
        results = [
            _entry("warm.checks", "notBuilt"),
            _entry("warm.fuzz", "cached"),
            _entry("warm.vm-test", "local"),
            _entry("checks.a", "notBuilt"),
        ]
        self.assertEqual(warm_kinds(results), ["checks", "vm-test"])


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
        results = [
            _entry("warm.checks", "notBuilt"),
            _entry("warm.fuzz", "cached"),
            _entry("warm.vm-test", "cached"),
            _entry("warm.coverage", "cached"),
            _entry("checks.clippy-rio-nix", "notBuilt"),
            _entry("checks.treefmt", "notBuilt"),
            _entry("checks.doc-rio-nix", "cached"),
            _entry("fuzz.fuzz-refscan", "notBuilt"),
            _entry("vm-test.vm-chaos-standalone", "cached"),
            _entry("coverage.unit-rio-nix", "cached", out="/nix/store/aaa"),
        ]
        out = build_outputs(results)
        self.assertEqual(json.loads(out["warm"]), ["checks"])
        self.assertEqual(
            json.loads(out["checks"]),
            [
                {"name": "misc", "targets": "treefmt"},
                {"name": "rio-nix", "targets": "clippy-rio-nix"},
            ],
        )
        self.assertEqual(
            json.loads(out["fuzz"]),
            [{"name": "fuzz-refscan", "targets": "fuzz-refscan"}],
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
