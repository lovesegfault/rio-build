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
#   warm            [{"name": "checks+vm-test",
#                     "drvs": "/nix/store/a.drv /nix/store/b.drv"}]
#                   The GLOBAL trunk (drvs needed by >=2 clusters of
#                   any kind), partitioned into dependency-closed
#                   components and packed into at most
#                   MAX_WARM_SHARDS matrix entries. One warm matrix
#                   job realises all shards in parallel before the
#                   gated fan-out starts. [] -> nothing shared ->
#                   warm skipped. Counting globally (not per kind)
#                   is what stops two warm runners from racing to
#                   build the member libs that both the check
#                   clusters and the docker images need.
#   checks          [{"name": "rio-store",
#                     "targets": "clippy-rio-store nextest-rio-store",
#                     "drvs": "/nix/store/x.drv /nix/store/y.drv"}]
#                   Clusters whose members need something from the
#                   trunk -- they gate on the warm matrix.
#   checks-nowait   Same shape; clusters with NO overlap with the
#                   warm set. They start as soon as gen-matrix
#                   finishes (treefmt feedback does not wait for the
#                   rust trunk). Only `checks` and `formal` get this
#                   split: every fuzz/vm/coverage entry always needs
#                   its kind's trunk, so a nowait partition there
#                   would always be empty.
#   formal          Formal-verification checks (quint/TLC model
#                   checks, MBT conformance, kani proofs): kani-*
#                   singletons plus round-robin shards of the rest,
#                   gated on the warm matrix. Same object shape.
#   formal-nowait   The non-gated formal clusters (in practice the
#                   quint shards -- their only inputs are the .qnt
#                   models and the quint toolchain from the public
#                   cache, so they never wait for the rust trunk).
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
import shutil
import subprocess
import sys
import unittest
from collections import defaultdict

# Substituted by pkgs.replaceVars when this script is built into
# `packages.gen-matrix`. CI runs the file straight from the checkout
# (`python3 nix/gen_matrix.py` -- the wrapper package costs an extra
# flake eval plus a python+flake8 toolchain substitution on the
# critical path), which leaves the literal in place; resolve_nix_eval_jobs
# falls back to PATH and then to building the pinned binary.
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
MATRIX_KINDS = ("checks", "formal", "fuzz", "vm-test", "coverage")

# Kinds whose clusters are partitioned into a warm-gated matrix and a
# nowait matrix (`<kind>` / `<kind>-nowait`) by trunk overlap. fuzz /
# vm-test / coverage entries always need their kind's trunk, so a
# nowait partition there would always be empty.
SPLIT_KINDS = ("checks", "formal")

# Apalache server heap (MiB) assumed for a formal check whose
# derivation exports no meta.serverHeapMb (bug_383): matches
# quint.nix's constructor default. Checks at or below this ride the
# round-robin shards; anything above is isolated into a singleton
# shard named for the check.
DEFAULT_SERVER_HEAP_MB = 4096

# Documentation constant (bug_383): the per-shard memory envelope the
# ci.yml formal jobs are budgeted against. Derivation: a shard builds
# with `nix build --max-jobs 2`, so the worst case is two concurrent
# DEFAULT_SERVER_HEAP_MB Apalache servers + TLC + node overhead
# (~2×(4GiB + ~0.5GiB) ≈ 9-10GiB) — inside a 16GiB runner. A check
# that RAISES serverHeapMb past the default would break that envelope
# inside a shared shard (8GiB server × 2 jobs > 16GiB), which is
# exactly why cluster_formal isolates it as a singleton (one heavy
# server + one light neighbor at most ≈ 12-13GiB, still inside
# 16GiB). The value is exported by the quint.nix constructors as
# derivation meta — ONE binding sizes the JVM and the shard placement.
FORMAL_SHARD_BUDGET_MB = 2 * DEFAULT_SERVER_HEAP_MB

# Target shard width for the `formal` kind (see cluster_formal).
# Override with FORMAL_SHARD_SIZE (validated by formal_shard_size --
# anything that is not an integer >= 1 is a hard gen-matrix failure).
# Sized so a shard stays well inside its job timeout even when
# round-robin deals it two or three of the minutes-class exhaustive
# regime checks, while keeping the worst-case matrix (every formal
# check uncached) under ~20 jobs. ci.yml's formal jobs build a shard
# with `nix build --max-jobs 2` (memory budget -- see the formal job
# comment there), so 12 entries is ~6 build waves; widen the shards
# only together with that budget.
DEFAULT_FORMAL_SHARD_SIZE = 12

