# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # passthrough-small — ≤threshold input, two opens, zero read upcalls
  # ══════════════════════════════════════════════════════════════════
  # small_1m (1 MiB < 8 MiB threshold) is read twice: the first open is
  # a whole-file miss (fetch + promote), the second a hit — both must
  # reply passthrough and neither may produce a read upcall. miss_stream
  # must stay at zero (≤threshold never streams).
  #
  # The script attempts `echo poison > /var/rio/cache/ab/test` after the
  # two opens; if that WRITE SUCCEEDS it exits 9 — before the warm_4k
  # sentinel read. The pod's volumeMounts are captured here (while the
  # pod still exists) for the cache-readonly subtest.
  #
  # Gate: warm_4k is read AFTER both small_1m opens AND the poison
  # write attempt, and this is its first read on the node — its
  # appearance in /var/rio/cache is the host-visible "both opens
  # happened and the write attempt did not succeed" signal.
  with subtest("passthrough-small: two opens of a small input, both passthrough"):
      drv_pt, build_pt = submit_drv(
          "${ptSmallDrv}",
          extra_args=(
              f"--arg small1m '(builtins.storePath {p_small1m})' "
              f"--arg warm4k '(builtins.storePath {p_warm4k})'"
          ),
      )
      pt_small_pod = castore_pod()
      assert_cached(b3_warm4k, "warm_4k sentinel (passthrough-small gate)", timeout=300)

      m = worker_metrics(pt_small_pod)
      n_passthrough = series(m, "rio_builder_castore_fuse_open_mode_total", must=('mode="passthrough"',))
      n_read_upcalls = series(m, "rio_builder_castore_fuse_upcalls_total", must=('op="read"',))
      n_stream = series(m, "rio_builder_castore_fuse_open_case_total", must=('case="miss_stream"',))
      n_small = series(m, "rio_builder_castore_fuse_open_case_total", must=('case="miss_small"',))
      assert n_passthrough >= 2, (
          f"passthrough-small: only {n_passthrough} passthrough opens (expected the "
          f"two small_1m opens at minimum); open_mode: "
          f"{fam(m, 'rio_builder_castore_fuse_open_mode_total')!r}"
      )
      assert n_read_upcalls == 0, (
          f"passthrough-small: {n_read_upcalls} read upcalls — passthrough is not "
          f"engaging; upcalls: {fam(m, 'rio_builder_castore_fuse_upcalls_total')!r}"
      )
      assert n_stream == 0, (
          f"passthrough-small: a ≤threshold input took the streaming path; "
          f"open_case: {fam(m, 'rio_builder_castore_fuse_open_case_total')!r}"
      )
      assert n_small >= 1, (
          f"passthrough-small: no whole-file miss recorded (small_1m's first read "
          f"should be one); open_case: "
          f"{fam(m, 'rio_builder_castore_fuse_open_case_total')!r}"
      )

      # Capture the pod's volume-mount shape for cache-readonly while
      # the pod still exists.
      pt_small_mounts = kubectl(
          f"get pod {pt_small_pod} -o jsonpath="
          "'{range .spec.containers[0].volumeMounts[*]}{.name}={.mountPath}:ro={.readOnly} {end}'",
          ns="${nsBuilders}",
      )

      finish_async(build_pt, "passthrough-small")
      print("passthrough-small PASS: both opens passthrough, zero read upcalls")
''
