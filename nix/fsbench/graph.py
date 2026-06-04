#!/usr/bin/env python3
"""Graph castore-FUSE fsbench measurements.

Two input shapes, auto-detected by the file's ``schema``:

* ``fsbench/v1`` (``result.json``) — single-run snapshots. Plots cold/warm
  read throughput, jq build cold-vs-warm, and overfetch across runs.
  Cold phases are single-rep (no error bar); warm phases carry a 3-rep
  spread; a "cold" run is only honest if its cold-open latency is high.
* ``fsbench-cold-reps/v1`` (``cold-reps.json``) — N-cold-rep aggregates
  from ``fsbench cold-reps``. Plots jq_build_cold wall time (total +
  configure split) and read_storm_cold throughput as one bar per input
  file, with stderr error bars, and annotates whether the mean±stderr
  bands of the first two files overlap.

The script is before/after-agnostic: "before vs after" is just which two
files you hand it.

Usage:
    graph.py [PATH ...] [-o OUT.png]

Each PATH is either a directory (globbed for ``*/result.json``) or an
explicit ``result.json`` / ``cold-reps.json`` file. Defaults: PATH=.,
OUT=fsbench.png. When any cold-reps file is given, the cold-reps plot is
produced; otherwise the across-runs result.json plot.
"""

from __future__ import annotations

import argparse
import glob
import json
import os
import sys

import matplotlib

matplotlib.use("Agg")  # headless: render straight to PNG
import matplotlib.pyplot as plt
import numpy as np

# A cold run whose cold-open p99 is below this is really a warm-cache run
# masquerading as cold (chunks already resident -> opens are ~instant).
# The honesty gate proper checks promote bytes; this is the cheap proxy we
# can compute from phase data alone.
HONEST_COLD_OPEN_MS = 10.0


def _metric(phase: dict, key: str):
    """Return a (value, rep_spread) pair for a phase metric, or (None, None)."""
    m = phase.get("metrics", {}).get(key)
    if not isinstance(m, dict):
        return None, None
    val = m.get("value", m.get("p50"))
    return val, m.get("rep_spread")


def _phase_secs(phase: dict):
    a, b = phase.get("start_epoch_ms"), phase.get("end_epoch_ms")
    return (b - a) / 1000.0 if a is not None and b is not None else None


def _run_row(d: dict) -> dict:
    """Build the across-runs row for one fsbench/v1 result document."""
    ph = d.get("phases", {})
    cold_mib, _ = _metric(ph.get("read_storm_cold", {}), "mib_s")
    warm_mib, warm_spread = _metric(ph.get("read_storm_warm", {}), "mib_s")
    lwarm_mib, _ = _metric(ph.get("read_storm_local_warm", {}), "mib_s")
    cold_open = ph.get("read_storm_cold", {}).get("metrics", {}).get("open_ns", {})
    cold_open_p99_ms = (cold_open.get("p99", 0) or 0) / 1e6 if isinstance(cold_open, dict) else 0
    jq_cold = _phase_secs(ph.get("jq_build_cold", {}))
    jq_warm = _phase_secs(ph.get("jq_build_warm", {}))
    cm = d.get("cluster_metrics", {})
    mnt = cm.get("mountd", {}) if isinstance(cm, dict) else {}
    promote = mnt.get("promote_bytes_total_delta")
    uniq = d.get("workload", {}).get("unique_chunk_bytes")
    return {
        "label": d.get("created_at", "?")[11:19],
        "inst": d.get("placement", {}).get("instance_type", "?"),
        "cold_mib": cold_mib,
        "warm_mib": warm_mib,
        "warm_spread": warm_spread,
        "lwarm_mib": lwarm_mib,
        "cold_open_p99_ms": cold_open_p99_ms,
        "jq_cold": jq_cold,
        "jq_warm": jq_warm,
        "jq_fetch": (jq_cold - jq_warm) if (jq_cold and jq_warm) else None,
        "overfetch_pct": (100 * (promote - uniq) / uniq) if (promote and uniq) else None,
        "honest": cold_open_p99_ms >= HONEST_COLD_OPEN_MS,
    }