# Upper bound on warm matrix width. The trunk usually decomposes into
# 2-4 independent components (the normal tree + images, the
# instrumented tree, the fuzz workspaces), each of which gets its own
# runner; more components than this get packed together. Raising it
# only helps when the trunk has more genuinely independent components
# than this, which would be unusual for this dependency graph.
MAX_WARM_SHARDS = 4


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
            # bug_383: absent for non-quint kinds and for derivations
            # that predate the meta export — both default to the
            # constructor's 4096, i.e. "not heavy".
            "serverHeapMb": (r.get("meta") or {}).get(
                "serverHeapMb", DEFAULT_SERVER_HEAP_MB
            ),
        }
    return dict(out)


def cluster_needed(cluster, meta):
    """Union of a cluster's members' needed-sets."""
    needed = set()
    for target in cluster["targets"].split():
        needed |= meta[target]["needed"]
    return needed


def global_trunk(kind_clusters, kind_meta):
    """Drvs needed by >=2 clusters ACROSS ALL KINDS, as a set.

    This is the warm stage's work list: build these once and every
    cluster that needs them substitutes instead of rebuilding. The
    count is global, not per-kind, because the kinds' closures
    overlap (the docker images embed the same member libs the check
    clusters link against; on a dep bump both rustc profiles need the
    same third-party crates) and a per-kind count would assign the
    overlap to several warm jobs that then race to build it.

    Drvs needed by only ONE cluster are left to that cluster's job --
    warming them would serialize the cluster behind the warm stage
    for no dedup benefit.
    """
    counts = defaultdict(int)
    for kind, clusters in kind_clusters.items():
        for cluster in clusters:
            for drv in cluster_needed(cluster, kind_meta[kind]):
                counts[drv] += 1
    return {drv for drv, n in counts.items() if n >= 2}


def trunk_components(trunk, kind_clusters, kind_meta):
    """Partition the trunk into co-occurrence components.

    Two trunk drvs are connected iff some cluster needs both.
    neededBuilds is closed under uncached-dependency, so every
    dependency edge inside the trunk is also a co-occurrence edge --
    which makes each component dependency-closed: no component ever
    needs a drv from another component, so components can build on
    separate runners concurrently with zero cross-talk.

    Returns [{"drvs": frozenset, "kinds": frozenset}] sorted largest
    first. `kinds` is the set of matrix kinds whose clusters touch
    the component -- it exists purely to give the warm shard a
    human-readable name in the GHA UI.
    """
    parent = {drv: drv for drv in trunk}

    def find(x):
        root = x
        while parent[root] != root:
            root = parent[root]
        while parent[x] != root:
            parent[x], x = root, parent[x]
        return root

    kinds_touching = defaultdict(set)
    for kind, clusters in kind_clusters.items():
        for cluster in clusters:
            shared = cluster_needed(cluster, kind_meta[kind]) & trunk
            if not shared:
                continue
            it = iter(shared)
            first = next(it)
            for drv in it:
                parent[find(drv)] = find(first)
            kinds_touching[find(first)].add(kind)

    groups = defaultdict(set)
    for drv in trunk:
        groups[find(drv)].add(drv)
    comps = [
        {
            "drvs": frozenset(drvs),
            # kinds_touching is keyed by a root that may have been
            # re-parented by a later union; re-resolve every root.
            "kinds": frozenset(
                kind
                for root, kinds in kinds_touching.items()
                if find(root) == find(next(iter(drvs)))
                for kind in kinds
            ),
        }
        for drvs in groups.values()
    ]
    return sorted(
        comps, key=lambda c: (-len(c["drvs"]), sorted(c["drvs"]))
    )


