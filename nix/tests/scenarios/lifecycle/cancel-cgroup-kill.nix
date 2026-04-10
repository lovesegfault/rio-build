# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # cancel-cgroup-kill — gRPC CancelBuild mid-exec → cgroup.kill="1"
  # ══════════════════════════════════════════════════════════════════
  # cgroup.rs:180 kill() writes "1" to cgroup.kill → kernel SIGKILLs
  # every PID in the tree. NO prior test cancels a RUNNING build —
  # recovery kills the scheduler (build keeps running on the worker).
  #
  # P0294: the Build CR is gone. Cancel via gRPC CancelBuild
  # directly (the SAME RPC the old CR finalizer called). Flow:
  # CancelBuild RPC → scheduler dispatch Cancel to worker →
  # runtime.rs try_cancel_build → cgroup::kill_cgroup →
  # fs::write(cgroup.kill, "1"). Log signal: "build cancelled via
  # cgroup.kill" at runtime.rs:197.
  #
  # Submission is ALSO via gRPC (SubmitBuild, not ssh-ng://) so we
  # get the build_id back for CancelBuild. The ssh-ng:// path goes
  # through the gateway's Nix worker protocol — no build_id is
  # surfaced to the nix client. P0289's build-timeout port inherits
  # this gRPC-direct pattern.
  with subtest("cancel-cgroup-kill: gRPC CancelBuild mid-exec → cgroup.kill"):
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

      # CancelBuild via gRPC — the replacement for "delete Build CR →
      # finalizer → CancelBuild". sched_grpc handles port-forward +
      # mTLS + protoset. Unary RPC, returns CancelBuildResponse.
      cancel_resp = sched_grpc(
          json.dumps({"buildId": build_id, "reason": "vm-test-cancel"}),
          "rio.scheduler.SchedulerService/CancelBuild",
      )
      print(f"cancel-cgroup-kill: CancelBuild → {cancel_resp.strip()!r}")

      # PRIMARY assertion: cgroup REMOVED within 30s. The 60s sleep
      # hasn't completed — so removal proves the cancel chain ran:
      # scheduler dispatch Cancel → worker try_cancel_build →
      # cgroup.kill="1" → kernel SIGKILLs procs → BuildCgroup::Drop
      # rmdirs. Kernel rejects rmdir on non-empty cgroup, so gone ⇒
      # procs emptied. Pre-P0294 observed: gone in <1.5s.
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
              timeout=30,
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

      print("cancel-cgroup-kill PASS: cgroup rmdir'd in <30s "
            "(sleep was 60s ⇒ killed not completed)")
''
