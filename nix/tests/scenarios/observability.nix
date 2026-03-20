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

  # Coverage mode: graceful-stop + profraw tar + copy_from_vm. Additive so
  # normal-mode CI budget is unchanged.
  covTimeoutHeadroom = if common.coverage then 300 else 0;
in
pkgs.testers.runNixOSTest {
  name = "rio-observability";
  skipTypeCheck = true;
  # 3 sequential builds (~5s each under VM) + OTLP batch flush interval (~5s)
  # + VM boot overhead. 600s is generous; phase2b was also 600s implicit.
  globalTimeout = 600 + covTimeoutHeadroom;

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

    with subtest("trace-export: scheduler + gateway spans in otelcol"):
        spans, services = wait_for_spans({"scheduler", "gateway"})
        assert "scheduler" in services, (
            f"no scheduler spans; services present: {services}"
        )
        assert "gateway" in services, (
            f"no gateway spans; services present: {services}"
        )
        print(f"trace-export: {len(spans)} spans across services {services}")

    # ══════════════════════════════════════════════════════════════════
    # trace-id-propagation: STDERR_NEXT trace_id is real + queryable
    # ══════════════════════════════════════════════════════════════════
    #
    # Gateway emits `rio trace_id: <32-hex>` via STDERR_NEXT after
    # SubmitBuild — gives operators a grep handle into the trace
    # backend. The emitted id must appear in at least one gateway span
    # in the collector file (proves the id is the gateway's actual
    # trace, not a fabricated hex string).
    #
    # TODO(phase4b): round-4 validation proved that the gateway trace
    # does NOT include scheduler/worker spans — link_parent() calls
    # tracing::Span::current().set_parent() but the #[instrument] span
    # was already created at function entry, so the scheduler handler
    # span is an orphan with its own trace_id (LINKED to the gateway
    # trace but not a child). The data-carry fix (round 3) makes
    # scheduler→worker work, but gateway→scheduler was never actually
    # parented. The previous ≥3 span-count assertion was satisfied by
    # gateway alone (it emits many spans per ssh-ng session). Fix:
    # either (a) create handler spans manually AFTER extracting
    # traceparent (not #[instrument] + link_parent), or (b) return
    # scheduler trace_id in SubmitBuildResponse so gateway emits THAT
    # in STDERR_NEXT. Unit test at actor/tests/dispatch.rs:227
    # verifies the scheduler→worker data-carry in isolation.
    #
    # Assertion stays GATEWAY-ONLY until this is fixed.

    with subtest("trace-id-propagation: STDERR_NEXT id matches a gateway span"):
        m = re.search(r"rio trace_id: ([0-9a-f]{32})", output)
        assert m, (
            f"expected 'rio trace_id: <32-hex>' in build output; "
            f"first 500 chars: {output[:500]!r}"
        )
        emitted_trace_id = m.group(1)

        # otelcol file exporter writes traceId as no-dash hex —
        # same format as gateway emits. Case-fold to be safe.
        # Under KVM the build completes fast enough that the gateway's
        # SubmitBuild span may not have flushed yet — re-poll the
        # collector file until the emitted id appears (same pattern
        # as wait_for_spans).
        deadline = time.time() + 30
        gateway_trace_ids = set()
        while time.time() < deadline:
            spans = load_otel_spans(${gatewayHost})
            gateway_trace_ids = {
                tid.lower()
                for svc, tid, _ in spans
                if svc == "gateway" and tid
            }
            if emitted_trace_id.lower() in gateway_trace_ids:
                break
            time.sleep(2)
        assert emitted_trace_id.lower() in gateway_trace_ids, (
            f"emitted trace_id {emitted_trace_id} not in gateway spans; "
            f"gateway has {len(gateway_trace_ids)} distinct trace_ids: "
            f"{sorted(gateway_trace_ids)[:5]}..."
        )
        print(
            f"trace_id {emitted_trace_id}: present in gateway spans "
            f"(scheduler/worker linkage: see TODO(phase4b) above)"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