def pack_shards(comps, max_shards):
    """LPT-pack components into at most max_shards warm matrix
    entries: [{"name": "checks+vm-test", "drvs": "a.drv b.drv"}].

    One shard per component when there are few components (the common
    case: the normal tree + images, the instrumented tree, the fuzz
    tree). More components than slots -> largest-first into the
    currently-lightest shard, which keeps max(shard) within 4/3 of
    optimal. Components are never split: splitting one would put a
    drv's dependency in a sibling shard that runs concurrently.
    """
    if not comps:
        return []
    bins = [
        {"drvs": set(), "kinds": set()}
        for _ in range(min(len(comps), max_shards))
    ]
    # comps arrive largest-first from trunk_components; keep that
    # order so LPT's approximation bound holds.
    for comp in comps:
        target = min(bins, key=lambda b: len(b["drvs"]))
        target["drvs"] |= comp["drvs"]
        target["kinds"] |= comp["kinds"]
    return [
        {
            "name": "+".join(sorted(b["kinds"])) or "trunk",
            "drvs": " ".join(sorted(b["drvs"])),
        }
        for b in bins
        if b["drvs"]
    ]


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


def formal_shard_size():
    """FORMAL_SHARD_SIZE from the environment, validated.

    The knob feeds cluster_formal's ceil-division and slicing: a value
    < 1 would silently emit an EMPTY shard list -- every non-kani
    formal check dropped from the matrix while ci-gate stays green --
    and a non-integer would die with a raw traceback. Both become a
    one-line hard failure naming the knob and the offending value
    instead; gen-matrix must never quietly emit a thinner formal lane.
    """
    raw = os.environ.get("FORMAL_SHARD_SIZE")
    if raw is None:
        return DEFAULT_FORMAL_SHARD_SIZE
    try:
        value = int(raw)
    except ValueError:
        value = None
    if value is None or value < 1:
        sys.exit(
            f"FORMAL_SHARD_SIZE must be an integer >= 1, got {raw!r}"
        )
    return value


