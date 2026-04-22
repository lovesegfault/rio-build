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
      # PrefetchComplete on a fresh registration). Since
      # `approx_input_closure` includes inputSrcs, that hint is
      # non-empty even for leaf drvs — every mid-queue
      # re-registration now sends a real PrefetchHint and waits
      # for the ACK round-trip, so fallback fires once per race
      # (~30-40 of ~50 dispatches under load-50drv churn). The
      # test only needs to catch "warm-gate NEVER engages" — i.e.
      # fallback == dispatch count — so bound at the dispatch
      # count, not a hand-tuned fraction of it.
      fallback = ${gatewayHost}.succeed(
          "curl -sf http://localhost:9091/metrics | "
          "grep '^rio_scheduler_warm_gate_fallback_total ' | "
          "awk '{print $2}' || echo 0"
      ).strip() or "0"
      assert float(fallback) < 50, (
          f"warm-gate fallback fired {fallback} times — that's "
          f"the full ~50-dispatch load-50drv set, meaning the "
          f"gate never engages (no PrefetchComplete ever flips "
          f"warm=true). Check scheduler 'warm-gate fallback' "
          f"debug logs and handle_prefetch_complete."
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
