# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # warm-gate — PrefetchHint ACK opens the warm-gate; fallback unused
  # ══════════════════════════════════════════════════════════════════
  # All 3 workers register at boot with an EMPTY ready queue (no
  # build submitted yet) → on_worker_registered short-circuits, flips
  # warm=true immediately. Every best_worker() call during later
  # fragments finds warm candidates → fallback never fires.
  #
  # The per-assignment PrefetchHint (dispatch.rs:342) still fires on
  # every non-leaf assignment. The worker fetches + ACKs
  # PrefetchComplete → scheduler records rio_scheduler_warm_prefetch_
  # paths. By this fragment (placed AFTER fanout/chunks/load-50drv),
  # at least one parent-with-children has dispatched → histogram
  # populated.
  #
  # This is a post-hoc observability check — doesn't submit its own
  # build. Cheap (~0s), lives in the disrupt split after load-50drv.
  with subtest("warm-gate: PrefetchComplete recorded; fallback bounded"):
      # One-shot workers re-register mid-queue during load-50drv,
      # so the cold-fallback CAN fire (became-idle dispatch races
      # PrefetchComplete on a fresh registration). The old
      # invariant (fallback==0, "all workers register with empty
      # queue at boot") no longer holds. Bound it loosely: ≪ the
      # number of dispatches (~50) — a value near 50 would mean
      # the warm-gate never engages.
      fallback = ${gatewayHost}.succeed(
          "curl -sf http://localhost:9091/metrics | "
          "grep '^rio_scheduler_warm_gate_fallback_total ' | "
          "awk '{print $2}' || echo 0"
      ).strip() or "0"
      assert float(fallback) < 10, (
          f"warm-gate fallback fired {fallback} times — expected "
          f"a handful at most under one-shot churn. Warm-gate "
          f"never engaging? Check scheduler 'warm-gate fallback' "
          f"debug logs."
      )

      # PrefetchComplete histogram: the count suffix exists and is
      # ≥1. At least one per-assignment hint went out (fanout's
      # collector depends on 4 leaves → approx_input_closure non-
      # empty → hint sent → worker ACKs). If 0 or absent, the
      # worker→scheduler PrefetchComplete plumbing is broken.
      hist_count = ${gatewayHost}.succeed(
          "curl -sf http://localhost:9091/metrics | "
          "grep '^rio_scheduler_warm_prefetch_paths_count ' | "
          "awk '{print $2}' || echo 0"
      ).strip()
      assert hist_count and float(hist_count) >= 1, (
          f"expected ≥1 PrefetchComplete recorded, got "
          f"rio_scheduler_warm_prefetch_paths_count={hist_count!r}. "
          f"Worker handle_prefetch_hint → PrefetchComplete → "
          f"scheduler handle_prefetch_complete chain broken?"
      )
      print(f"warm-gate: fallback={fallback}, prefetch_complete_count={hist_count}")
''
