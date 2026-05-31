# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
#
# pull-fetcher: hosted by vm-pull-canary-k3s (after the pull-canary
# fragment, which leaves the cluster with no Pools). Fetcher-kind pull
# coverage for the 1c gate ("pool kinds the canary did not cover"):
# a kind=Fetcher Pool builds a fixed-output derivation end-to-end on
# the pull path, and the pod follows the OA3 one-pull default — it
# pulls exactly one assignment, reports it, and exits instead of
# retaining a session.
#
# The FOD needs no network: it is a flat-hash fixed-output derivation
# whose builder writes a known payload (outputHash is the sha256 of
# those bytes), so it routes like any FOD (is_fixed_output →
# effective_features=[fetcher] → only the Fetcher pool covers it)
# while staying buildable in the airgapped fixture.
#
# What this proves (1c gate items, T-1c.2):
#   - the fetcher pool kind runs the pull path: the spawned pod carries
#     RIO_EXECUTOR_KIND=fetcher, the FOD's execution is minted by the
#     pull transaction, and the client gets its store path;
#   - a clean fetcher pull build charges nothing (no drv_attempts row);
#   - OA3 one-pull: exactly one execution is minted for the FOD, and
#     after the report the pod completes (Succeeded) instead of
#     holding a session / pulling further work; the open-attempt view
#     drains to empty.
scope: with scope; ''
  import time

  with subtest("pull-fetcher: fetcher-kind pull builds a FOD end-to-end (OA3 one-pull)"):
      # ── Preconditions: pull-canary cleanup left no Pools ───────────
      for pool_ns in ["${nsBuilders}", "${nsFetchers}"]:
          leftover = k3s_server.succeed(
              f"k3s kubectl -n {pool_ns} get pools --no-headers 2>/dev/null | wc -l"
          ).strip()
          assert leftover == "0", (
              f"pull-fetcher expects no Pools in {pool_ns} before it starts, found {leftover}"
          )

      # ── Fetcher node label ──────────────────────────────────────────
      # The reconciler injects the pool-static
      # nodeSelector{rio.build/fetcher: true} into every kind=Fetcher
      # pod (§13e B4 — the last-resort constraint for builtin FODs), so
      # a node must carry the label or the pod sits Pending forever.
      # Same runtime labeling the fetcher-split scenario does; the full
      # Karpenter label/taint chain is EKS-only.
      kubectl("label node k3s-agent rio.build/fetcher=true --overwrite", ns="kube-system")

      # ── Fetcher pool ────────────────────────────────────────────────
      # hostUsers: true — the fetcher rendering is forced non-privileged
      # (CEL forbids privileged for kind=Fetcher) and k3s containerd
      # does not chown the pod cgroup for user namespaces, so the
      # fixture-wide hostUsers escape hatch applies here explicitly
      # (the helm-rendered pools inherit it from poolDefaults).
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: Pool\n"
          "metadata:\n"
          "  name: pull-fetcher\n"
          "  namespace: ${nsFetchers}\n"
          "spec:\n"
          "  kind: Fetcher\n"
          "  maxConcurrent: 2\n"
          "  systems: [x86_64-linux]\n"
          "  image: rio-builder:dev\n"
          "  imagePullPolicy: Never\n"
          "  hostUsers: true\n"
          "  nodeSelector: null\n"
          "  tolerations: null\n"
          "EOF"
      )

      def pf_count(sql):
          return int(psql_k8s(k3s_server, sql).strip() or "0")

      def pf_open_count(marker):
          """Open attempts for marker (the ListOpenAttempts view)."""
          return pf_count(
              "SELECT count(*) FROM assignments a "
              "JOIN drv_executions e ON e.exec_id = a.exec_id "
              "JOIN derivations d ON d.derivation_id = a.derivation_id "
              "WHERE a.status IN ('pending','acknowledged') "
              f"AND d.drv_path LIKE '%{marker}%' "
              "AND NOT EXISTS (SELECT 1 FROM drv_attempts t "
              " WHERE t.exec_id = a.exec_id "
              " AND t.termination_reason IS NOT NULL)"
          )

      def pf_exec_count(marker):
          """All executions ever minted for marker (every execution row
          is pull-minted — the pull transaction is the only writer)."""
          return pf_count(
              "SELECT count(*) FROM drv_executions e "
              "JOIN assignments a ON a.exec_id = e.exec_id "
              "JOIN derivations d ON d.derivation_id = a.derivation_id "
              f"WHERE d.drv_path LIKE '%{marker}%'"
          )

      def pf_attempt_rows(marker):
          return pf_count(
              "SELECT count(*) FROM drv_attempts a "
              "JOIN derivations d ON d.derivation_id = a.derivation_id "
              f"WHERE d.drv_path LIKE '%{marker}%'"
          )

      # ── Submit the FOD in the background ───────────────────────────
      # Background so the ~15 s build window is observable in the open
      # view / pod env while it runs.
      client.succeed(
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${pcFetcherFod} > /tmp/pull-fetcher.out 2>&1 & "
          "echo $! > /tmp/pull-fetcher.pid"
      )

      # The fetcher pod spawns in the fetchers namespace and pulls the
      # FOD: an open pull-mode attempt appears.
      deadline = time.time() + 300
      while time.time() < deadline:
          if pf_open_count("pc-fetcher-fod") == 1:
              break
          time.sleep(3)
      if pf_open_count("pc-fetcher-fod") != 1:
          # Name the failure: is it spawn (no Job/pod), scheduling
          # (Pending pod), or the pull/client side?
          k3s_server.execute(
              "echo '=== DIAG[pull-fetcher]: no open attempt ===' >&2; "
              "k3s kubectl -n ${nsFetchers} get pools,jobs,pods -o wide >&2 2>&1; "
              "k3s kubectl -n ${nsFetchers} describe pods 2>&1 | tail -40 >&2; "
              "k3s kubectl -n ${nsBuilders} get pods -o wide >&2 2>&1 || true"
          )
          client.execute("echo '=== client nix-build output ===' >&2; tail -40 /tmp/pull-fetcher.out >&2 || true")
      assert pf_open_count("pc-fetcher-fod") == 1, (
          "the fetcher pod should have pulled the FOD (one open pull-mode attempt)"
      )

      # The pod doing the work is a fetcher-kind pod
      # (RIO_EXECUTOR_KIND=fetcher); the retired RIO_DISPATCH_MODE
      # discriminator must NOT be rendered.
      pod = k3s_server.succeed(
          "k3s kubectl -n ${nsFetchers} get pods "
          "-l rio.build/pool=pull-fetcher "
          "-o jsonpath='{.items[0].metadata.name}'"
      ).strip()
      assert pod, "a pull-fetcher pod should exist while the FOD builds"
      pod_env = k3s_server.succeed(
          f"k3s kubectl -n ${nsFetchers} get pod {pod} "
          "-o jsonpath='{.spec.containers[0].env[?(@.name==\"RIO_EXECUTOR_KIND\")].value} "
          "{.spec.containers[0].env[?(@.name==\"RIO_DISPATCH_MODE\")].value}'"
      ).split()
      assert pod_env == ["fetcher"], (
          "the fetcher pod must render RIO_EXECUTOR_KIND=fetcher and no "
          f"RIO_DISPATCH_MODE (the knob is retired), got {pod_env!r}"
      )
      print(f"pull-fetcher: pod {pod} pulled the FOD (kind=fetcher)")

      # ── The report lands and the client gets its store path ────────
      client.wait_until_succeeds(
          "! kill -0 $(cat /tmp/pull-fetcher.pid) 2>/dev/null",
          timeout=300,
      )
      out = client.succeed("cat /tmp/pull-fetcher.out").strip()
      assert "/nix/store/" in out, (
          f"the fetcher-kind pull build should deliver a store path, got: {out!r}"
      )

      # Ledger facts: built on the pull path, charged nothing, exactly
      # one execution ever minted (OA3 one-pull — no re-pull, no second
      # assignment), and the open view drains.
      assert pf_exec_count("pc-fetcher-fod") == 1, (
          "exactly one execution expected for the FOD, got "
          f"{pf_exec_count('pc-fetcher-fod')}"
      )
      assert pf_attempt_rows("pc-fetcher-fod") == 0, (
          "a clean fetcher pull build must charge nothing"
      )
      deadline = time.time() + 120
      while time.time() < deadline and pf_open_count("pc-fetcher-fod") != 0:
          time.sleep(3)
      assert pf_open_count("pc-fetcher-fod") == 0, (
          "the FOD's attempt should close once the report lands"
      )

      # ── OA3 one-pull: the pod completes instead of holding a session.
      # The pull client exits 0 after its single report, so the pod goes
      # Succeeded and the Job completes; a session-retaining pod would
      # stay Running waiting for more work.
      k3s_server.wait_until_succeeds(
          f"test \"$(k3s kubectl -n ${nsFetchers} get pod {pod} "
          "-o jsonpath='{.status.phase}' 2>/dev/null || echo Gone)\" != Running",
          timeout=120,
      )
      phase = k3s_server.succeed(
          f"k3s kubectl -n ${nsFetchers} get pod {pod} "
          "-o jsonpath='{.status.phase}' 2>/dev/null || echo Gone"
      ).strip()
      assert phase in ("Succeeded", "Gone"), (
          "after its one pull+report the fetcher pod must complete "
          f"(or already be GC'd), got phase {phase!r}"
      )
      print(f"pull-fetcher: OA3 one-pull holds (pod phase after report: {phase})")

      # ── Cleanup ────────────────────────────────────────────────────
      kubectl("delete pool pull-fetcher --wait=false", ns="${nsFetchers}")
      kubectl("label node k3s-agent rio.build/fetcher- --overwrite", ns="kube-system")
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsFetchers} get pool pull-fetcher 2>/dev/null",
          timeout=30,
      )
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsFetchers} get pods "
          "-l rio.build/pool=pull-fetcher "
          "--no-headers 2>/dev/null | grep -q .",
          timeout=120,
      )
      print("pull-fetcher PASS: fetcher-kind pull built the FOD, charged nothing, "
            "and the pod exited after its single pull (OA3)")
''
