# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # eio-infra-retry — EIO on an input read is infra, never a poison
  # ══════════════════════════════════════════════════════════════════
  # The derivation's BUILDER is the seeded eio-builder script — a path
  # that is never node-cached, so the daemon's execve is the build's
  # first data fetch. The test takes the store's chunk objects offline
  # (mv of the filesystem chunk-backend directory inside the
  # rio-store-chunks PV — metadata stays, GetDirectory still works, so
  # the mount succeeds and the failure happens exactly at the input
  # read). The kernel's open-for-exec gets EIO from the castore lower,
  # nix-daemon reports `executing '<closure root>': Input/output error`,
  # and the executor must reclassify that MiscFailure as
  # InfrastructureFailure: the scheduler re-queues (status never reaches
  # failed/poisoned) and, once the chunks are restored, a retry
  # completes. State-based fault injection — no timing window at all.
  with subtest("eio-infra-retry: input-read EIO classifies as infra and recovers"):
      assert_not_cached(b3_eio_builder, "eio builder before the build")

      # Locate the store's chunk PV on k3s-server (local-path PVC bound
      # to the rio-store-chunks claim; the filesystem backend keeps its
      # objects under <baseDir>/chunks).
      pv_name = kubectl(
          "get pvc rio-store-chunks -o jsonpath='{.spec.volumeName}'", ns="${nsStore}"
      ).strip()
      pv_path = k3s_server.succeed(
          f"k3s kubectl get pv {pv_name} -o jsonpath='{{.spec.hostPath.path}}'"
      ).strip()
      assert pv_path.startswith("/var/"), (
          f"eio-infra-retry: unexpected rio-store-chunks PV hostPath {pv_path!r}"
      )
      k3s_server.succeed(f"test -d {pv_path}/chunks")
      k3s_server.succeed(f"mv {pv_path}/chunks {pv_path}/chunks-offline")
      print(f"eio-infra-retry: store chunk objects taken offline ({pv_path}/chunks)")

      try:
          drv_eio, build_eio = submit_drv(
              "${eioInfraDrv}",
              extra_args=f"--arg eioBuilder '(builtins.storePath {p_eio_builder})'",
          )
          # First attempt: a pod comes up, the execve EIOs, the executor
          # reports the failure and the pod leaves Running. Pod
          # lifecycle (not log polling) is the "first attempt is over"
          # signal.
          first_pod = castore_pod()
          k3s_server.wait_until_succeeds(
              f"phase=$(k3s kubectl -n ${nsBuilders} get pod {first_pod} "
              "-o jsonpath='{.status.phase}' 2>/dev/null || echo Gone); "
              "test \"$phase\" != Running && test \"$phase\" != Pending",
              timeout=300,
          )
          status_after_fail = drv_status(drv_eio)
          assert status_after_fail not in ("failed", "poisoned", "dependency_failed"), (
              f"eio-infra-retry: derivation went terminally {status_after_fail!r} "
              f"after the input-read EIO — the failure was not classified as "
              f"infrastructure (or the scheduler poisoned it)"
          )
          print(
              f"eio-infra-retry: first attempt over (pod {first_pod} done), drv "
              f"status {status_after_fail!r} (re-queued, not poisoned)"
          )
      finally:
          # Restore the chunk objects no matter what — every later fetch
          # in this split needs them.
          k3s_server.succeed(
              f"if test -d {pv_path}/chunks-offline; then "
              f"rm -rf {pv_path}/chunks && "
              f"mv {pv_path}/chunks-offline {pv_path}/chunks; fi"
          )
          print("eio-infra-retry: store chunk objects restored")

      # Recovery on retry is the strongest proof the failure was
      # retryable: the same derivation must now complete, and the
      # builder script's digest lands in the node cache on the way.
      wait_drv_status(drv_eio, ["completed"], timeout=600, ctx="eio-infra-retry recovery")
      release_watch(build_eio)
      assert_cached(b3_eio_builder, "eio builder after the successful retry", timeout=120)

      # Classification evidence: the executor's reclassified error
      # message ("input materialization failed (I-043/I-178): …
      # executing '<builder>': Input/output error") flows into the
      # scheduler's infrastructure-failure handler log line. One bounded
      # fetch after the fact — the line distinguishes the infra path
      # from a permanent-failure retry, which the PG status alone
      # cannot.
      sched_tail = k3s_server.succeed(
          "k3s kubectl -n ${ns} logs -l app.kubernetes.io/name=rio-scheduler "
          "--tail=20000 --since=20m 2>/dev/null | "
          "grep -c 'input materialization failed' || true"
      ).strip()
      assert int(sched_tail or "0") >= 1, (
          "eio-infra-retry: the scheduler never logged the input-materialization "
          "reclassification for the failed attempt — the EIO was not classified "
          "as an infrastructure failure"
      )
      print("eio-infra-retry PASS: EIO classified as infra, re-queued, recovered")
''