def cluster_formal(names, shard_size=None, heap_by_name=None):
    """Shard the formal-verification checks (quint-*/mbt-*/kani-*).

    These derivations' build IS the verification run -- a TLC or CBMC
    process per check, JVM heaps in the gigabytes -- and the full set
    is ~160 entries growing by tens per campaign. One runner cannot
    absorb them: when they shared the catch-all `misc` cluster, that
    job starved its runner of memory long before the timeout.

    kani-* entries stay singletons: each drags a per-member
    kani-compiler closure whose build cost has nothing to do with the
    quint shards, the job name gives exact attribution, and the warm
    stage already dedups the closure they share. Everything else is
    dealt round-robin into shards of at most shard_size entries over
    the sorted name list -- round-robin because the expensive
    exhaustive regime checks sort adjacently (quint-<model>-base /
    -contend / -corrupt / ...), and contiguous chunking would stack
    several of them into one shard while its siblings get only cheap
    witness checks.
    """
    if shard_size is None:
        shard_size = formal_shard_size()
    heap_by_name = heap_by_name or {}
    kani = sorted(n for n in names if n.startswith("kani-"))

    # bug_383: a check whose Apalache server heap exceeds the default
    # cannot share a shard — two concurrent heavy servers under
    # `--max-jobs 2` blow the runner envelope (FORMAL_SHARD_BUDGET_MB).
    # Isolate each into a singleton named for the check (the kani
    # pattern: exact attribution, no neighbor to starve).
    def is_heavy(n):
        return heap_by_name.get(n, DEFAULT_SERVER_HEAP_MB) > DEFAULT_SERVER_HEAP_MB

    nonkani = [n for n in names if not n.startswith("kani-")]
    heavy = sorted(n for n in nonkani if is_heavy(n))
    rest = sorted(n for n in nonkani if not is_heavy(n))
    clusters = [{"name": n, "targets": n} for n in kani]
    clusters.extend({"name": n, "targets": n} for n in heavy)
    if rest:
        count = -(-len(rest) // shard_size)  # ceil division
        width = len(str(count))
        clusters.extend(
            {
                "name": f"quint-{i + 1:0{width}d}of{count}",
                "targets": " ".join(rest[i::count]),
            }
            for i in range(count)
        )
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
    and -- for checks and formal -- split the clusters into
    gated/nowait by whether they overlap the warm set.
    """
    pend = pending(results)
    clusterers = {
        "checks": cluster_checks,
        "formal": cluster_formal,
        "fuzz": singletons,
        "vm-test": singletons,
        "coverage": cluster_coverage,
    }
    # Cluster every kind first: the trunk analysis is global, so it
    # needs the full cluster list before any kind's matrix can be
    # finalized.
    kind_clusters = {
        kind: (
            cluster_formal(
                sorted(pend.get(kind, {})),
                heap_by_name={
                    n: m.get("serverHeapMb", DEFAULT_SERVER_HEAP_MB)
                    for n, m in pend.get(kind, {}).items()
                },
            )
            if kind == "formal"
            else clusterers[kind](sorted(pend.get(kind, {})))
        )
        for kind in MATRIX_KINDS
    }
    trunk = global_trunk(kind_clusters, pend)
    shards = pack_shards(
        trunk_components(trunk, kind_clusters, pend), MAX_WARM_SHARDS
    )
    matrices = {"warm": shards}
    for kind in MATRIX_KINDS:
        meta = pend.get(kind, {})
        clusters = kind_clusters[kind]
        if kind in SPLIT_KINDS:
            gated, nowait = partition_gated(clusters, meta, trunk)
            matrices[kind] = attach_drvs(gated, meta)
            matrices[f"{kind}-nowait"] = attach_drvs(nowait, meta)
        else:
            matrices[kind] = attach_drvs(clusters, meta)
    outputs = {}
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


def resolve_nix_eval_jobs():
    """Locate nix-eval-jobs without paying for a wrapper-package build.

    Preference order: the path replaceVars baked in (running as the
    built `gen-matrix` package), the runner image's PATH (free once
    the image pre-bakes it), and finally `nix build .#nix-eval-jobs`
    (one flake eval, still cheaper than `nix run .#gen-matrix` which
    additionally builds the flake8-checked wrapper and substitutes the
    python toolchain).
    """
    if not NIX_EVAL_JOBS.startswith("@"):
        return NIX_EVAL_JOBS
    on_path = shutil.which("nix-eval-jobs")
    if on_path:
        return on_path
    out_path = subprocess.run(
        ["nix", "build", ".#nix-eval-jobs", "--no-link", "--print-out-paths"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout.strip()
    return f"{out_path}/bin/nix-eval-jobs"


def run_nix_eval_jobs(workers):
    """Stream-eval .#githubActions, echoing progress to stderr.

    Returns the parsed JSONL list. --force-recurse: the matrix kinds
    are plain attrsets with no recurseIntoAttrs marker.
    --check-cache-status: probe configured substituters per attr so
    cached entries can be elided.
    """
    cmd = [
        resolve_nix_eval_jobs(),
        "--flake",
        ".#githubActions",
        "--force-recurse",
        "--check-cache-status",
        # bug_383: surface meta.serverHeapMb so cluster_formal can
        # isolate heavy Apalache checks (verified supported and
        # composable with --check-cache-status on the pinned
        # nix-eval-jobs).
        "--meta",
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
    # Validate the FORMAL_SHARD_SIZE knob up front so a bad value is a
    # sub-second failure, not one surfacing after the multi-minute
    # nix-eval-jobs pass (cluster_formal re-reads it later either way).
    formal_shard_size()
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
    for shard in json.loads(outputs["warm"]):
        n = len(shard["drvs"].split())
        print(f"::notice::warm {shard['name']}: {n} shared drvs")
    for key in (
        "checks",
        "checks-nowait",
        "formal",
        "formal-nowait",
        "fuzz",
        "vm-test",
        "coverage",
    ):
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
                        "serverHeapMb": DEFAULT_SERVER_HEAP_MB,
                    },
                    "c": {
                        "drv": "/d/checks.c.drv",
                        "needed": frozenset(),
                        "serverHeapMb": DEFAULT_SERVER_HEAP_MB,
                    },
                },
                "fuzz": {
                    "d": {
                        "drv": "/d/fuzz.d.drv",
                        "needed": frozenset(),
                        "serverHeapMb": DEFAULT_SERVER_HEAP_MB,
                    },
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


def _kinded(**kinds):
    """Shorthand for (kind_clusters, kind_meta) from
    _kinded(checks=[("a", {"/d/x.drv"}), ...], ...). Each entry is a
    singleton cluster named after itself."""
    kind_clusters, kind_meta = {}, {}
    for kind, entries in kinds.items():
        kind_clusters[kind] = [
            {"name": n, "targets": n} for n, _ in entries
        ]
        kind_meta[kind] = {
            n: {"drv": f"/d/{n}.drv", "needed": frozenset(needed)}
            for n, needed in entries
        }
    return kind_clusters, kind_meta


class GlobalTrunkTests(unittest.TestCase):
    def test_sharing_counts_across_kinds(self):
        # The member lib is needed by one CHECK cluster and one VM
        # cluster. A per-kind count would see 1+1 and never warm it;
        # the global count sees 2 and does. This is the bug class
        # that had two warm jobs racing to build the same members.
        kc, km = _kinded(
            checks=[("clippy-rio-common", {"/d/member.drv"})],
            **{"vm-test": [("vm-chaos", {"/d/member.drv", "/d/img.drv"})]},
        )
        self.assertEqual(global_trunk(kc, km), {"/d/member.drv"})

    def test_intra_cluster_sharing_is_not_warmed(self):
        # Both members of ONE cluster need dep.drv; no other cluster
        # does. The cluster's single nix invocation already builds it
        # once -- warming it would only serialize this cluster behind
        # the warm stage.
        kc, km = _kinded(checks=[])
        kc["checks"] = [
            {
                "name": "rio-store",
                "targets": "clippy-rio-store nextest-rio-store",
            }
        ]
        km["checks"] = {
            "clippy-rio-store": {
                "drv": "/d/c.drv",
                "needed": frozenset({"/d/store-deps.drv"}),
            },
            "nextest-rio-store": {
                "drv": "/d/n.drv",
                "needed": frozenset({"/d/store-deps.drv"}),
            },
        }
        self.assertEqual(global_trunk(kc, km), set())

    def test_single_cluster_total_has_empty_trunk(self):
        kc, km = _kinded(checks=[("a", {"/d/dep.drv"})])
        self.assertEqual(global_trunk(kc, km), set())


class ComponentTests(unittest.TestCase):
    def test_disjoint_closures_are_separate_components(self):
        # The normal tree (checks+vm) and the instrumented tree
        # (coverage) never appear in the same cluster's needs ->
        # independent components -> separate warm runners.
        kc, km = _kinded(
            checks=[
                ("clippy-a", {"/d/member.drv"}),
                ("clippy-b", {"/d/member.drv"}),
            ],
            coverage=[
                ("unit", {"/d/member-cov.drv"}),
                ("vm-x", {"/d/member-cov.drv"}),
            ],
        )
        trunk = global_trunk(kc, km)
        comps = trunk_components(trunk, kc, km)
        self.assertEqual(
            sorted(sorted(c["drvs"]) for c in comps),
            [["/d/member-cov.drv"], ["/d/member.drv"]],
        )
        self.assertEqual(
            sorted(sorted(c["kinds"]) for c in comps),
            [["checks"], ["coverage"]],
        )

    def test_co_occurrence_bridges_into_one_component(self):
        # The image depends on the member, so any vm cluster's needs
        # contain both -> they must land in the same shard (a shard
        # cannot substitute a sibling shard's output).
        kc, km = _kinded(
            checks=[
                ("clippy-a", {"/d/member.drv"}),
                ("clippy-b", {"/d/member.drv"}),
            ],
            **{
                "vm-test": [
                    ("vm-x", {"/d/member.drv", "/d/img.drv"}),
                    ("vm-y", {"/d/img.drv"}),
                ]
            },
        )
        trunk = global_trunk(kc, km)
        comps = trunk_components(trunk, kc, km)
        self.assertEqual(len(comps), 1)
        self.assertEqual(comps[0]["drvs"], frozenset({"/d/member.drv", "/d/img.drv"}))
        self.assertEqual(comps[0]["kinds"], frozenset({"checks", "vm-test"}))

    def test_components_sorted_largest_first(self):
        kc, km = _kinded(
            checks=[
                ("a", {"/d/x.drv"}),
                ("b", {"/d/x.drv"}),
            ],
            fuzz=[
                ("f1", {"/d/p.drv", "/d/q.drv", "/d/r.drv"}),
                ("f2", {"/d/p.drv", "/d/q.drv", "/d/r.drv"}),
            ],
        )
        comps = trunk_components(global_trunk(kc, km), kc, km)
        self.assertEqual(
            [len(c["drvs"]) for c in comps], [3, 1]
        )


class PackTests(unittest.TestCase):
    @staticmethod
    def _comp(kinds, *drvs):
        return {"drvs": frozenset(drvs), "kinds": frozenset(kinds)}

    def test_one_shard_per_component_when_room(self):
        comps = [
            self._comp({"checks", "vm-test"}, "/d/a.drv", "/d/b.drv"),
            self._comp({"coverage"}, "/d/c.drv"),
        ]
        self.assertEqual(
            pack_shards(comps, 4),
            [
                {"name": "checks+vm-test", "drvs": "/d/a.drv /d/b.drv"},
                {"name": "coverage", "drvs": "/d/c.drv"},
            ],
        )

    def test_overflow_packs_into_lightest_shard(self):
        comps = [
            self._comp({"checks"}, "/d/a.drv", "/d/b.drv", "/d/c.drv"),
            self._comp({"coverage"}, "/d/d.drv", "/d/e.drv"),
            self._comp({"fuzz"}, "/d/f.drv"),
        ]
        shards = pack_shards(comps, 2)
        self.assertEqual(len(shards), 2)
        # Largest component alone in shard 0; the two smaller ones
        # packed together in shard 1 (3 vs 2+1).
        self.assertEqual(
            sorted(s["drvs"].split() for s in shards),
            [
                ["/d/a.drv", "/d/b.drv", "/d/c.drv"],
                ["/d/d.drv", "/d/e.drv", "/d/f.drv"],
            ],
        )
        merged = next(s for s in shards if "/d/f.drv" in s["drvs"])
        self.assertEqual(merged["name"], "coverage+fuzz")

    def test_empty_components_empty_shards(self):
        self.assertEqual(pack_shards([], 4), [])


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


class FormalClusterTests(unittest.TestCase):
    def setUp(self):
        # Keep the env-knob tests hermetic: stash any ambient
        # FORMAL_SHARD_SIZE and restore it afterwards.
        self._saved_shard_size = os.environ.pop("FORMAL_SHARD_SIZE", None)

    def tearDown(self):
        os.environ.pop("FORMAL_SHARD_SIZE", None)
        if self._saved_shard_size is not None:
            os.environ["FORMAL_SHARD_SIZE"] = self._saved_shard_size

    def test_heavy_checks_isolated_into_singletons(self):
        # bug_383 red-first: against the pre-isolation cluster_formal,
        # this test FAILS — the 8GiB checks land inside round-robin
        # shards and two of them under --max-jobs 2 exceed the runner
        # envelope (FORMAL_SHARD_BUDGET_MB).
        names = [f"quint-light{i}" for i in range(6)] + [
            "quint-heavy-a",
            "quint-heavy-b",
            "kani-rio-lease",
        ]
        heap = {
            "quint-heavy-a": 8192,
            "quint-heavy-b": 8192,
            # absent entries default to DEFAULT_SERVER_HEAP_MB
        }
        got = cluster_formal(names, shard_size=3, heap_by_name=heap)
        by_name = {c["name"]: c["targets"] for c in got}
        # Each heavy is a singleton named for the check (kani pattern).
        self.assertEqual(by_name["quint-heavy-a"], "quint-heavy-a")
        self.assertEqual(by_name["quint-heavy-b"], "quint-heavy-b")
        # kani stays a singleton too.
        self.assertEqual(by_name["kani-rio-lease"], "kani-rio-lease")
        # Lights are sharded; no heavy appears in any shared shard and
        # nothing is lost.
        shard_members = " ".join(
            c["targets"] for c in got if c["name"].startswith("quint-") and "of" in c["name"]
        ).split()
        self.assertEqual(
            sorted(shard_members), sorted(f"quint-light{i}" for i in range(6))
        )

    def test_absent_heap_meta_defaults_light(self):
        # A formal set with NO heap metadata behaves exactly as before
        # (pure round-robin) — the meta export is additive.
        names = [f"quint-w{i}" for i in range(4)]
        with_meta = cluster_formal(names, shard_size=2, heap_by_name={})
        without = cluster_formal(names, shard_size=2)
        self.assertEqual(with_meta, without)

    def test_kani_singletons_rest_sharded(self):
        names = [f"quint-w{i}" for i in range(5)] + [
            "kani-rio-lease",
            "mbt-rio-lease",
        ]
        got = cluster_formal(names, shard_size=3)
        self.assertEqual(
            got[0], {"name": "kani-rio-lease", "targets": "kani-rio-lease"}
        )
        shards = got[1:]
        self.assertEqual([s["name"] for s in shards], ["quint-1of2", "quint-2of2"])
        # Every non-kani name lands in exactly one shard, none lost.
        members = " ".join(s["targets"] for s in shards).split()
        self.assertEqual(
            sorted(members),
            sorted(["mbt-rio-lease"] + [f"quint-w{i}" for i in range(5)]),
        )
        # Balanced within one entry of each other.
        sizes = [len(s["targets"].split()) for s in shards]
        self.assertLessEqual(max(sizes) - min(sizes), 1)

    def test_round_robin_spreads_adjacent_heavy_names(self):
        # Exhaustive regime checks sort adjacently (-base, -contend,
        # -corrupt, -crash). Round-robin must deal them to different
        # shards instead of stacking them into the first one.
        names = [
            "quint-m-base",
            "quint-m-contend",
            "quint-m-corrupt",
            "quint-m-crash",
            "quint-m-witness-a",
            "quint-m-witness-b",
        ]
        got = cluster_formal(names, shard_size=3)
        self.assertEqual(len(got), 2)
        first = got[0]["targets"].split()
        second = got[1]["targets"].split()
        self.assertIn("quint-m-base", first)
        self.assertIn("quint-m-contend", second)

    def test_shard_cap_and_count(self):
        names = [f"quint-{i:03d}" for i in range(25)]
        got = cluster_formal(names, shard_size=12)
        self.assertEqual(len(got), 3)
        self.assertTrue(
            all(len(c["targets"].split()) <= 12 for c in got)
        )

    def test_zero_padding_keeps_ui_sort_order(self):
        names = [f"quint-{i:03d}" for i in range(60)]
        got = cluster_formal(names, shard_size=6)
        self.assertEqual(got[0]["name"], "quint-01of10")
        self.assertEqual(got[-1]["name"], "quint-10of10")

    def test_empty_input_empty_output(self):
        self.assertEqual(cluster_formal([]), [])

    def test_shard_size_env_negative_fails_loudly(self):
        # A negative size would flow into the ceil-division as-is and
        # silently drop every non-kani entry from the matrix (the
        # shard count goes <= 0, so the range() emitting shards is
        # empty) while ci-gate stays green. It must instead be a hard
        # gen-matrix failure naming the knob and the value.
        os.environ["FORMAL_SHARD_SIZE"] = "-3"
        with self.assertRaises(SystemExit) as ctx:
            cluster_formal(["quint-a", "quint-b"])
        self.assertIn("FORMAL_SHARD_SIZE", str(ctx.exception))
        self.assertIn("-3", str(ctx.exception))

    def test_shard_size_env_non_integer_fails_loudly(self):
        # A non-integer would be a raw int() traceback and 0 a
        # ZeroDivisionError; both get the same one-line failure.
        for bad in ("twelve", "0", ""):
            with self.subTest(bad=bad):
                os.environ["FORMAL_SHARD_SIZE"] = bad
                with self.assertRaises(SystemExit) as ctx:
                    cluster_formal(["quint-a", "quint-b"])
                self.assertIn("FORMAL_SHARD_SIZE", str(ctx.exception))
                self.assertIn(repr(bad), str(ctx.exception))

    def test_shard_size_env_valid_override_is_used(self):
        os.environ["FORMAL_SHARD_SIZE"] = "2"
        got = cluster_formal([f"quint-{i}" for i in range(4)])
        self.assertEqual(
            [c["name"] for c in got], ["quint-1of2", "quint-2of2"]
        )

    def test_formal_partition_quint_nowait_kani_gated(self):
        # Two kani singletons share the kani-compiler member closure
        # -> it is in the trunk -> they gate on warm. The quint shard
        # needs nothing beyond its own drv -> it starts immediately.
        kani_dep = "/d/kani-member.drv"
        results = [
            _entry(
                "formal.kani-rio-lease",
                "notBuilt",
                drv="/d/kani-rio-lease.drv",
                needed=[kani_dep, "/d/kani-rio-lease.drv"],
            ),
            _entry(
                "formal.kani-rio-store",
                "notBuilt",
                drv="/d/kani-rio-store.drv",
                needed=[kani_dep, "/d/kani-rio-store.drv"],
            ),
            _entry(
                "formal.quint-leader-election",
                "notBuilt",
                drv="/d/quint-le.drv",
                needed=["/d/quint-le.drv"],
            ),
        ]
        out = build_outputs(results)
        self.assertEqual(
            json.loads(out["warm"]),
            [{"name": "formal", "drvs": kani_dep}],
        )
        gated = json.loads(out["formal"])
        nowait = json.loads(out["formal-nowait"])
        self.assertEqual(
            sorted(c["name"] for c in gated),
            ["kani-rio-lease", "kani-rio-store"],
        )
        self.assertEqual(
            nowait,
            [
                {
                    "name": "quint-1of1",
                    "targets": "quint-leader-election",
                    "drvs": "/d/quint-le.drv",
                }
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
        member = "/d/rio-common-build.drv"
        img = "/d/docker-img.drv"
        cov = "/d/rio-common-cov.drv"
        results = [
            # Two crates' clippy both need the member drv -> shared.
            _entry(
                "checks.clippy-rio-nix",
                "notBuilt",
                drv="/d/clippy-rio-nix.drv",
                needed=[member, "/d/clippy-rio-nix.drv"],
            ),
            _entry(
                "checks.clippy-rio-store",
                "notBuilt",
                drv="/d/clippy-rio-store.drv",
                needed=[member, "/d/clippy-rio-store.drv"],
            ),
            # treefmt needs nothing from the trunk -> nowait.
            _entry(
                "checks.treefmt",
                "notBuilt",
                drv="/d/treefmt.drv",
                needed=["/d/treefmt.drv"],
            ),
            _entry("checks.doc-rio-nix", "cached"),
            # Two VM tests need the image, which needs the member ->
            # the member and the image co-occur -> one component
            # spanning checks and vm-test.
            _entry(
                "vm-test.vm-a",
                "notBuilt",
                drv="/d/vm-a.drv",
                needed=[member, img, "/d/vm-a.drv"],
            ),
            _entry(
                "vm-test.vm-b",
                "notBuilt",
                drv="/d/vm-b.drv",
                needed=[member, img, "/d/vm-b.drv"],
            ),
            # The unit cluster and a vm-* coverage cluster both need
            # the instrumented member (the cov image embeds it) --
            # two distinct clusters -> trunk -> its own component,
            # disjoint from the normal tree.
            _entry(
                "coverage.unit-rio-nix",
                "notBuilt",
                drv="/d/unit-rio-nix.drv",
                needed=[cov],
                out="/nix/store/aaa",
            ),
            _entry(
                "coverage.vm-chaos-standalone",
                "notBuilt",
                drv="/d/cov-vm-chaos.drv",
                needed=[cov],
                out="/nix/store/bbb",
            ),
            # Single uncached fuzz entry -> nothing shared -> no warm.
            _entry("fuzz.fuzz-refscan", "notBuilt", drv="/d/refscan.drv"),
        ]
        out = build_outputs(results)
        # Two shards: the normal tree (member+img, largest first) and
        # the instrumented tree.
        self.assertEqual(
            json.loads(out["warm"]),
            [
                {
                    "name": "checks+vm-test",
                    "drvs": f"{img} {member}",
                },
                {"name": "coverage", "drvs": cov},
            ],
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
        self.assertEqual(len(json.loads(out["vm-test"])), 2)
        self.assertEqual(len(json.loads(out["coverage"])), 2)
        self.assertEqual(
            json.loads(out["coverage-paths"]),
            {
                "unit-rio-nix": "/nix/store/aaa",
                "vm-chaos-standalone": "/nix/store/bbb",
            },
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