def load_runs(fsbench_dir: str) -> list[dict]:
    # Accept either the .fsbench dir itself or its parent.
    roots = [
        os.path.join(fsbench_dir, "*", "result.json"),
        os.path.join(fsbench_dir, ".fsbench", "*", "result.json"),
    ]
    paths: list[str] = []
    for pat in roots:
        paths.extend(glob.glob(pat))
    runs = []
    for p in sorted(set(paths)):
        try:
            d = json.load(open(p))
        except (OSError, json.JSONDecodeError):
            continue
        if d.get("schema") != "fsbench/v1":
            continue
        runs.append(_run_row(d))
    return runs


def _stem(path: str) -> str:
    base = os.path.basename(path)
    return base[:-5] if base.endswith(".json") else base


def _cold_reps_row(path: str, d: dict) -> dict:
    return {
        "stem": _stem(path),
        "node": d.get("node") or "?",
        "inst": d.get("instance_type") or "?",
        "reps_accepted": d.get("reps_accepted"),
        "metrics": d.get("metrics", {}),
    }


def collect(inputs: list[str]) -> tuple[list[dict], list[dict]]:
    """Partition inputs into across-runs rows and cold-reps rows.

    Directories are globbed for result.json (legacy path); explicit .json
    files dispatch on their schema.
    """
    runs: list[dict] = []
    cold: list[dict] = []
    for inp in inputs:
        if os.path.isdir(inp):
            runs.extend(load_runs(inp))
            continue
        if not inp.endswith(".json"):
            continue
        try:
            d = json.load(open(inp))
        except (OSError, json.JSONDecodeError):
            continue
        schema = d.get("schema")
        if schema == "fsbench-cold-reps/v1":
            cold.append(_cold_reps_row(inp, d))
        elif schema == "fsbench/v1":
            runs.append(_run_row(d))
    return runs, cold


