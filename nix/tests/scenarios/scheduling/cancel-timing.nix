# scheduling subtest fragment — composed by scenarios/scheduling.nix mkTest.
scope: with scope; ''
  import time as _time

  # ══════════════════════════════════════════════════════════════════
  # cancel-timing — gRPC CancelBuild mid-exec → cgroup gone within 5s
  # ══════════════════════════════════════════════════════════════════
  # Same cancel chain as lifecycle.nix:cancel-cgroup-kill but on the
  # standalone fixture (no k3s, plaintext gRPC :9001) and a TIGHTER
  # budget: 5s vs 30s. The 5s budget < 10s prom scrape interval —
  # cgroup-gone MUST be asserted via direct `test` on the worker VM,
  # NOT via a prometheus scrape. A `derivations_running==0` prom
  # check would race the scrape and flake.
  #
  # Flow: CancelBuild RPC → scheduler handle_cancel_build →
  # cancel_signals_total++ → CancelSignal over worker stream →
  # runtime.rs try_cancel_build → fs::write(cgroup.kill, "1") →
  # kernel SIGKILLs tree → executor drain → BuildCgroup::Drop rmdirs.
  # lifecycle.nix observed <1.5s; 5s is comfortable on local VMs.
  #
  # SubmitBuild via gRPC, NOT ssh-ng://: ssh-ng doesn't surface
  # build_id to the nix client. And client-disconnect mid-
  # wopBuildDerivation does NOT trigger CancelBuild — session.rs:107's
  # EOF-cancel path fires only BETWEEN opcodes; mid-opcode the build
  # handler removes the id before bubbling (handler/build.rs:462), so
  # active_build_ids is empty by the time the outer loop could see it.
  with subtest("cancel-timing: CancelBuild → cgroup gone within 5s"):
      cancel_before = scrape_metrics(${gatewayHost}, 9091)

      drv_path, build_id = submit_single_drv("${cancelDrv}")
      print(f"cancel-timing: submitted, build_id={build_id}")

      # Wait for cgroup — THIS IS the phase=Building signal (daemon
      # spawned, cgroup created, sleep started). sanitize_build_id
      # (executor/mod.rs:973) = basename with . → _, so the cgroup
      # name ends "-sched-cancel-timing_drv".
      #
      # cancelDrv can land on ANY worker. Probe all; first hit
      # wins. DelegateSubgroup=builds puts per-build cgroups as
      # SIBLINGS of builds/ under the service cgroup (worker.nix:180).
      # `find -print -quit` stops at first match; `| grep .` fails on
      # empty output (find exits 0 on no-match) so the Python-side rc
      # check works.
      cgroup_parent = "/sys/fs/cgroup/system.slice/rio-builder.service"
      assigned = None
      cgroup_path = None
      for _ in range(30):
          for w in all_workers:
              rc, out = w.execute(
                  f"find {cgroup_parent} -maxdepth 1 -type d "
                  f"-name '*sched-cancel-timing_drv' -print -quit "
                  f"2>/dev/null | grep ."
              )
              if rc == 0 and out.strip():
                  assigned = w
                  cgroup_path = out.strip()
                  break
          if assigned:
              break
          _time.sleep(1)
      assert assigned is not None, (
          "no worker created a cgroup for cancelDrv within 30s"
      )
      print(f"cancel-timing: assigned={assigned.name}, cgroup={cgroup_path}")

      # Precondition: cgroup.procs NON-EMPTY. If empty, the build
      # isn't actually running in the cgroup and the 5s gone-assert
      # below proves nothing (could vanish for any reason). Without
      # this the test would pass on a broken add_process().
      procs_before = int(assigned.succeed(
          f"wc -l < {cgroup_path}/cgroup.procs"
      ).strip())
      assert procs_before > 0, (
          f"cgroup.procs empty at {cgroup_path} — build not running "
          f"in the cgroup? cancel-gone assertion would be vacuous."
      )

      # Cancel. Clock the end-to-end latency.
      t0 = _time.monotonic()
      cancel_payload = json.dumps({
          "buildId": build_id,
          "reason": "vm-test-cancel-timing",
      })
      ${gatewayHost}.succeed(
          f"grpcurl -plaintext "
          f"-protoset ${protoset}/rio.protoset "
          f"-d '{cancel_payload}' "
          f"localhost:9001 rio.scheduler.SchedulerService/CancelBuild"
      )

      # PRIMARY: cgroup GONE within 5s — DIRECT probe, NOT prom.
      # Kernel rejects rmdir on non-empty cgroup → gone ⇒ procs
      # emptied ⇒ kill landed. 300s sleep hasn't completed (elapsed
      # < 5s ≪ 300s) so removal PROVES the cancel chain ran.
      try:
          assigned.wait_until_succeeds(
              f"! test -e {cgroup_path}",
              timeout=5,
          )
      except Exception:
          procs_after = assigned.execute(
              f"cat {cgroup_path}/cgroup.procs 2>/dev/null | wc -l"
          )[1].strip()
          dump_all_logs([${gatewayHost}] + all_workers)
          print(f"cancel-timing DIAG: procs_after={procs_after} "
                f"(was {procs_before}), build_id={build_id}")
          raise
      elapsed = _time.monotonic() - t0
      print(f"cancel-timing: cgroup gone in {elapsed:.2f}s "
            f"(budget 5s, sleep was 300s)")

      # Worker logged the kill path (runtime.rs:238). No kubelet
      # http2-stream flake here (standalone journald, not k8s).
      assigned.succeed(
          "journalctl -u rio-builder --no-pager | "
          "grep 'build cancelled via cgroup.kill'"
      )

      # cancel_signals_total delta ≥1. Monotone counter — post-hoc
      # scrape is fine. The 5s<10s caveat is about tight-window
      # gauge assertions, not counters read after the fact.
      cancel_after = scrape_metrics(${gatewayHost}, 9091)
      c_before = metric_value(cancel_before,
          "rio_scheduler_cancel_signals_total") or 0.0
      c_after = metric_value(cancel_after,
          "rio_scheduler_cancel_signals_total") or 0.0
      assert c_after >= c_before + 1, (
          f"CancelBuild should increment cancel_signals_total by >=1; "
          f"before={c_before}, after={c_after}"
      )
''
