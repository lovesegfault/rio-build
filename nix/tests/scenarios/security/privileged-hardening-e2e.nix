# security.privileged-hardening-e2e — base_runtime_spec + cgroup rw-remount e2e
#
# Split out of scenarios/security.nix; see that file's header for the
# r[verify ...] marker placement (default.nix:vm-security-nonpriv-k3s).
{
  pkgs,
  common,
  drvs,
}:
{ fixture }:
let
  inherit (fixture) ns nsStore nsBuilders;

  # One trivial build to prove FUSE works end-to-end. Distinct
  # marker (no DAG-dedup with any other scenario's drvs).
  nonprivDrv = drvs.mkTrivial { marker = "sec-nonpriv-e2e"; };
  # Ephemeral workers: long-sleep warmup so the pod-introspection
  # subtests (nonpriv-admitted, cgroup-remount) have a Running pod
  # to inspect. Outlives those subtests; never awaited.
  warmupDrv = drvs.mkTrivial {
    marker = "sec-nonpriv-warmup";
    sleepSecs = 300;
  };
in
pkgs.testers.runNixOSTest {
  name = "rio-security-nonpriv";
  skipTypeCheck = true;

  # k3s bring-up ~3-4min + worker Ready ~3min + one build ~30s +
  # kubectl exec probes. The node-status capacity patch in
  # waitReady is synchronous (~1s) so no extra bring-up tax over
  # the privileged fast-path.
  globalTimeout = 900 + common.covTimeoutHeadroom;

  inherit (fixture) nodes;

  testScript = ''
    ${common.assertions}

    ${common.kvmCheck}
    start_all()
    ${fixture.waitReady}
    ${fixture.kubectlHelpers}

    # Ephemeral workers: spawn one before the pod-introspection
    # subtests below. sshKeySetup + seed + warmup build → wait_worker_pod.
    ${fixture.sshKeySetup}
    ${common.seedBusybox "k3s-server"}
    ${common.mkBuildHelperV2 {
      gatewayHost = "k3s-server";
      dumpLogsExpr = ''dump_all_logs([], kube_node=k3s_server, kube_namespace="${ns}")'';
    }}
    client.succeed(
        "nohup nix-build --no-out-link --store ssh-ng://k3s-server "
        "--arg busybox '(builtins.storePath ${common.busybox})' "
        "${warmupDrv} >/tmp/warmup.log 2>&1 &"
    )
    wp = wait_worker_pod(timeout=270)
    print(f"nonpriv: warmup spawned worker pod {wp}")

    ${
      # ── PSA restricted: control-plane namespaces enforce + pods admitted ──
      # sec.psa.control-plane-restricted — verify marker at default.nix:
      # vm-security-nonpriv-k3s. rio-system + rio-store enforce PSA
      # restricted; all four control-plane pods pass admission with
      # runAsNonRoot, drop-ALL, seccomp:RuntimeDefault, readOnlyRoot
      # (templates/_helpers.tpl rio.podSecurityContext). A root probe
      # pod to rio-system MUST be rejected.
      #
      # Skipped under coverage mode: k3s-full.nix overrides namespaces.
      # {system,store}.psa=privileged so the hostPath profraw volume is
      # admitted (hostPath → restricted rejection). The securityContext
      # helpers also self-guard on NOT coverage (hostPath not subject
      # to fsGroup → UID-65532 profraw writes would EACCES).
      #
      # Nix ''...'' dedenting strips leading whitespace to the minimum
      # common indent — interpolating into `with subtest(...):` drops
      # the body to col-0 (IndentationError). Each branch carries its
      # OWN `with subtest` header so the dedented block is self-
      # consistent Python.
      if common.coverage then
        ''
          with subtest("psa-restricted: rio-system + rio-store enforce restricted"):
              print(
                  "psa-restricted SKIP: coverage mode overrides "
                  "namespaces.{system,store}.psa=privileged for "
                  "hostPath profraw volume (k3s-full.nix "
                  "optionalAttrs coverage)"
              )
        ''
      else
        ''
          with subtest("psa-restricted: rio-system + rio-store enforce restricted"):
              # 1. Namespace PSA enforce label = restricted
              for ns_name in ("${ns}", "${nsStore}"):
                  psa = k3s_server.succeed(
                      f"k3s kubectl get ns {ns_name} "
                      "-o jsonpath="
                      "'{.metadata.labels.pod-security\\.kubernetes\\.io/enforce}'"
                  ).strip()
                  assert psa == "restricted", (
                      f"namespace {ns_name} should enforce PSA "
                      f"restricted (values.yaml namespaces.*.psa); "
                      f"got: {psa!r}"
                  )

              # 2. Control-plane pods Running (proves securityContext
              #    satisfies restricted admission). waitReady already
              #    waited for these; this re-asserts with explicit
              #    securityContext introspection.
              for depl, ns_name in [
                  ("rio-scheduler", "${ns}"),
                  ("rio-gateway", "${ns}"),
                  ("rio-controller", "${ns}"),
                  ("rio-store", "${nsStore}"),
              ]:
                  pod_json = k3s_server.succeed(
                      f"k3s kubectl -n {ns_name} get pod "
                      f"-l app.kubernetes.io/name={depl} "
                      "-o jsonpath='{.items[0].spec.securityContext}'"
                  )
                  sc = json.loads(pod_json or "{}")
                  assert sc.get("runAsNonRoot") is True, (
                      f"{depl} pod should set runAsNonRoot:true "
                      f"(rio.podSecurityContext helper); got "
                      f"securityContext={sc!r}"
                  )
                  assert sc.get("runAsUser") == 65532, (
                      f"{depl} pod should set runAsUser:65532 "
                      f"(distroless nonroot UID); got {sc!r}"
                  )

              # 3. Root probe pod → rejected by admission.
              #    kubectl run without --override defaults to root
              #    (no runAsNonRoot, no seccomp) → PSA restricted
              #    admission rejects with "violates PodSecurity".
              #    execute() not succeed() — we EXPECT nonzero exit.
              rc, out = k3s_server.execute(
                  "k3s kubectl -n ${ns} run root-probe "
                  "--image=busybox --restart=Never "
                  "-- /bin/true 2>&1"
              )
              assert rc != 0, (
                  f"root probe pod should be REJECTED by PSA "
                  f"restricted admission; kubectl run succeeded "
                  f"(rc=0). Output: {out!r}"
              )
              assert "violates PodSecurity" in out and "restricted" in out, (
                  f"rejection should cite PSA restricted; got "
                  f"rc={rc} out={out!r}"
              )
              print(
                  "psa-restricted PASS: ${ns}+${nsStore} enforce "
                  "restricted, 4 control-plane pods admitted with "
                  f"runAsNonRoot, root probe rejected: {out.strip()[:120]!r}"
              )
        ''
    }

    # ── Worker pod security posture: non-privileged admitted ────────
    # waitReady already proved the x86-64 builder pool reconciled.
    # Fetch the live pod spec and assert hostUsers:false + privileged
    # absent/false. If privileged:true leaked through (helm layering
    # miss, null vs false semantics), the DS check above might still
    # pass (DS runs regardless) but THIS fails — the load-bearing half.
    with subtest("nonpriv-admitted: privileged:false rendered and admitted"):
        pod_json = kubectl(f"get pod {wp} -o json", ns="${nsBuilders}")
        pod = json.loads(pod_json)

        # hostUsers: vmtest-full-nonpriv.yaml sets hostUsers:true
        # (OPT OUT of userns) because k3s's containerd with systemd
        # cgroup driver doesn't chown the pod cgroup to the userns-
        # mapped root UID → worker mkdir /sys/fs/cgroup/leaf fails
        # EACCES → CrashLoopBackOff. See the hostUsers comment in
        # vmtest-full-nonpriv.yaml. hostUsers:false verification
        # stays at builders.rs unit test (renders-shape) + EKS
        # smoke test (containerd 2.0+ delegation).
        host_users = pod["spec"].get("hostUsers")
        assert host_users is not False, (
            f"expected hostUsers unset or true (k3s opt-out), got "
            f"{host_users!r}. If False: vmtest-full-nonpriv.yaml "
            f"hostUsers:true override didn't propagate → worker "
            f"will CrashLoop on cgroup mkdir EACCES."
        )

        # privileged absent or false on the worker container. The
        # controller's build_container() only sets privileged:true
        # when the Pool spec has it; false → field omitted.
        sc = pod["spec"]["containers"][0].get("securityContext", {})
        assert not sc.get("privileged", False), (
            f"worker container still privileged: securityContext={sc}"
        )

        # seccompProfile: vmtest-full-nonpriv inherits the chart
        # default (Localhost), which the controller renders as
        # POD-level RuntimeDefault + CONTAINER-level Localhost
        # (security.md worker.seccomp.localhost-profile). The pod
        # sandbox doesn't need pivot_root; enforcement is on the
        # worker container.
        pod_sc = pod["spec"].get("securityContext", {})
        seccomp = pod_sc.get("seccompProfile", {})
        assert seccomp.get("type") == "RuntimeDefault", (
            f"expected pod-level seccompProfile.type=RuntimeDefault, "
            f"got {seccomp!r}"
        )
        worker_seccomp = sc.get("seccompProfile", {})
        assert worker_seccomp.get("type") == "Localhost", (
            f"expected container-level seccompProfile.type=Localhost, "
            f"got {worker_seccomp!r}"
        )
        assert worker_seccomp.get("localhostProfile") == "operator/rio-builder.json", (
            f"unexpected localhostProfile path: {worker_seccomp!r}"
        )

        # procMount NOT set — k8s PSA rejects procMount:Unmasked
        # when hostUsers:true (KEP-4265). Worker remounts /proc
        # fresh in its pre_exec instead (executor/daemon/spawn.rs)
        # to bypass containerd's /proc masking for nix-daemon's
        # mountAndPidNamespacesSupported() check.

        # No dev-fuse hostPath volume — base_runtime_spec injects
        # /dev/fuse directly (the load-bearing mechanism this
        # scenario exists to prove; see fuse-e2e subtest below for
        # the works-end-to-end half).
        vols = [v["name"] for v in pod["spec"].get("volumes", [])]
        assert "dev-fuse" not in vols, (
            f"non-privileged pod should NOT have hostPath /dev/fuse "
            f"volume (base_runtime_spec injects it); got volumes={vols!r}"
        )
        print(
            f"nonpriv-admitted PASS: privileged absent, "
            f"seccomp=Localhost, no hostPath /dev/fuse, "
            f"hostUsers={host_users!r} (k3s opt-out)"
        )

    # ── cgroup rw-remount succeeded ─────────────────────────────────
    # containerd mounts /sys/fs/cgroup RO for non-privileged pods
    # even with CAP_SYS_ADMIN. delegated_root() does
    # MS_REMOUNT|MS_BIND to clear the per-mount-point RO flag BEFORE
    # mkdir /sys/fs/cgroup/leaf + writing subtree_control. Under
    # privileged:true containerd mounts rw already → remount is a
    # no-op. This is the first time the remount is load-bearing in
    # any VM test.
    #
    # Proof: /sys/fs/cgroup/leaf exists inside the worker container
    # (mkdir would EROFS without the remount), and subtree_control
    # has controllers enabled (write would EROFS without the remount).
    with subtest("cgroup-remount: /sys/fs/cgroup writable + /leaf created"):
        # rio-builder image has no shell/coreutils — `kubectl exec -- test`
        # / `cat` fail with "executable file not found in $PATH".
        # Proof via two host-side signals instead:
        #
        # 1. Worker log line — delegated_root() emits "moved self
        #    into leaf sub-cgroup" after mkdir+move succeeds. If
        #    the remount failed (EROFS) or mkdir failed (EACCES
        #    under userns), the worker crashes BEFORE this log
        #    line → CrashLoopBackOff → we never reach this subtest.
        log = kubectl(f"logs {wp}", ns="${nsBuilders}")
        assert "moved self into leaf sub-cgroup" in log, (
            "worker log should show 'moved self into leaf sub-"
            "cgroup' (delegated_root() success signal). Log tail: "
            f"{log[-800:]}"
        )
        # 2. Node-side cgroup hierarchy — the worker's container
        #    cgroup on the HOST has a leaf/ subdir AFTER the
        #    worker moved itself. Find it via the containerID from
        #    pod status. Path: /sys/fs/cgroup/kubepods.slice/.../
        #    cri-containerd-<id>.scope/leaf. Checking on k3s-agent
        #    (where the pod scheduled — see describe Node field).
        cid = kubectl(
            f"get pod {wp} "
            "-o jsonpath='{.status.containerStatuses[0].containerID}'",
            ns="${nsBuilders}",
        ).strip().removeprefix("containerd://")
        assert cid, "no containerID yet — pod not started?"
        # Pod may schedule on EITHER node (topology spread is
        # soft). Check both; the find on the wrong node returns
        # empty and xargs -r skips. cgroup.subtree_control: the
        # worker writes "+memory +cpu ..." after the remount. If
        # the write silently failed (RO), `memory` is absent.
        find_scope = (
            "find /sys/fs/cgroup/kubepods.slice "
            f"-name 'cri-containerd-{cid}.scope' -type d 2>/dev/null"
        )
        subtree = ""
        for node in [k3s_agent, k3s_server]:
            out = node.execute(
                f"{find_scope} | head -1 | "
                "xargs -r -I{} cat {}/cgroup.subtree_control"
            )[1].strip()
            if out:
                subtree = out
                # Verify /leaf subdir exists on the same node.
                node.succeed(
                    f"{find_scope} | head -1 | "
                    "xargs -I{} test -d {}/leaf"
                )
                break
        assert "memory" in subtree, (
            f"container cgroup.subtree_control should have 'memory' "
            f"(enable_subtree_controllers wrote it post-remount); "
            f"got: {subtree!r} — cgroup scope not found on either "
            f"node? cid={cid}"
        )
        print(
            f"cgroup-remount PASS: worker log shows leaf move, "
            f"host-side leaf/ exists, subtree_control={subtree!r}"
        )

    # ── base_runtime_spec passthrough: our entries reach runc ───────
    # nix/base-runtime-spec.nix sets exactly 2 linux.devices nodes
    # (fuse+kvm) and 2 linux.resources.devices cgroup-allow rules.
    # `crictl inspect` shows the OCI spec containerd hands to runc
    # — with base_runtime_spec, that's our base spec MODIFIED by
    # CRI for container-specifics (process.args/env/cwd/mounts) but
    # NOT for default devices: CRI's WithDefaultUnixDevices does
    # not run when a base spec is loaded. runc's libcontainer
    # (specconv AllowedDevices + CreateCgroupConfig) then adds
    # /dev/{null,zero,full,tty,urandom,random} nodes AND the
    # deny-all + standard-dev cgroup allows BELOW the OCI-spec
    # layer — invisible here, proven functionally by build-
    # completes below (build would fail without /dev/null).
    #
    # What this gate locks in: a containerd bump that makes CRI
    # DROP the base spec's device entries (e.g. a spec opt that
    # resets linux.devices/resources.devices instead of leaving
    # them) would surface here as fuse missing — before the
    # 270s+ build-timeout it'd otherwise take to notice.
    # `cid` is reused from cgroup-remount above.
    with subtest("base-runtime-spec-passthrough: fuse/kvm reach the OCI spec handed to runc"):
        node_name = kubectl(
            f"get pod {wp} -o jsonpath='{{.spec.nodeName}}'",
            ns="${nsBuilders}",
        ).strip()
        node = {"k3s-server": k3s_server, "k3s-agent": k3s_agent}[node_name]
        spec = json.loads(node.succeed(
            f"k3s crictl inspect {cid} | "
            "${pkgs.jq}/bin/jq -c '.info.runtimeSpec.linux | "
            "{devs: [.devices[].path], resdevs: .resources.devices}'"
        ))
        devs, resdevs = spec["devs"], spec["resdevs"]
        # /dev/kvm is host-conditional: the k3s ExecStartPre picks
        # the withKvm spec variant iff `test -c /dev/kvm` on the
        # node. CI VM tests run with nested KVM (the NixOS test
        # driver passes -enable-kvm and the guest kernel loads
        # kvm_intel/kvm_amd), so host_has_kvm is normally True —
        # the iff check guards both directions if that ever changes.
        host_has_kvm = node.succeed("test -c /dev/kvm && echo y || echo n").strip() == "y"
        assert "/dev/fuse" in devs, (
            f"linux.devices={devs!r} — base_runtime_spec /dev/fuse "
            f"missing from OCI spec handed to runc. "
            f"CRI spec opt reset the list? See nix/base-runtime-spec.nix."
        )
        assert ("/dev/kvm" in devs) == host_has_kvm, (
            f"linux.devices={devs!r} host_has_kvm={host_has_kvm} — "
            f"/dev/kvm presence in OCI spec must match host. "
            f"pick-base-runtime-spec ExecStartPre symlinked wrong "
            f"variant? See nix/tests/fixtures/k3s-full.nix."
        )
        assert any(
            r.get("allow") and r.get("major") == 10 and r.get("minor") == 229
            for r in resdevs
        ), (
            f"linux.resources.devices={resdevs!r} — fuse cgroup "
            f"allow (10:229) missing from OCI spec. CRI spec opt "
            f"reset the list?"
        )
        print(
            f"base-runtime-spec-passthrough PASS: linux.devices="
            f"{sorted(devs)!r} host_has_kvm={host_has_kvm}, "
            f"resources.devices={len(resdevs)} entries "
            f"(runc-libcontainer adds /dev/null etc. + "
            f"deny-all post-OCI-spec — not visible here, proven "
            f"by build-completes)"
        )

    # ── Build completes: FUSE works via base_runtime_spec ───────────
    # The FUSE mount is the overlay lower layer. If base_runtime_spec
    # injection failed (containerd didn't mknod /dev/fuse inside the
    # container + add it to the device cgroup), fuser::mount2 fails →
    # worker never
    # registers → waitReady's workers_active=1 wait already timed
    # out. Reaching here IS implicit proof; the build is end-to-end
    # confirmation (overlay mount + nix-daemon unshare + cgroup per-
    # build tree all work under the non-privileged security context).
    with subtest("build-completes: full nonpriv path end-to-end"):
        out_path = build("${nonprivDrv}")
        assert out_path.startswith("/nix/store/"), (
            f"nonpriv build should succeed; got: {out_path!r}"
        )
        assert "rio-test-sec-nonpriv-e2e" in out_path, (
            f"wrong drv output name: {out_path!r}"
        )
        print(
            f"build-completes PASS: nonpriv e2e build output {out_path}"
        )

    ${common.collectCoverage fixture.pyNodeVars}
  '';
}
