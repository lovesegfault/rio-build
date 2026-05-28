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
#       worker1 = { };
#       worker2 = { };
#       worker3 = { };
#     };
#     withOtel = true;
#   };
#
# 3 workers (one build per pod) forces each chain step to a distinct worker
# (same as phase2b). withOtel=true adds otelcol to control + sets RIO_OTEL_ENDPOINT
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
  # + VM boot overhead. 900s matches sibling scenarios (dashboard, fetcher-
  # split). Was 600s — P0499 hit timeout under keynes concurrent-CI load
  # (no assertion failure, just ran out of clock on OTLP flush wait). P0509.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
      withSeed = true;
    }}

    import re
    import time

    workers = [worker1, worker2, worker3]

    ${common.mkBuildHelperV2 {
      inherit gatewayHost;
      dumpLogsExpr = "dump_all_logs([${gatewayHost}] + workers)";
    }}

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
    #
    # strip_to_store_path=False: `output` is searched for the
    # `rio: build <id> (trace <hex>)` STDERR_NEXT line
    # (trace-id-propagation subtest below). The default strip would
    # discard everything but the store path.
    output = build("${drvs.chain}", strip_to_store_path=False)

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
            # Pull-era fleet observable (set every establishment-sweep
            # tick, so present even at 0); replaces the stream-era
            # rio_scheduler_workers_active registration gauge here —
            # standalone workers deliver over the pull path since the
            # T-1c.2b re-point and never register.
            "rio_scheduler_open_attempts",
            "rio_scheduler_builds_total",
            "rio_scheduler_builds_active",
            # Incremented per delivered assignment on BOTH paths (the
            # pull transaction increments it too).
            "rio_scheduler_assignments_total",
        ],
        (${gatewayHost}, 9092, "store"): [
            "rio_store_put_path_total",
            # Build-log data plane: the builder streams log batches to
            # the store's LogService.AppendLog (not to the scheduler).
            # Presence after the chain build proves the ingest path is
            # wired end-to-end; the floor assertion below proves the
            # marker lines actually arrived.
            "rio_store_log_ingest_lines_total",
        ],
    }
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

        # Worker metrics: rio-builder is one-shot per intent on the
        # pull path (the spawner execs it per assignment; it exits
        # after its report). Between builds no rio-builder process is
        # running, so neither per-name presence nor endpoint-up is
        # testable after the fact. The journald count below proves the
        # builder processes ran, completed, and exercised their
        # increment paths (the exporter is wired by the same
        # bootstrap() call that emits these log lines); endpoint
        # exposure on a live builder stays covered by the k8s
        # readiness probes against :9193 health and by unit tests.
        total_builds = sum(journal_builds_succeeded(w) for w in workers)
        assert total_builds >= 3, (
            f"chain A→B→C should produce ≥3 successful builds across "
            f"workers; journald shows {total_builds}"
        )

    # ══════════════════════════════════════════════════════════════════
    # log-pipeline: worker LogBatcher → rio-store LogService.AppendLog
    # ══════════════════════════════════════════════════════════════════
    #
    # Each chain step echoes `PHASE2B-LOG-MARKER: building <name>` to
    # stderr (chain.nix:30). Flow (post log-data-plane cutover):
    #   worker LogBatcher → per-build LogUploader → AppendLog gRPC
    #   stream → rio-store ingest (accept + buffer + chunk cut to the
    #   chunk backend + drv_log_chunks manifest row). The live tail the
    #   gateway relays to the nix client comes from the store's TailLog,
    #   not from the scheduler — the scheduler never sees a log line.
    #
    # We assert on the store-side ingest metric.
    # `rio_store_log_ingest_lines_total` increments when the store
    # ACCEPTS a line from a builder's AppendLog stream — ≥3 (one marker
    # per chain step) proves the builder→store data plane works end to
    # end with real builds, real assignment tokens, and real network.
    # The TailLog read-back path is covered by vm-log-service-standalone
    # (synthetic ingest, restart survival, cross-session dedup) and the
    # gateway's unit tests (the relay).
    #
    # `output` captured above for debugging — not asserted on for
    # markers (would make the test fragile against Nix client changes).

    with subtest("log-pipeline: marker lines reach the store's AppendLog ingest"):
        # ≥3: one PHASE2B-LOG-MARKER per chain step. Might be more
        # (busybox sh also writes to stderr, chain.nix does `ls -la`).
        assert_metric_ge(
            ${gatewayHost}, 9092,
            "rio_store_log_ingest_lines_total",
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
        spans, services = wait_for_spans({"scheduler", "gateway", "builder"})
        assert "scheduler" in services, (
            f"no scheduler spans; services present: {services}"
        )
        assert "gateway" in services, (
            f"no gateway spans; services present: {services}"
        )
        assert "builder" in services, (
            f"no worker spans; services present: {services}"
        )
        print(f"trace-export: {len(spans)} spans across services {services}")

    # ══════════════════════════════════════════════════════════════════
    # trace-id-propagation: STDERR_NEXT trace_id spans scheduler+worker
    # ══════════════════════════════════════════════════════════════════
    #
    # Gateway emits `rio: build <id> (trace <32-hex>)` via STDERR_NEXT
    # after SubmitBuild — the build_id is the user-facing handle for
    # the dashboard / `rio-cli builds` / cancellation; the trace suffix
    # gives operators a grep handle into the trace backend (appended
    # only when OTel is wired — these VM nodes have it). The emitted
    # trace id is the SCHEDULER's (from the x-rio-trace-id
    # response-metadata header, not the gateway's own span — see
    # r[obs.trace.scheduler-id-in-metadata]). That trace extends
    # through worker via WorkAssignment.traceparent data-carry.
    #
    # link_parent() + #[instrument] produces a LINK, not a parent: the
    # scheduler handler span keeps its own trace_id. Gateway's trace
    # contains only gateway spans; the scheduler's is the useful one.
    # Round-4 validation proved this; option (b) from the phase4b TODO
    # (return scheduler trace_id in response metadata) is now landed.

    with subtest("trace-id-propagation: STDERR_NEXT id spans scheduler+worker"):
        m = re.search(r"rio: build \S+ \(trace ([0-9a-f]{32})\)", output)
        assert m, (
            f"expected 'rio: build <id> (trace <32-hex>)' in build output; "
            f"first 500 chars: {output[:500]!r}"
        )
        emitted_trace_id = m.group(1).lower()

        # otelcol file exporter writes traceId as no-dash hex —
        # same format as gateway emits. Case-fold to be safe.
        # The emitted id is the SCHEDULER's trace_id (x-rio-trace-id
        # header). The WorkAssignment.traceparent data-carry extends
        # this trace through worker. Assert both services appear.
        # spawn_monitored creates a CHILD span (not re-entering the
        # parent), so the scheduler's #[instrument] SubmitBuild span
        # closes when the handler returns — not when the bridge task
        # ends. The scheduler span should flush within one OTEL batch
        # cycle (~5s). The worker span still needs the build to finish
        # (3-drv chain ~60s under KVM) + batch-flush. 120s covers
        # chain + flush + slop; could be tightened once worker-side
        # span timing is characterized.
        deadline = time.time() + 120
        services_in_trace = set()
        while time.time() < deadline:
            spans = load_otel_spans(${gatewayHost})
            services_in_trace = {
                svc
                for svc, tid, _ in spans
                if tid and tid.lower() == emitted_trace_id
            }
            if {"scheduler", "builder"} <= services_in_trace:
                break
            time.sleep(2)
        assert "scheduler" in services_in_trace, (
            f"scheduler not in trace {emitted_trace_id}; "
            f"services in trace: {services_in_trace}; "
            f"scheduler trace_ids: "
            f"{sorted({t.lower() for s,t,_ in spans if s=='scheduler' and t})[:5]}"
        )
        assert "builder" in services_in_trace, (
            f"worker not in trace {emitted_trace_id}; "
            f"services in trace: {services_in_trace}; "
            f"worker trace_ids: "
            f"{sorted({t.lower() for s,t,_ in spans if s=='builder' and t})[:5]}"
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
            if svc == "builder" and tid and tid.lower() == emitted_trace_id
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
        # r[sched.trace.assignment-traceparent] — span_from_traceparent
        # produces PARENTING: set_parent() runs before first enter;
        # tracing-opentelemetry allocates the OTel span lazily on enter,
        # so the parent context is available. Worker parentSpanId should
        # match a scheduler spanId. (Regression guard: was observe-only
        # print; committed to assert after the mechanism was confirmed.)
        assert overlap, (
            "span_from_traceparent → expected PARENTING but no overlap; "
            f"worker_parents={sorted(worker_parents)[:3]} "
            f"sched_span_ids={sorted(sched_span_ids)[:3]}"
        )
        print(
            "CONFIRMED: span_from_traceparent → PARENTING "
            "(worker parentSpanId in scheduler spanId set; "
            f"overlap={sorted(overlap)[:3]})"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