def plot(runs: list[dict], out: str) -> None:
    if not runs:
        sys.exit("no fsbench/v1 result.json files found")
    n = len(runs)
    x = np.arange(n)
    xlabels = [f"{r['label']}\n{r['inst']}" for r in runs]
    # Honest-cold runs in solid colour, warm-cache (dishonest) runs hatched/grey.
    honest = [r["honest"] for r in runs]
    bar_c = ["#2a7" if h else "#bbb" for h in honest]

    fig, axes = plt.subplots(2, 2, figsize=(13, 9))
    fig.suptitle(
        "castore-FUSE fsbench — across runs  (green = honest cold, grey = warm-cache run)",
        fontsize=13,
        fontweight="bold",
    )

    def bars(ax, vals, title, ylabel, fmt="{:.1f}"):
        v = [0 if x is None else x for x in vals]
        b = ax.bar(x, v, color=bar_c, edgecolor="#333")
        for h in [i for i, r in enumerate(runs) if not r["honest"]]:
            b[h].set_hatch("////")
        ax.set_title(title, fontsize=11, fontweight="bold")
        ax.set_ylabel(ylabel)
        ax.set_xticks(x)
        ax.set_xticklabels(xlabels, fontsize=8)
        for i, val in enumerate(vals):
            if val is not None:
                ax.text(i, v[i], fmt.format(val), ha="center", va="bottom", fontsize=8)
        return b

    # 1. Cold read MiB/s — the headline. No error bar: single-rep by construction.
    ax = axes[0, 0]
    bars(ax, [r["cold_mib"] for r in runs], "Cold read (read_storm_cold)", "MiB/s")
    ax.text(
        0.5,
        0.93,
        "single-rep — no variance measured",
        transform=ax.transAxes,
        ha="center",
        fontsize=8,
        style="italic",
        color="#a00",
    )

    # 2. Warm read MiB/s WITH the 3-rep spread as an error bar.
    ax = axes[0, 1]
    warm = [r["warm_mib"] for r in runs]
    wv = [0 if x is None else x for x in warm]
    # rep_spread = (max-min)/median; draw it as a symmetric +/- half-spread bar.
    err = [
        (r["warm_spread"] * (r["warm_mib"] or 0) / 2) if r["warm_spread"] else 0
        for r in runs
    ]
    ax.bar(x, wv, color=bar_c, edgecolor="#333", yerr=err, capsize=5, ecolor="#a00")
    ax.set_title("Warm read (read_storm_warm)  — err = 3-rep spread", fontsize=11, fontweight="bold")
    ax.set_ylabel("MiB/s")
    ax.set_xticks(x)
    ax.set_xticklabels(xlabels, fontsize=8)
    for i, r in enumerate(runs):
        if r["warm_mib"] is not None:
            sp = f"  ±{r['warm_spread']*100:.0f}%" if r["warm_spread"] else ""
            ax.text(i, wv[i], f"{r['warm_mib']:.0f}{sp}", ha="center", va="bottom", fontsize=8)

    # 3. jq build: cold vs warm, with the cold-minus-warm fetch overhead called out.
    ax = axes[1, 0]
    w = 0.38
    jc = [r["jq_cold"] or 0 for r in runs]
    jw = [r["jq_warm"] or 0 for r in runs]
    ax.bar(x - w / 2, jc, w, label="cold (fetch+compile)", color="#37c", edgecolor="#333")
    ax.bar(x + w / 2, jw, w, label="warm (compile only)", color="#9cf", edgecolor="#333")
    ax.set_title("jq build wall time — cold vs warm", fontsize=11, fontweight="bold")
    ax.set_ylabel("seconds")
    ax.set_xticks(x)
    ax.set_xticklabels(xlabels, fontsize=8)
    ax.legend(fontsize=8)
    for i, r in enumerate(runs):
        if r["jq_fetch"] is not None:
            ax.text(i, max(jc[i], jw[i]), f"fetch≈{r['jq_fetch']:.1f}s", ha="center", va="bottom", fontsize=8, color="#a00")

    # 4. Overfetch % (promote bytes over the dedup unique set).
    ax = axes[1, 1]
    bars(ax, [r["overfetch_pct"] for r in runs], "Overfetch (promote − unique) / unique", "%", fmt="{:.1f}%")

    fig.tight_layout(rect=(0, 0, 1, 0.96))
    fig.savefig(out, dpi=130)
    print(f"wrote {out}  ({n} runs)")
    for r in runs:
        print(
            f"  {r['label']} {r['inst']:<13} cold={r['cold_mib']} "
            f"warm={r['warm_mib']}(±{(r['warm_spread'] or 0)*100:.0f}%) "
            f"jq_fetch={r['jq_fetch']} honest={r['honest']}"
        )


def _agg(metrics: dict, key: str):
    """Return the AggStats dict for a cold-reps metric, or None."""
    s = metrics.get(key)
    return s if isinstance(s, dict) else None


def _bands_overlap(a: dict, b: dict) -> bool:
    """Do the mean±stderr bands of two AggStats overlap?"""
    lo_a, hi_a = a["mean"] - a["stderr"], a["mean"] + a["stderr"]
    lo_b, hi_b = b["mean"] - b["stderr"], b["mean"] + b["stderr"]
    return not (hi_a < lo_b or hi_b < lo_a)


