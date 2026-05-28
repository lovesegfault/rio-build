# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # cancel-cgroup-kill — gRPC CancelBuild mid-exec → cgroup.kill="1"
  # ══════════════════════════════════════════════════════════════════
  # cgroup.rs:180 kill() writes "1" to cgroup.kill → kernel SIGKILLs
  # every PID in the tree. NO prior test in this group cancels a
  # RUNNING build — recovery kills the scheduler (build keeps running
  # on the worker).
  #
  # Pull path (T-1c.2b re-point — the chart pool dispatches pull):
  # CancelBuild RPC → scheduler closes the attempt + marks the drv
  # cancelled → controller sees the closed-while-active edge on its
  # next reconcile tick and foreground-deletes the owning Job →
  # kubelet SIGTERMs the pod → builder's pull abort path
  # (runtime/pull.rs build_phase_with_abort) calls try_cancel_build →
  # cgroup::kill_cgroup → fs::write(cgroup.kill, "1"). Same builder
  # kill mechanism as the stream era; the delivery hop changed from
  # the in-stream Cancel dispatch to the controller Job deletion.
  # Distinct from the pull-canary cancel arm: this runs under the
  # default vmtest-full values (3600s probe deadline), proving the
  # chain does not depend on the canary overlay's 180s deadline.
  #
  # Submission is ALSO via gRPC (SubmitBuild, not ssh-ng://) so we
  # get the build_id back for CancelBuild. The ssh-ng:// path goes
  # through the gateway's Nix worker protocol — no build_id is
  # surfaced to the nix client. P0289's build-timeout port inherits
  # this gRPC-direct pattern.
  with subtest("cancel-cgroup-kill: gRPC CancelBuild mid-exec → cgroup.kill"):
      import time
      drv_path, build_id = submit_single_drv("${cancelDrv}")
      print(f"cancel-cgroup-kill: submitted, build_id={build_id}")

      # Wait for the build's cgroup to appear — this IS the
      # "phase=Building" signal (daemon spawned, cgroup created,
      # sleep started). sanitize_build_id(drv_path) = basename with
      # . → _, so the cgroup dir contains "lifecycle-cancel_drv".
      #
      # Worker pod is DISTROLESS — no `find`/`wc`/`test` via `kubectl
      # exec`. Probe from the VM host instead: the worker's cgroup-ns
      # scopes its OWN /sys/fs/cgroup view, but the HOST sees the full
      # kubepods.slice/... tree. Same leaf dirname, longer path.
      # Resolve which node the pod is on (the Job pod may schedule
      # to either).
      #
      # timeout=120: dispatch-lag variance (flannel subnet race
      # observed 2026-03-16 delaying worker pod start).
      wp = wait_worker_pod()
      worker_node = k3s_server.succeed(
          f"k3s kubectl -n ${nsBuilders} get pod {wp} "
          "-o jsonpath='{.spec.nodeName}'"
      ).strip()
      worker_vm = k3s_agent if worker_node == "k3s-agent" else k3s_server
      # -print -quit stops after first match (no `| head` SIGPIPE).
      # `grep .` makes the command fail when find emits nothing (find
      # itself exits 0 on no-match), so wait_until_succeeds retries.
      cgroup_path = worker_vm.wait_until_succeeds(
          "find /sys/fs/cgroup -type d -name '*lifecycle-cancel_drv' "
          "-print -quit 2>/dev/null | grep .",
          timeout=180,
      ).strip()
      procs_before = int(worker_vm.succeed(
          f"wc -l < {cgroup_path}/cgroup.procs"
      ).strip())
      assert procs_before > 0, (
          f"cgroup.procs empty ({cgroup_path}) — build not actually "
          f"running in the cgroup?"
      )
      print(f"cancel-cgroup-kill: node={worker_node}, cgroup={cgroup_path}, "
            f"procs={procs_before}")

      # The controller cancels only on the closed-while-active edge of
      # an attempt it has previously OBSERVED open (ListOpenAttempts
      # is read once per ~10s reconcile tick) — give it two ticks of
      # open evidence before the verdict, same as the pull-canary
      # cancel arm. The 180s sleeper leaves ample margin.
      time.sleep(20)

      # CancelBuild via gRPC — the replacement for "delete Build CR →
      # finalizer → CancelBuild". sched_grpc handles port-forward +
      # protoset. Unary RPC, returns CancelBuildResponse.
      cancel_resp = sched_grpc(
          json.dumps({"buildId": build_id, "reason": "vm-test-cancel"}),
          "rio.scheduler.SchedulerService/CancelBuild",
      )
      print(f"cancel-cgroup-kill: CancelBuild → {cancel_resp.strip()!r}")

      # PRIMARY assertion: cgroup REMOVED within the AD5 composite
      # bound (90s = scheduler verdict + ≤1 controller tick +
      # foreground Job delete + SIGTERM abort), the same bound the
      # pull-canary cancel arm asserts. The sleeper is 180s and the
      # observation pause above consumed ~22s, so a cgroup gone
      # inside this window can only mean the cancel chain killed the
      # build — the sleep cannot have finished on its own. Kernel
      # rejects rmdir on non-empty cgroup, so gone ⇒ procs emptied.
      #
      # NOT checking `kubectl logs | grep 'cancelled via cgroup.kill'`:
      # the polling itself triggers kubelet "Failed when writing line
      # to log file, err=http2: stream closed" on the worker's log
      # file (runs 6+7 only, ~4-5s cadence from grep-poll start; runs
      # 4+5 never reached here). Worker emits the line (runtime.rs:197)
      # but kubelet's containerd log-read stream is disrupted under
      # TCG — not persisted to /var/log/pods/.../worker/0.log. Not a
      # rio bug; the cgroup-gone speed is conclusive.
      try:
          worker_vm.wait_until_succeeds(
              f"! test -e {cgroup_path}",
              timeout=90,
          )
      except Exception:
          procs_after = worker_vm.succeed(
              f"cat {cgroup_path}/cgroup.procs 2>/dev/null | wc -l || echo gone"
          ).strip()
          k3s_server.execute(
              "echo '=== DIAG: worker logs (non-DEBUG, last 2m) ===' >&2; "
              f"k3s kubectl -n ${nsBuilders} logs {wp} --since=2m "
              "  | grep -vE '\"level\":\"DEBUG\"' | tail -40 >&2 || true; "
              "echo '=== DIAG: scheduler leader logs (cancel dispatch) ===' >&2; "
              "leader=$(k3s kubectl -n ${ns} get lease rio-scheduler-leader "
              "  -o jsonpath='{.spec.holderIdentity}') && "
              "k3s kubectl -n ${ns} logs $leader --since=2m "
              "  | grep -iE 'cancel' >&2 || true"
          )
          print(f"cancel-cgroup-kill DIAG: procs_after={procs_after} "
                f"(was {procs_before}), build_id={build_id}")
          raise

      # Structural backstop for the timing argument above: the drv
      # must have ended cancelled, never completed. If the cancel
      # chain had silently failed and the sleep somehow finished, the
      # report would have completed the drv and this trips loudly.
      cancel_status = psql_k8s(k3s_server,
          "SELECT status FROM derivations "
          "WHERE drv_path LIKE '%lifecycle-cancel%'"
      ).strip()
      assert cancel_status == "cancelled", (
          f"the cancelled build must end status='cancelled' "
          f"(killed, not completed), got {cancel_status!r}"
      )

      print("cancel-cgroup-kill PASS: cgroup rmdir'd inside the 90s "
            "composite bound and the drv ended cancelled "
            "(sleep was 180s ⇒ killed not completed)")
''
