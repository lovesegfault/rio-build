# lifecycle subtest fragment — composed by scenarios/controller-kwok.nix
# mkTest. Object-state half of ephemeral-pool: proves CEL admission +
# `reconcile_ephemeral` spawns a Job per queued build + spawn_count
# subtracts active. Does NOT wait on build completion (no real kubelet
# under kwok-only — KWOK fakes the Job's pod Ready/Succeeded but never
# runs rio-builder, so the build stays queued at the scheduler).
#
# This fragment is also picked up by lifecycle.nix's `importDir` (every
# .nix in this directory is). It's never SELECTED by any k3s subtests
# list, so its body is Nix-evaluated to a string and discarded. Keep
# Nix-level interpolations limited to `scope` vars that BOTH preludes
# provide (`nsBuilders`, `ephemeralDrv1`, `common`).
scope: with scope; ''
  with subtest("ephemeral-spawn: CEL + status + Job spawned + runaway guard"):
      import json as _json

      kubectl(
          "delete pool x86-64 --ignore-not-found --wait=true",
          ns="${nsBuilders}",
      )
      wait_workers_zero("ephemeral-spawn precondition")

      # ── CEL: hostNetwork without privileged rejected ──────────────
      assert_cel_rejects(
          "hostnet-unprivileged",
          "  hostNetwork: true\n"
          "  maxConcurrent: 4\n"
          "  systems: [x86_64-linux]\n"
          "  image: rio-builder",
          "hostNetwork:true requires privileged:true",
      )

      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: Pool\n"
          "metadata:\n"
          "  name: ephemeral\n"
          "  namespace: ${nsBuilders}\n"
          "spec:\n"
          "  kind: Builder\n"
          "  maxConcurrent: 4\n"
          "  systems: [x86_64-linux]\n"
          "  image: rio-builder:dev\n"
          "  imagePullPolicy: Never\n"
          "  privileged: true\n"
          "  terminationGracePeriodSeconds: 60\n"
          "  nodeSelector: null\n"
          "  tolerations: null\n"
          "EOF"
      )

      # ── No StatefulSet/Service materialised for an ephemeral Pool ─
      import time
      time.sleep(3)
      k3s_server.fail(
          "k3s kubectl -n ${nsBuilders} get sts ephemeral-workers 2>/dev/null"
      )
      k3s_server.fail(
          "k3s kubectl -n ${nsBuilders} get svc ephemeral-workers 2>/dev/null"
      )
      k3s_server.wait_until_succeeds(
          "test \"$(k3s kubectl -n ${nsBuilders} get pool ephemeral "
          "-o jsonpath='{.status.desiredReplicas}')\" = 4",
          timeout=60,
      )

      # ── Job spawned for one queued build ──────────────────────────
      # Background ssh-ng submit — never expected to COMPLETE under
      # KWOK (no real builder). Only needs to push scheduler queued>0
      # so `reconcile_ephemeral` spawns. systemd-run for the same
      # reasons as componentscaler.nix (PID-1 detach, journalctl
      # surfaces eval/ssh errors on timeout).
      client.succeed(
          "systemd-run --unit=ephspawn-build "
          "--setenv=HOME=/root "
          "--setenv=PATH=/run/current-system/sw/bin "
          "nix-build --no-out-link --store 'ssh-ng://k3s-server' "
          "--arg busybox '(builtins.storePath ${common.busybox})' "
          "${ephemeralDrv1}"
      )
      k3s_server.wait_until_succeeds(
          "test -n \"$(k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=ephemeral -o name)\"",
          timeout=45,
      )
      job_count = int(k3s_server.succeed(
          "k3s kubectl -n ${nsBuilders} get jobs "
          "-l rio.build/pool=ephemeral -o name | wc -l"
      ).strip())
      # KWOK's pod-complete Stage marks the Job's pod Succeeded after a
      # short delay → Job Complete → ttlSecondsAfterFinished may reap →
      # next reconcile tick spawns another (build still queued at the
      # scheduler — no real worker ever registered). The runaway guard
      # is therefore "≤ maxConcurrent", not "== 1"; it still catches a
      # spawn_count-doesn't-subtract-active regression (which would
      # spawn 4 + 4 + 4 … unbounded).
      assert job_count <= 4, (
          f"ephemeral Job count {job_count} > maxConcurrent for one "
          "queued drv — spawn_count must subtract active Jobs"
      )

      # ── Intent-deadline stamped on the Job ────────────────────────
      job = _json.loads(kubectl(
          "get jobs -l rio.build/pool=ephemeral -o json", ns="${nsBuilders}"
      ))["items"][0]
      assert job["spec"].get("activeDeadlineSeconds"), (
          "ctrl.ephemeral.intent-deadline: Job.spec.activeDeadlineSeconds unset"
      )

      # ── Cleanup ───────────────────────────────────────────────────
      client.execute("systemctl stop ephspawn-build 2>/dev/null")
      kubectl("delete pool ephemeral --wait=false", ns="${nsBuilders}")
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get pool ephemeral 2>/dev/null",
          timeout=30,
      )
      print("ephemeral-spawn PASS")
''
