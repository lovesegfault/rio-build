# Observability scenario: metrics registration, log pipeline, trace export.
#
# Replaces phase2b's Tempo-based trace validation with opentelemetry-collector
# (file exporter → /var/lib/otelcol/traces.json). Tempo was slow to boot and
# flaky under VM memory pressure — 5+ fix commits in git history. otelcol is
# a single static binary that starts in <1s, and the file exporter gives us
# structured JSON we parse with load_otel_spans() instead of shelling curl
# through Tempo's query API.
#
# DROPPED from phase2b (with rationale):
#
#   Tempo — see above. Same validation (OTLP export works + spans queryable),
#     tighter assertion (we parse every span, not `len(traces) > 0`).
#
#   Cache-hit timing assertion (`assert elapsed < 10.0`) — phase2b itself
#     admits "the REAL signal is the metric below"; VM timing is noisy and
#     protocol-warm already covers cache-hit via rio_scheduler_cache_hits_total
#     delta assertion.
#
# Fixture wiring (caller's responsibility, see nix/tests/default.nix):
#
#   fixture = standalone {
#     workers = {
#       worker1 = { maxBuilds = 1; };
#       worker2 = { maxBuilds = 1; };
#       worker3 = { maxBuilds = 1; };
#     };
#     withOtel = true;
#   };
#
# maxBuilds=1 × 3 workers forces each chain step to a distinct worker (same
# as phase2b). withOtel=true adds otelcol to control + sets RIO_OTEL_ENDPOINT
# on all services (standalone.nix:86-118).
#
# obs.metric.gateway — verify marker at default.nix:vm-observability-standalone
#   EXPECTED_METRICS[(gateway, 9090)] asserts rio_gateway_connections_*,
#   opcodes, handshakes, channels are present in /metrics after a build.
#   metrics-rs only registers on first increment — presence proves both
#   the describe_*! wiring at gateway/lib.rs:25+ AND actual increments.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };
in
pkgs.testers.runNixOSTest {
  name = "rio-observability";
  skipTypeCheck = true;
  # 3 sequential builds (~5s each under VM) + OTLP batch flush interval (~5s)
  # + VM boot overhead. 600s is generous; phase2b was also 600s implicit.
  globalTimeout = 600 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    import re
    import time

    ${common.kvmPreopen}
    start_all()
    ${fixture.waitReady}
    ${common.sshKeySetup gatewayHost}
    ${common.seedBusybox gatewayHost}

    store_url = "ssh-ng://${gatewayHost}"
    workers = [worker1, worker2, worker3]

    def build_chain():
        """Build the A→B→C chain. Captures stderr (2>&1) — the
        PHASE2B-LOG-MARKER lines flow worker→scheduler→gateway→STDERR_NEXT→
        client stderr. The `rio trace_id: <hex>` line also lands here."""
        try:
            return client.succeed(
                f"nix-build --no-out-link --store '{store_url}' "
                "--arg busybox '(builtins.storePath ${common.busybox})' "
                "${drvs.chain} 2>&1"
            )
        except Exception:
            dump_all_logs([${gatewayHost}] + workers)
            raise

    # ══════════════════════════════════════════════════════════════════
    # metrics-registered: every spec'd rio_{component}_* metric appears
    # on its /metrics endpoint after at least one operation has touched
    # it. Not checking values — just that register!()/describe!() fired.
    # Catches: metric renamed in code but not spec, metric behind a
    # feature flag that's off, exporter initialized but registry wrong.
    # ══════════════════════════════════════════════════════════════════

    # Run the chain build FIRST. Counters/histograms only appear after
    # first increment (metrics-rs doesn't pre-register). Gauges appear
    # immediately but better to check everything post-traffic.
    output = build_chain()

    # Port map (nix/modules/*.nix defaults):
    #   gateway=9090  scheduler=9091  store=9092  worker=9093
    # scrape_metrics uses curl localhost — firewall irrelevant.
    #
    # ONLY includes metrics that FIRE during a successful chain build:
    #   - no cache hits (first build) → rio_scheduler_cache_hits_total absent
    #   - no disconnects → rio_scheduler_worker_disconnects_total absent
    #   - inline chunk backend → rio_store_chunk_cache_* absent
    # Those counters exist (grep rio-*/src/lib.rs describe_counter!) but
    # metrics-rs only registers on first increment. Scheduling scenario
    # covers worker_disconnects; protocol-warm covers cache_hits.
    # The gateway entries below (rio_gateway_connections_*, opcodes,
    # handshakes, channels) are the describe_counter!/describe_gauge!
    # registrations at gateway/lib.rs:25+. Presence in /metrics after
    # a successful build proves they're wired AND incremented (metrics-
    # rs only registers on first increment). The per-component loop
    # below asserts each name is present.
    EXPECTED_METRICS = {
        (${gatewayHost}, 9090, "gateway"): [
            "rio_gateway_connections_total",
            "rio_gateway_connections_active",
            "rio_gateway_opcodes_total",
            "rio_gateway_handshakes_total",
            "rio_gateway_channels_active",
        ],
        (${gatewayHost}, 9091, "scheduler"): [
            "rio_scheduler_workers_active",
            "rio_scheduler_builds_total",
            "rio_scheduler_builds_active",
            "rio_scheduler_assignments_total",
            "rio_scheduler_log_lines_forwarded_total",
        ],
        (${gatewayHost}, 9092, "store"): [
            "rio_store_put_path_total",
        ],
    }
    # Worker metrics checked separately below — chain is sequential
    # (A→B→C), scheduler MAY dispatch all 3 to one worker. Find one
    # that actually built. The metrics exist; we just need a worker
    # that incremented them.
    EXPECTED_WORKER_METRICS = [
        "rio_worker_builds_total",
        "rio_worker_builds_active",
        "rio_worker_fuse_cache_misses_total",
        "rio_worker_uploads_total",
    ]

    with subtest("metrics-registered: spec'd metrics present on /metrics"):
        for (node, port, component), names in EXPECTED_METRICS.items():
            scraped = scrape_metrics(node, port)
            present = set(scraped.keys())
            missing = [n for n in names if n not in present]
            assert not missing, (
                f"{component} (:{port}) missing metrics: {missing}\n"
                f"  present rio_{component}_* metrics: "
                f"{sorted(m for m in present if m.startswith(f'rio_{component}_'))}"
            )

        # Find a worker that actually built something, then check it
        # has all expected worker metrics. (Proves the metrics exist
        # in the rio-worker binary; scheduling scenario proves exact
        # per-worker counts.)
        busy_worker = None
        for w in workers:
            scraped = scrape_metrics(w, 9093)
            if "rio_worker_builds_total" in scraped:
                busy_worker = (w, scraped)
                break
        assert busy_worker is not None, (
            "no worker has rio_worker_builds_total — chain build "
            "didn't dispatch anywhere?"
        )
        w, scraped = busy_worker
        present = set(scraped.keys())
        missing = [n for n in EXPECTED_WORKER_METRICS if n not in present]
        assert not missing, (
            f"worker {w.name} (:9093) missing metrics: {missing}\n"
            f"  present rio_worker_* metrics: "
            f"{sorted(m for m in present if m.startswith('rio_worker_'))}"
        )

        # Bloom fill gauge: set every heartbeat (10s). busy_worker
        # inserted ≥1 path into its bloom (busybox FUSE fetch →
        # cache.insert → bloom.insert); by the time the chain
        # completes (~15s of 3 sequential builds) at least one
        # heartbeat fired with a non-empty filter. Far below 0.5
        # — filter sized for 50k items, VM test inserts a handful.
        # obs.metric.bloom-fill-ratio — verify marker at
        # default.nix:vm-observability-standalone.
        fill = scraped.get("rio_worker_bloom_fill_ratio", {}).get("")
        assert fill is not None, (
            f"rio_worker_bloom_fill_ratio gauge missing on {w.name}; "
            f"present rio_worker_* metrics: "
            f"{sorted(m for m in present if m.startswith('rio_worker_'))}"
        )
        assert 0.0 < fill < 0.5, (
            f"unexpected bloom fill ratio on {w.name}: {fill} "
            "(expected >0 after inserts, <<0.5 with 50k-item capacity)"
        )

    # ══════════════════════════════════════════════════════════════════
    # log-pipeline: worker LogBatcher → scheduler → gateway STDERR_NEXT
    # ══════════════════════════════════════════════════════════════════
    #
    # Each chain step echoes `PHASE2B-LOG-MARKER: building <name>` to
    # stderr (chain.nix:30). Flow:
    #   worker LogBatcher → BuildExecution gRPC stream → scheduler recv
    #   task LogBuffers push + ForwardLogBatch → actor resolves drv_path
    #   → interested_builds → emit_build_event(Log) → broadcast → bridge
    #   → gateway → STDERR_NEXT → client.
    #
    # We assert on the PENULTIMATE hop via the metric. The final
    # gateway→client STDERR_NEXT leg depends on the Nix client's
    # rendering (modern Nix filters raw STDERR_NEXT outside activity
    # results). `rio_scheduler_log_lines_forwarded_total` increments
    # inside the actor's ForwardLogBatch handler — ≥3 (one per step)
    # proves the full internal pipeline works. The ring buffer +
    # AdminService.GetBuildLogs give the authoritative log-serving path
    # for the dashboard; the STDERR_NEXT tail is a best-effort
    # convenience whose rendering we don't control.
    #
    # `output` captured above for debugging — not asserted on for
    # markers (would make the test fragile against Nix client changes).

    with subtest("log-pipeline: marker lines reach scheduler actor"):
        # ≥3: one PHASE2B-LOG-MARKER per chain step. Might be more
        # (busybox sh also writes to stderr, chain.nix does `ls -la`).
        assert_metric_ge(
            ${gatewayHost}, 9091,
            "rio_scheduler_log_lines_forwarded_total",
            floor=3,
        )
        _ = output  # captured for debugging; see rationale above

    # ══════════════════════════════════════════════════════════════════
    # trace-export: spans present in otelcol file exporter output
    # ══════════════════════════════════════════════════════════════════
    #
    # OTLP batch exporter flushes on interval (default ~5s). The file
    # exporter writes one ExportTraceServiceRequest JSON per line.
    # load_otel_spans() flattens to [(service_name, trace_id, span)].
    #
    # Stronger than phase2b's grep-through-Tempo-API: we see EVERY span,
    # not just `len(traces) > 0`. The services set here is the exact set
    # of service.name resource attributes that exported — if scheduler's
    # OTLP layer is broken, "scheduler" simply won't appear and the
    # failure message shows what DID export.

    def wait_for_spans(expected_services, timeout=60):
        """Poll load_otel_spans until all expected_services appear.
        OTLP SDK batch exporter flushes every ~5s + file exporter has
        its own buffering; 60s is generous."""
        deadline = time.time() + timeout
        spans, services = [], set()
        while time.time() < deadline:
            spans = load_otel_spans(${gatewayHost})
            services = {svc for svc, _, _ in spans if svc}
            if expected_services <= services:
                return spans, services
            time.sleep(2)
        raise AssertionError(
            f"timed out waiting for services {expected_services - services}; "
            f"have {services} ({len(spans)} spans)"
        )

    with subtest("trace-export: gateway+scheduler+worker spans in otelcol"):
        spans, services = wait_for_spans({"scheduler", "gateway", "worker"})
        assert "scheduler" in services, (
            f"no scheduler spans; services present: {services}"
        )
        assert "gateway" in services, (
            f"no gateway spans; services present: {services}"
        )
        assert "worker" in services, (
            f"no worker spans; services present: {services}"
        )
        print(f"trace-export: {len(spans)} spans across services {services}")

    # ══════════════════════════════════════════════════════════════════
    # trace-id-propagation: STDERR_NEXT trace_id spans scheduler+worker
    # ══════════════════════════════════════════════════════════════════
    #
    # Gateway emits `rio trace_id: <32-hex>` via STDERR_NEXT after
    # SubmitBuild — gives operators a grep handle into the trace
    # backend. The emitted id is the SCHEDULER's trace_id (from the
    # x-rio-trace-id response-metadata header, not the gateway's own
    # span — see r[obs.trace.scheduler-id-in-metadata]). That trace
    # extends through worker via WorkAssignment.traceparent data-carry.
    #
    # link_parent() + #[instrument] produces a LINK, not a parent: the
    # scheduler handler span keeps its own trace_id. Gateway's trace
    # contains only gateway spans; the scheduler's is the useful one.
    # Round-4 validation proved this; option (b) from the phase4b TODO
    # (return scheduler trace_id in response metadata) is now landed.

    with subtest("trace-id-propagation: STDERR_NEXT id spans scheduler+worker"):
        m = re.search(r"rio trace_id: ([0-9a-f]{32})", output)
        assert m, (
            f"expected 'rio trace_id: <32-hex>' in build output; "
            f"first 500 chars: {output[:500]!r}"
        )
        emitted_trace_id = m.group(1).lower()

        # otelcol file exporter writes traceId as no-dash hex —
        # same format as gateway emits. Case-fold to be safe.
        # The emitted id is the SCHEDULER's trace_id (x-rio-trace-id
        # header). The WorkAssignment.traceparent data-carry extends
        # this trace through worker. Assert both services appear.
        # Under KVM the build completes fast enough that spans may not
        # have flushed yet — re-poll the collector file until both
        # services appear in the trace (same pattern as wait_for_spans).
        deadline = time.time() + 30
        services_in_trace = set()
        while time.time() < deadline:
            spans = load_otel_spans(${gatewayHost})
            services_in_trace = {
                svc
                for svc, tid, _ in spans
                if tid and tid.lower() == emitted_trace_id
            }
            if {"scheduler", "worker"} <= services_in_trace:
                break
            time.sleep(2)
        assert "scheduler" in services_in_trace, (
            f"scheduler not in trace {emitted_trace_id}; "
            f"services in trace: {services_in_trace}; "
            f"scheduler trace_ids: "
            f"{sorted({t.lower() for s,t,_ in spans if s=='scheduler' and t})[:5]}"
        )
        assert "worker" in services_in_trace, (
            f"worker not in trace {emitted_trace_id}; "
            f"services in trace: {services_in_trace}; "
            f"worker trace_ids: "
            f"{sorted({t.lower() for s,t,_ in spans if s=='worker' and t})[:5]}"
        )
        print(
            f"trace_id {emitted_trace_id}: spans services {services_in_trace}"
        )

        # ── span_from_traceparent: parenting vs link ────────────────────
        # span_from_traceparent (interceptor.rs:126) is info_span!() THEN
        # set_parent() — the span is created but NOT yet entered when
        # set_parent runs. link_parent (same file) calls set_parent on an
        # ALREADY-ENTERED #[instrument] span and produces a LINK (proven
        # by the unit test at rio-scheduler/src/grpc/tests.rs which checks
        # the trace_id differs). This block OBSERVES whether the
        # not-yet-entered variant produces parenting (same trace_id,
        # worker's parentSpanId in scheduler's spanId set) or a link.
        # The doc text at r[sched.trace.assignment-traceparent] gets
        # tightened based on this observation.
        sched_spans = [
            sp for svc, tid, sp in spans
            if svc == "scheduler" and tid and tid.lower() == emitted_trace_id
        ]
        worker_spans = [
            sp for svc, tid, sp in spans
            if svc == "worker" and tid and tid.lower() == emitted_trace_id
        ]
        assert sched_spans and worker_spans, (
            f"precondition: both services in trace {emitted_trace_id}; "
            f"sched={len(sched_spans)} worker={len(worker_spans)}"
        )
        sched_span_ids = {sp.get("spanId") for sp in sched_spans if sp.get("spanId")}
        worker_parents = {
            sp.get("parentSpanId") for sp in worker_spans if sp.get("parentSpanId")
        }
        overlap = worker_parents & sched_span_ids
        if overlap:
            print(
                "CONFIRMED: span_from_traceparent → PARENTING "
                "(worker parentSpanId in scheduler spanId set; "
                f"overlap={sorted(overlap)[:3]})"
            )
        else:
            print(
                "CONFIRMED: span_from_traceparent → LINK only "
                "(no worker parentSpanId matches any scheduler spanId; "
                f"worker_parents={sorted(worker_parents)[:3]} "
                f"sched_span_ids={sorted(sched_span_ids)[:3]})"
            )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
