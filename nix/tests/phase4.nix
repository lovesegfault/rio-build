# Phase 4a milestone validation: multi-tenancy foundation.
#
# Section A (this file, 4a): tenant resolution smoke.
#   SSH key comment → gateway captures tenant NAME → scheduler
#   resolves to UUID via `tenants` table → `builds.tenant_id` row
#   has the resolved UUID. Unknown tenant → InvalidArgument.
#   Empty comment → single-tenant mode (tenant_id IS NULL).
#
# Sections B–J appended in 4b/4c (see docs/src/phases/phase4.md
# section map): GC references, rate-limit, maxSilentTime, rio-cli,
# cancel-timing, NetPol, WorkerPoolSet, security, load scenario.
#
# Topology (3 VMs, k3s from the start — avoids a 4c refactor when
# Sections G (NetPol) / H (WorkerPoolSet) need it):
#   control — PG + store + scheduler + gateway (systemd, PLAINTEXT)
#   k8s     — k3s + rio-controller POD (Helm-rendered) + worker-as-pod.
#             Controller-as-pod exercises the production ClusterRole +
#             Deployment template in CI (controller-as-systemd used the
#             admin kubeconfig, bypassing RBAC). See nix/helm-render.nix.
#   client  — nix client, ssh-ng to control's gateway
#
# PLAINTEXT (no mTLS/HMAC) for Section A: tenant resolution doesn't
# depend on TLS, and the WorkerPool CRD uses `tlsSecretName` (a k8s
# Secret mount), not direct env vars. Wiring TLS for the worker pod
# is a separate concern that Section I (4c security: mTLS no-cert
# rejection) will add via a k8s Secret.
#
# Three SSH keys with different comments on the client; all three
# written to control's authorized_keys. The gateway captures the
# MATCHED entry's comment as the tenant name.
#
# Run:
#   NIXBUILDNET_REUSE_BUILD_FAILURES=false nix-build-remote --no-nom -- -L .#checks.x86_64-linux.vm-phase4
{
  pkgs,
  rio-workspace,
  rioModules,
  dockerImages, # for airgap preload into k3s
  nixhelm, # helm-render.nix fetches PG subchart (helm template presence check)
  system,
  coverage ? false,
  # crds arg still passed via k3sArgs (phase3a/3b use it). phase4 ignores
  # it — helmRendered includes CRDs (00-crds.yaml split).
  ...
}:
let
  common = import ./common.nix {
    inherit
      pkgs
      rio-workspace
      rioModules
      coverage
      ;
  };

  helmRender = import ../helm-render.nix { inherit pkgs nixhelm system; };

  # ── Test derivation ─────────────────────────────────────────────────
  # Three distinct derivations (trivially different output) so each
  # build creates a fresh `builds` row instead of DAG-dedup reusing
  # the first build's result.
  mkTestDrv =
    marker:
    pkgs.writeText "phase4-drv-${marker}.nix" ''
      { busybox }:
      derivation {
        name = "phase4-${marker}";
        builder = "''${busybox}/bin/sh";
        args = [ "-c" "echo phase4-${marker} > $out" ];
        system = "x86_64-linux";
      }
    '';
  testDrvKnown = mkTestDrv "known";
  testDrvUnknown = mkTestDrv "unknown";
  testDrvAnon = mkTestDrv "anon";

  # ── WorkerPool CR ───────────────────────────────────────────────────
  # Same as phase3a. Applied via testScript AFTER airgap image import.
  workerPoolCR = {
    apiVersion = "rio.build/v1alpha1";
    kind = "WorkerPool";
    metadata = {
      name = "default";
      namespace = "default";
    };
    spec = {
      replicas = {
        min = 1;
        max = 1;
      };
      autoscaling = {
        metric = "queueDepth";
        targetValue = 2;
      };
      image = "rio-worker:dev";
      maxConcurrentBuilds = 1;
      fuseCacheSize = "5Gi";
      sizeClass = "";
      systems = [ "x86_64-linux" ];
      features = [ ];
      privileged = true;
      hostNetwork = true;
      imagePullPolicy = "IfNotPresent";
    };
  };
  workerPoolCRFile = pkgs.writeText "workerpool.json" (builtins.toJSON workerPoolCR);

