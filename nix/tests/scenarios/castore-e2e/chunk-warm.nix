# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # chunk-warm — faults-split baseline build
  # ══════════════════════════════════════════════════════════════════
  # Streams big_f clean (canary that the fixture is sane before any
  # fault is injected) and pre-warms small_1m + warm_4k so
  # mountd-restart's post-restart opens are cache hits by construction.
  # Leaves /var/rio/chunks populated with big_f's chunks — the inventory
  # integrity-fail corrupts. Carries no markers.
  with subtest("chunk-warm: clean streaming baseline + cache pre-warm"):
      drv_cw, build_cw = submit_drv(
          "${chunkWarmDrv}",
          extra_args=(
              f"--arg bigF '(builtins.storePath {p_big_f})' "
              f"--arg small1m '(builtins.storePath {p_small1m})' "
              f"--arg warm4k '(builtins.storePath {p_warm4k})'"
          ),
      )
      pod = castore_pod()
      wait_worker_metric(
          pod, "grep -q 'case=\"miss_stream\"'", timeout=300, ctx="chunk-warm miss_stream"
      )
      assert_cached(b3_big_f, "big_f after the clean stream", timeout=300)
      assert_cached(b3_small1m, "small_1m pre-warm", timeout=120)
      assert_cached(b3_warm4k, "warm_4k pre-warm", timeout=120)
      k3s_agent.succeed("find /var/rio/chunks -type f -print -quit | grep -q .")
      n_chunks = int(k3s_agent.succeed("find /var/rio/chunks -type f | wc -l").strip())
      print(f"chunk-warm: node chunk cache holds {n_chunks} chunks")

      finish_async(build_cw, "chunk-warm")
      print("chunk-warm PASS: baseline stream + pre-warm complete")
''