def plot_cold_reps(files: list[dict], out: str) -> None:
    if not files:
        sys.exit("no fsbench-cold-reps/v1 files found")
    n = len(files)
    x = np.arange(n)
    stems = [f["stem"] for f in files]

    def vals_errs(aggs):
        return (
            [a["mean"] if a else 0 for a in aggs],
            [a["stderr"] if a else 0 for a in aggs],
        )

    fig, axes = plt.subplots(1, 2, figsize=(max(7, 3 * n), 5))
    fig.suptitle(
        "cold jq-build — one bar per input  (bar = mean, error = ±1 stderr)",
        fontsize=13,
        fontweight="bold",
    )

    # 1. jq_build_cold wall time: total + configure split, grouped per file.
    ax = axes[0]
    w = 0.38
    total = [_agg(f["metrics"], "jq_build_cold.total_wall_ms") for f in files]
    conf = [_agg(f["metrics"], "jq_build_cold.configure_wall_ms") for f in files]
    tv, te = vals_errs(total)
    cv, ce = vals_errs(conf)
    ax.bar(x - w / 2, tv, w, yerr=te, capsize=5, label="total", color="#37c", ecolor="#a00")
    ax.bar(x + w / 2, cv, w, yerr=ce, capsize=5, label="configure", color="#9cf", ecolor="#a00")
    ax.set_title("jq_build_cold wall time", fontsize=11, fontweight="bold")
    ax.set_ylabel("ms")
    ax.set_xticks(x)
    ax.set_xticklabels(stems, fontsize=8)
    ax.legend(fontsize=8)
    for i, a in enumerate(total):
        if a:
            ax.text(i - w / 2, a["mean"], f"{a['mean']:.0f}\n±{a['stderr']:.0f}", ha="center", va="bottom", fontsize=7)

    # Headline: do the first two files' total-wall bands overlap?
    if n >= 2 and total[0] and total[1]:
        overlap = _bands_overlap(total[0], total[1])
        msg = (
            "bands OVERLAP — difference not resolved at ±1 stderr"
            if overlap
            else "bands SEPARATE — difference resolved at ±1 stderr"
        )
        ax.text(
            0.5,
            0.97,
            msg,
            transform=ax.transAxes,
            ha="center",
            va="top",
            fontsize=9,
            fontweight="bold",
            color=("#a00" if overlap else "#070"),
        )

    # 2. read_storm_cold throughput, one bar per file.
    ax = axes[1]
    mib = [_agg(f["metrics"], "read_storm_cold.mib_s") for f in files]
    mv, me = vals_errs(mib)
    ax.bar(x, mv, 0.6, yerr=me, capsize=5, color="#2a7", ecolor="#a00", edgecolor="#333")
    ax.set_title("read_storm_cold throughput", fontsize=11, fontweight="bold")
    ax.set_ylabel("MiB/s")
    ax.set_xticks(x)
    ax.set_xticklabels(stems, fontsize=8)
    for i, a in enumerate(mib):
        if a:
            ax.text(i, a["mean"], f"{a['mean']:.1f}\n±{a['stderr']:.1f}", ha="center", va="bottom", fontsize=7)

    fig.tight_layout(rect=(0, 0, 1, 0.93))
    fig.savefig(out, dpi=130)
    print(f"wrote {out}  ({n} cold-reps files)")
    for f, a in zip(files, total):
        if a:
            print(
                f"  {f['stem']:<16} node={f['node']} reps={f['reps_accepted']} "
                f"jq_total={a['mean']:.0f}±{a['stderr']:.0f}ms"
            )
        else:
            print(f"  {f['stem']:<16} node={f['node']} reps={f['reps_accepted']} (no jq_total)")


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Graph fsbench result.json (across runs) or cold-reps.json (before/after error bars).",
    )
    ap.add_argument(
        "inputs",
        nargs="*",
        default=["."],
        help="directories (globbed for result.json) or explicit result.json / cold-reps.json files",
    )
    ap.add_argument("-o", "--out", default="fsbench.png")
    a = ap.parse_args()
    runs, cold_reps = collect(a.inputs)
    if cold_reps:
        plot_cold_reps(cold_reps, a.out)
    elif runs:
        plot(runs, a.out)
    else:
        sys.exit("no fsbench result.json or cold-reps.json files found")


if __name__ == "__main__":
    main()