in
pkgs.testers.runNixOSTest {
  name = "rio-phase4";
  # k3s startup (~60s) + airgap import (~30s) + worker pod ready (~90s)
  # + 3 build cases (~30s each). Wide margin for CI jitter.
  globalTimeout = 900;

  nodes = {
    control = common.mkControlNode {
      hostName = "control";
      memorySize = 2048;
      extraFirewallPorts = [
        9091
        9092
      ];
      extraSchedulerConfig = {
        tickIntervalSecs = 2;
        lease = {
          name = "rio-scheduler-lease";
          namespace = "default";
          kubeconfigPath = "/etc/kube/config";
        };
      };
      extraPackages = [ pkgs.postgresql ];
    };

    k8s = common.mkK3sNode {
      # Helm-rendered: controller as a pod. The chart's vmtest values
      # profile enables ONLY the controller (hostNetwork=true so
      # `control` hostname resolves via node's /etc/hosts).
      helmRendered = helmRender {
        valuesFile = ../../infra/helm/rio-build/values/vmtest.yaml;
        extraSet = {
          "controller.schedulerAddr" = "control:9001";
          "controller.storeAddr" = "control:9002";
          "controller.extraEnv[0].name" = "RIO_LOG_FORMAT";
          "controller.extraEnv[0].value" = "pretty";
        }
        // pkgs.lib.optionalAttrs coverage {
          "controller.coverage.enabled" = "true";
          "controller.extraEnv[1].name" = "LLVM_PROFILE_FILE";
          "controller.extraEnv[1].value" = "/var/lib/rio/cov/rio-%p-%m.profraw";
        };
      };
      # Controller is now a pod → its image must be preloaded too.
      extraK3sImages = [
        dockerImages.worker
        dockerImages.controller
      ];
    };

    client = common.mkClientNode { gatewayHost = "control"; };
  };

  testScript = ''
    start_all()
    ${common.waitForControlPlane "control"}

    # ── k3s bootstrap (same sequence as phase3a) ──────────────────────
    k8s.wait_for_unit("k3s.service")
    k8s.wait_for_file("/etc/rancher/k3s/k3s.yaml")
    # Airgap race fix (commit 51dde5a): k3s starts before images
    # imported → pods schedule before pause container available →
    # ErrImagePull. Wait for the pause image explicitly.
    k8s.wait_until_succeeds("k3s ctr images ls -q | grep -q pause", timeout=120)

    # Kubeconfig copy (127.0.0.1 → k8s) + scheduler restart → lease
    kubeconfig = k8s.succeed("cat /etc/rancher/k3s/k3s.yaml").replace("127.0.0.1", "k8s")
    control.succeed("mkdir -p /etc/kube")
    control.succeed("cat > /etc/kube/config << 'KUBEEOF'\n" + kubeconfig + "\nKUBEEOF")
    control.succeed("chmod 600 /etc/kube/config")
    control.succeed("systemctl restart rio-scheduler")
    control.wait_for_unit("rio-scheduler.service")
    control.wait_for_open_port(9001)

    k8s.wait_until_succeeds(
        "k3s kubectl get lease rio-scheduler-lease -n default "
        "-o jsonpath='{.spec.holderIdentity}' | grep -q control", timeout=30)

    # ── Bootstrap SSH (creates id_ed25519 with empty comment) ─────────
    # seedBusybox uses `nix copy --to 'ssh-ng://control'` which relies
    # on the client's default IdentityFile (id_ed25519). sshKeySetup
    # creates it with -C empty (single-tenant mode → no rejection).
    ${common.sshKeySetup "control"}

    # ── THREE additional SSH keys with different comments ─────────────
    # For the tenant test cases. All three + id_ed25519 go in
    # authorized_keys. Gateway matches by key_data, reads the MATCHED
    # entry's comment.
    client.succeed("ssh-keygen -t ed25519 -N ''' -C 'team-test' -f /root/.ssh/id_team_test")
    client.succeed("ssh-keygen -t ed25519 -N ''' -C 'unknown-team' -f /root/.ssh/id_unknown")
    client.succeed("ssh-keygen -t ed25519 -N ''' -C ''' -f /root/.ssh/id_anon")

    # All four to authorized_keys (id_ed25519 already there from
    # sshKeySetup; append the three tenant keys).
    tenant_keys = client.succeed(
        "cat /root/.ssh/id_team_test.pub /root/.ssh/id_unknown.pub /root/.ssh/id_anon.pub"
    )
    control.succeed(f"cat >> /var/lib/rio/gateway/authorized_keys << 'EOF'\n{tenant_keys}\nEOF")
    control.succeed("systemctl restart rio-gateway")
    control.wait_for_unit("rio-gateway.service")
    control.wait_for_open_port(2222)

    # ── Worker pod ready (phase3a sequence) ───────────────────────────
    k8s.wait_until_succeeds("k3s ctr images ls -q | grep -q 'rio-worker:dev'", timeout=120)
    k8s.succeed("k3s kubectl apply -f ${workerPoolCRFile}")
    k8s.wait_until_succeeds(
        "k3s kubectl wait --for=condition=Established crd/workerpools.rio.build --timeout=60s")
    # Controller is a pod now (not a systemd unit). Wait for it to be
    # Ready before expecting reconcile outputs. Deployment name from
    # the chart.
    k8s.wait_until_succeeds("k3s ctr images ls -q | grep -q 'rio-controller:dev'", timeout=120)
    k8s.wait_until_succeeds(
        "k3s kubectl wait --for=condition=Available deploy/rio-controller --timeout=120s",
        timeout=150)
    k8s.wait_until_succeeds("k3s kubectl get statefulset default-workers -o name", timeout=60)
    k8s.wait_until_succeeds(
        "k3s kubectl wait --for=condition=Ready pod/default-workers-0 --timeout=150s",
        timeout=180)
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | grep -E 'rio_scheduler_workers_active 1'",
        timeout=60)

    ${common.seedBusybox "control"}

    def dump_logs():
        k8s.execute("k3s kubectl logs deploy/rio-controller --tail=100 >&2 || true")
        k8s.execute("k3s kubectl logs default-workers-0 --tail=100 >&2 || true")
        control.execute("journalctl -u rio-scheduler -u rio-gateway --no-pager -n 100 >&2")

    def build_drv(identity_file, drv_path, expect_fail=False):
        """Build via ssh-ng using the given identity file (selects the
        matching authorized_keys entry and thus the tenant). Returns
        the store path (last line of output, after SSH warnings +
        build progress lines)."""
        cmd = (
            "nix-build --no-out-link "
            f"--store 'ssh-ng://root@control?ssh-key={identity_file}' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            f"{drv_path} 2>&1"
        )
        try:
            if expect_fail:
                return client.fail(cmd)
            out = client.succeed(cmd)
            # Last non-empty line is the store path. Earlier lines
            # include SSH known_hosts warning + nix-build progress.
            lines = [l.strip() for l in out.strip().split("\n") if l.strip()]
            return lines[-1] if lines else ""
        except Exception:
            dump_logs()
            raise

    # ══════════════════════════════════════════════════════════════════
    # Section A: tenant resolution smoke
    # ══════════════════════════════════════════════════════════════════

    # ── Pre-seed the tenants table ────────────────────────────────────
    # -q suppresses the 'INSERT 0 1' status message that psql prints
    # even with -t (tuples-only) for INSERT…RETURNING. SELECT doesn't
    # emit a command tag under -t, so -q is cosmetic there but used
    # everywhere for consistency.
    tenant_uuid = control.succeed(
        "sudo -u postgres psql rio -qtAc "
        "\"INSERT INTO tenants (tenant_name) VALUES ('team-test') RETURNING tenant_id\""
    ).strip()
    control.log(f"seeded tenant team-test = {tenant_uuid}")

    # Row-count check: COUNT(*) after each case. ORDER BY…LIMIT 1 alone
    # can't distinguish Case 3's NULL row from a Case-2 leak (if
    # rejection ever moved post-insert, it would also produce a NULL
    # row since unknown-team never resolves to a UUID).
    def build_count():
        return int(control.succeed(
            "sudo -u postgres psql rio -qtAc \"SELECT COUNT(*) FROM builds\""
        ).strip())
    initial_count = build_count()
    control.log(f"initial builds count: {initial_count}")

    # ── Case 1: key comment 'team-test' → resolved UUID in builds ─────
    out = build_drv("/root/.ssh/id_team_test", "${testDrvKnown}")
    assert out.startswith("/nix/store/"), f"known-tenant build should succeed: {out!r}"
    assert "phase4-known" in out, f"output path should contain drv marker: {out!r}"
    assert build_count() == initial_count + 1, "case 1 should insert exactly one build"
    db_tenant = control.succeed(
        "sudo -u postgres psql rio -qtAc "
        "\"SELECT tenant_id FROM builds ORDER BY submitted_at DESC LIMIT 1\""
    ).strip()
    assert db_tenant == tenant_uuid, \
        f"builds.tenant_id should match seeded UUID: expected {tenant_uuid}, got {db_tenant!r}"
    print(f"Section A case 1 PASS: known tenant resolved to {db_tenant}")

    # ── Case 2: key comment 'unknown-team' → InvalidArgument ──────────
    out = build_drv("/root/.ssh/id_unknown", "${testDrvUnknown}", expect_fail=True)
    assert "unknown tenant: unknown-team" in out, \
        f"error should include the tenant name (proves comment was captured+propagated), got: {out!r}"
    assert build_count() == initial_count + 1, \
        "case 2 rejection is pre-insert: count unchanged"
    print("Section A case 2 PASS: unknown tenant rejected pre-insert")

    # ── Case 3: empty comment → tenant_id IS NULL (single-tenant) ─────
    out = build_drv("/root/.ssh/id_anon", "${testDrvAnon}")
    assert out.startswith("/nix/store/"), f"anon build should succeed: {out!r}"
    assert "phase4-anon" in out, f"output path should contain drv marker: {out!r}"
    assert build_count() == initial_count + 2, "case 3 should insert one more build"
    db_tenant = control.succeed(
        "sudo -u postgres psql rio -qtAc "
        "\"SELECT COALESCE(tenant_id::text, 'NULL') FROM builds ORDER BY submitted_at DESC LIMIT 1\""
    ).strip()
    assert db_tenant == "NULL", \
        f"empty-comment key → tenant_id IS NULL, got {db_tenant!r}"
    print("Section A case 3 PASS: empty comment = single-tenant mode (NULL)")

    ${common.collectCoverage "control, k8s"}
  '';
}
