# Shared assertion helpers for VM testScripts.
#
# Injected at the top of each scenario's testScript via
# `${common.assertions}` interpolation. Runs inside the NixOS test
# driver's Python interpreter — `Machine` objects (control, worker1,
# client, etc.) are in scope as globals.
#
# The point of this module: structurally prevent `grep -E '...[1-9]'`
# as an assertion. Each helper produces a precise failure message with
# actual-vs-expected so VM test failures are debuggable from CI logs
# without needing driverInteractive.

import json
import re


# ── Prometheus text-format scraping ─────────────────────────────────
# The NixOS test driver has no requests/urllib niceties; curl is in
# every control/worker node's systemPackages (common.nix mkControlNode).

_METRIC_LINE = re.compile(r"^(\w+)(\{[^}]*\})?\s+([0-9.eE+-]+|NaN|[+-]Inf)$")


def scrape_metrics(node, port, host="localhost"):
    """Parse a Prometheus /metrics endpoint into {name: {labels: float}}.

    Labels key is the raw `{k="v",...}` string (empty string for
    unlabeled series). Float NaN/Inf preserved via float().
    """
    text = node.succeed(f"curl -sf http://{host}:{port}/metrics")
    out = {}
    for line in text.splitlines():
        if not line or line[0] == "#":
            continue
        m = _METRIC_LINE.match(line)
        if not m:
            continue
        name, labels, val = m.group(1), m.group(2) or "", m.group(3)
        out.setdefault(name, {})[labels] = float(val)
    return out


def metric_value(scraped, name, labels=""):
    """Extract one series from a scrape_metrics() result. None if absent."""
    return scraped.get(name, {}).get(labels)


def assert_metric_exact(node, port, name, expected, labels=""):
    """Assert a metric series exactly equals `expected`.

    Failure message includes ALL series for `name` — usually the bug
    is "wrong label" not "wrong value", and seeing {outcome="failure"}
    when you expected {outcome="success"} is instant diagnosis.
    """
    m = scrape_metrics(node, port)
    actual = m.get(name, {}).get(labels)
    assert actual == expected, (
        f"{name}{labels} = {actual!r}, expected {expected!r}\n"
        f"  all {name} series: {m.get(name, {})!r}"
    )


def assert_metric_delta(before, after, name, expected_delta, labels=""):
    """Baseline-delta assertion on two scrape_metrics() snapshots.

    For counters that may have pre-existing state (e.g., the test
    fixture's own startup traffic). Replaces the phase2c/3a pattern
    of `before = int(grep...)` + `after = int(grep...)` + manual math.
    """
    b = before.get(name, {}).get(labels, 0.0)
    a = after.get(name, {}).get(labels, 0.0)
    delta = a - b
    assert delta == expected_delta, (
        f"{name}{labels} delta = {delta} (before={b}, after={a}), "
        f"expected delta {expected_delta}"
    )


def assert_metric_ge(node, port, name, floor, labels=""):
    """Assert a metric >= floor. For monotonic counters where the
    exact value depends on timing (e.g. fuse_cache_misses)."""
    m = scrape_metrics(node, port)
    actual = m.get(name, {}).get(labels)
    assert actual is not None and actual >= floor, (
        f"{name}{labels} = {actual!r}, expected >= {floor}\n"
        f"  all {name} series: {m.get(name, {})!r}"
    )


# ── Set equality with symmetric-diff failure messages ───────────────
# For willBuild/willSubstitute/references — the `wopQueryMissing`
# bug (`5786f82`) was masked by a test using `.contains()` instead
# of set equality. Extra elements ARE the bug.

def assert_set_eq(actual, expected, context=""):
    a, e = set(actual), set(expected)
    if a == e:
        return
    missing = sorted(e - a)
    extra = sorted(a - e)
    raise AssertionError(
        f"{context}: set mismatch\n"
        f"  missing (expected but not present): {missing!r}\n"
        f"  extra   (present but not expected): {extra!r}"
    )


# ── SQL convenience ─────────────────────────────────────────────────
# Always -qtA: -q suppresses `INSERT 0 1` status, -t tuples-only,
# -A unaligned. psql(control, "SELECT ...") returns one string.

def psql(node, query, db="rio", user="postgres"):
    q = query.replace('"', '\\"')
    return node.succeed(f'sudo -u {user} psql {db} -qtAc "{q}"').strip()


def psql_k8s(kube_node, query, db="rio", user="rio", ns="rio-system",
             pod="rio-postgresql-0"):
    """psql for k3s-full fixture — bitnami PG runs in a pod, not as a
    systemd service. `kubectl exec` into the StatefulSet pod."""
    q = query.replace('"', '\\"')
    return kube_node.succeed(
        f'k3s kubectl -n {ns} exec {pod} -- '
        f'psql -U {user} {db} -qtAc "{q}"'
    ).strip()


# ── Log-dump-on-failure ─────────────────────────────────────────────
# Intended for `except Exception: dump_all_logs(...); raise` in the
# build helper — NixOS test failures otherwise show only the Python
# traceback, not the rio-* journals that explain WHY.

def dump_all_logs(nodes, kube_node=None, kube_namespace="rio-system"):
    for n in nodes:
        n.execute("journalctl -u 'rio-*' --no-pager -n 200 >&2 || true")
    if kube_node is not None:
        kube_node.execute(
            f"k3s kubectl -n {kube_namespace} logs "
            f"-l 'app.kubernetes.io/part-of=rio-build' "
            f"--all-containers --tail=100 --prefix >&2 || true"
        )
        kube_node.execute(
            f"k3s kubectl -n {kube_namespace} get events "
            f"--sort-by=.lastTimestamp | tail -50 >&2 || true"
        )


# ── otelcol trace-file parsing ──────────────────────────────────────
# For scenarios/observability.nix. The file exporter writes
# ExportTraceServiceRequest protos as JSON, one resourceSpans batch
# per line. We flatten to a list of (service_name, trace_id, span).

def load_otel_spans(node, path="/var/lib/otelcol/traces.json"):
    raw = node.succeed(f"cat {path} 2>/dev/null || echo ''")
    spans = []
    for line in raw.splitlines():
        if not line.strip():
            continue
        batch = json.loads(line)
        for rs in batch.get("resourceSpans", []):
            svc = next(
                (a["value"]["stringValue"]
                 for a in rs.get("resource", {}).get("attributes", [])
                 if a.get("key") == "service.name"),
                None,
            )
            for ss in rs.get("scopeSpans", []):
                for sp in ss.get("spans", []):
                    spans.append((svc, sp.get("traceId"), sp))
    return spans
