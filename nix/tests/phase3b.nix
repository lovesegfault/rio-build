# Phase 3b milestone validation: production hardening.
#
# Iteration 2: mTLS (T), HMAC (B), state recovery (S), GC (C),
# gateway validation (G). Skips k3s-dependent sections (E/F/D) —
# those need real K8s API (PDB/NetPol/Events/Build CRD); the FOD
# proxy (D) needs a Squid VM. Skips A (cancel via cgroup.kill) —
# complex timing, unit-tested in rio-worker/src/runtime.rs.
#
# Topology (3 VMs):
#   control — PG + store + scheduler + gateway. Server cert with
#             SANs {control, localhost, 127.0.0.1}. HMAC key
#             shared with itself (scheduler signs, store verifies).
#   worker  — rio-worker. Client cert signed by the same CA.
#             Connects to control:9001/9002 → SNI=control (tonic
#             derives SNI from URL host; see rio-common/src/tls.rs).
#   client  — nix client, ssh-ng to gateway. SSH doesn't use gRPC
#             TLS — client stays plaintext.
#
# PKI: generated at Nix eval time via pkgs.runCommand + openssl.
# Self-signed CA → server cert → client cert. Simpler than
# cert-manager in VM; cert-manager correctness is EKS smoke scope.
# The PKI derivation is a store path → available on all VMs (NixOS
# test driver makes the test's closure available on every node).
#
# Run:
#   NIXBUILDNET_REUSE_BUILD_FAILURES=false nix-build-remote --no-nom -- -L .#checks.x86_64-linux.vm-phase3b
{
  pkgs,
  rio-workspace,
  rioModules,
  # dockerImages + crds: passed for k3s sections (iteration 3).
  # Unused in iteration 2 but the flake passes them to all
  # phase3* tests — `...` swallows them so deadnix is happy and
  # the callers don't branch.
  ...
}:
let
  common = import ./common.nix { inherit pkgs rio-workspace rioModules; };

  # ── PKI: self-signed CA + server/client certs + HMAC key ────────────
  #
  # Generated at DERIVATION BUILD TIME (openssl runs once, output is a
  # store path). Not hermetic across rebuilds (openssl uses /dev/urandom
  # so every eval produces a fresh key set), but that's fine for a VM
  # test — we only need internal consistency (server cert signed by the
  # CA that the client trusts).
  #
  # Server cert SANs:
  #   - control: worker connects to control:9001/9002; tonic derives
  #     SNI from the URL authority → SNI=control. rustls verifies
  #     against SANs. MUST be present.
  #   - localhost: gateway (on control) connects to localhost:9001/
  #     9002 for scheduler/store. Same SNI derivation → SNI=localhost.
  #   - 127.0.0.1: belt + suspenders for IP-based addressing. Not
  #     used currently but costs nothing.
  #
  # Client cert: presented by the worker for mTLS. CN=rio-worker is
  # cosmetic (rio doesn't check CN, only that the cert chains to the
  # CA). SANs not needed for outbound — servers only check chain.
  #
  # HMAC key: hex-encoded 32-byte random. rio-common/src/hmac.rs
  # load_key reads RAW bytes (after stripping trailing \n). The hex
  # string IS the key (64 bytes as-is, not decoded). HMAC accepts any
  # key length so this is valid; hex avoids null bytes in the file.
  pki =
    pkgs.runCommand "rio-test-pki"
      {
        buildInputs = [ pkgs.openssl ];
      }
      ''
        mkdir -p $out

        # ── Self-signed root CA ─────────────────────────────────────
        openssl req -x509 -newkey rsa:2048 -nodes \
          -keyout $out/ca.key -out $out/ca.crt \
          -days 3650 -subj "/CN=rio-test-ca"

        # ── Server cert (SANs = control, localhost, 127.0.0.1) ──────
        # -addext works on openssl 1.1.1+. nixpkgs openssl is 3.x.
        openssl req -newkey rsa:2048 -nodes \
          -keyout $out/server.key -out server.csr \
          -subj "/CN=control"
        openssl x509 -req -in server.csr \
          -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
          -out $out/server.crt -days 3650 \
          -extfile <(printf 'subjectAltName=DNS:control,DNS:localhost,IP:127.0.0.1')

        # ── Client cert (worker mTLS identity) ──────────────────────
        openssl req -newkey rsa:2048 -nodes \
          -keyout $out/client.key -out client.csr \
          -subj "/CN=rio-worker"
        openssl x509 -req -in client.csr \
          -CA $out/ca.crt -CAkey $out/ca.key -CAcreateserial \
          -out $out/client.crt -days 3650

        # ── HMAC key (shared scheduler ↔ store secret) ──────────────
        # -hex emits lowercase hex + newline. load_key strips the
        # trailing \n (see rio-common/src/hmac.rs:105-111) so `echo`
        # vs `echo -n` doesn't matter here.
        openssl rand -hex 32 > $out/hmac.key
      '';

  # ── Protoset for grpcurl (no gRPC reflection on rio servers) ────────
  #
  # rio-scheduler + rio-store don't register tonic-reflection. grpcurl
  # without a protoset/proto can only probe the health service (via
  # bundled grpc.health.v1 descriptors). For TriggerGC (C1) we need
  # rio.admin.AdminService → compile the protos to a FileDescriptorSet
  # that grpcurl loads with -protoset.
  #
  # --include_imports: AdminService imports types.proto + google's
  # empty.proto. grpcurl needs the transitive closure to resolve
  # GCRequest/GCProgress. protoc bundles the well-known types
  # (empty.proto) automatically when this flag is set.
  protoset =
    pkgs.runCommand "rio-protoset"
      {
        buildInputs = [ pkgs.protobuf ];
      }
      ''
        mkdir -p $out
        protoc \
          --proto_path=${../../rio-proto/proto} \
          --descriptor_set_out=$out/rio.protoset \
          --include_imports \
          admin.proto types.proto
      '';

  # ── Trivial derivation for build tests (B1, S1) ─────────────────────
  # Same pattern as phase1b: one leaf, no inputDrvs, busybox builder.
  # The content of $out varies per-section (parameterized below) so
  # S1's post-restart build produces a DIFFERENT store path than B1's
  # — otherwise S1 would be a cache hit and not prove dispatch works.
  mkTestDrvFile =
    stamp:
    pkgs.writeText "phase3b-${stamp}.nix" ''
      { busybox }:
      derivation {
        name = "rio-3b-${stamp}";
        system = builtins.currentSystem;
        builder = "''${busybox}/bin/sh";
        args = [
          "-c"
          '''
            set -ex
            ''${busybox}/bin/busybox mkdir -p $out
            ''${busybox}/bin/busybox echo "phase3b ${stamp}" > $out/stamp
          '''
        ];
      }
    '';

  testDrvFileB1 = mkTestDrvFile "hmac";
  testDrvFileS1 = mkTestDrvFile "recovery";

  # __noChroot derivation: REJECTED by gateway's translate::validate_dag.
  # Rejection is pre-SubmitBuild so scheduler never sees it — no TLS
  # on the scheduler path exercised, but G1 still proves the rejection
  # happens. Kept from iteration 1.
  noChrootDrvFile = pkgs.writeText "phase3b-nochroot.nix" ''
    { busybox }:
    derivation {
      name = "rio-3b-nochroot";
      system = "x86_64-linux";
      __noChroot = true;  # ← rejected
      builder = "''${busybox}/bin/sh";
      args = [ "-c" "echo should-never-run > $out" ];
    }
  '';

  # ── Control-plane env: TLS + HMAC ───────────────────────────────────
  #
  # Applied to rio-store, rio-scheduler, rio-gateway via common.nix's
  # extraServiceEnv. All three share the SAME server cert (they all run
  # on `control`; the cert's SANs cover every name they're addressed
  # by). figment maps RIO_TLS__CERT_PATH → tls.cert_path (double
  # underscore = nesting).
  #
  # RIO_HMAC_KEY_PATH: scheduler signs assignment tokens, store
  # verifies. Same file → same key. Gateway ignores (not in its
  # Config struct; figment silently drops unknown vars).
  #
  # No RIO_HEALTH_ADDR here: each binary has a different default
  # (scheduler=9101, store=9102, gateway=9190). Setting it once would
  # make all three bind the SAME port → conflict. Defaults are correct.
  controlTlsEnv = {
    RIO_TLS__CERT_PATH = "${pki}/server.crt";
    RIO_TLS__KEY_PATH = "${pki}/server.key";
    RIO_TLS__CA_PATH = "${pki}/ca.crt";
    RIO_HMAC_KEY_PATH = "${pki}/hmac.key";
  };

  # Worker env: CLIENT cert (outbound mTLS to scheduler + store).
  # No HMAC key — the worker RECEIVES signed tokens from the scheduler
  # and forwards them as gRPC metadata on PutPath; it never signs.
  workerTlsEnv = {
    RIO_TLS__CERT_PATH = "${pki}/client.crt";
    RIO_TLS__KEY_PATH = "${pki}/client.key";
    RIO_TLS__CA_PATH = "${pki}/ca.crt";
  };

in
pkgs.testers.runNixOSTest {
  name = "rio-phase3b";

  nodes = {
    control = common.mkControlNode {
      hostName = "control";
      memorySize = 2048;
      extraServiceEnv = controlTlsEnv;
      # grpcurl: T1/T2/C1 (gRPC calls with/without TLS).
      # grpc-health-probe: T2/T3 (health checks without needing
      #   reflection or a protoset — it has grpc.health.v1 baked in).
      # openssl: T1 fallback (s_client for raw TLS handshake check).
      extraPackages = [
        pkgs.grpcurl
        pkgs.grpc-health-probe
        pkgs.openssl
      ];
      # 9091/9092/9093: metrics (curl checks). 9101/9102: plaintext
      # health ports (spawned by scheduler/store when TLS is on).
      # 9190: gateway's health port (always spawned; gateway's main
      # is SSH not gRPC). 9001/9002 already in base set (worker
      # connects cross-VM via TLS).
      extraFirewallPorts = [
        9091
        9092
        9093
        9101
        9102
        9190
      ];
    };

    worker = common.mkWorkerNode {
      hostName = "worker";
      maxBuilds = 1;
      extraServiceEnv = workerTlsEnv;
    };

    client = common.mkClientNode {
      gatewayHost = "control";
    };
  };

  testScript = ''
    start_all()

    # ── Control plane boot ────────────────────────────────────────────
    ${common.waitForControlPlane "control"}

    # ── SSH key + gateway restart (authorized_keys) ───────────────────
    ${common.sshKeySetup "control"}

    # ── Worker registration (mTLS handshake proven implicitly) ────────
    # The worker's Register RPC goes over mTLS (client cert required).
    # If the cert/CA/SAN config is wrong, the worker never registers
    # and this wait times out — so reaching B1 below IS the T-section
    # positive proof. But we also do explicit probes (T1-T3) for
    # clarity.
    worker.wait_for_unit("rio-worker.service")
    control.wait_until_succeeds(
        "curl -sf http://localhost:9091/metrics | "
        "grep -E 'rio_scheduler_workers_active 1'"
    )

    # ── Seed store ────────────────────────────────────────────────────
    ${common.seedBusybox "control"}

    # ── Build helper (top-level def, not inside subtest) ──────────────
    # B1 and S1 both need to nix-build a derivation. mkBuildHelper bakes
    # a single testDrvFile at Nix-eval time, so define it once and pass
    # the drv file via Python. The wrapped build_drv() accepts any path.
    def build_drv(workers, drv_path, capture_stderr=True):
        cmd = (
            "nix-build --no-out-link "
            "--store 'ssh-ng://control' "
            "--arg busybox '(builtins.storePath ${common.busybox})' "
            + drv_path
        )
        if capture_stderr:
            cmd += " 2>&1"
        try:
            return client.succeed(cmd)
        except Exception:
            for w in workers:
                w.execute("journalctl -u rio-worker --no-pager -n 200 >&2")
            control.execute("journalctl -u rio-scheduler -u rio-gateway --no-pager -n 200 >&2")
            raise

    # ════════════════════════════════════════════════════════════════
    # Section T: mTLS
    # ════════════════════════════════════════════════════════════════

    with subtest("T1: plaintext connect to TLS port fails"):
        # grpcurl -plaintext against the scheduler's main port (9001,
        # mTLS). The TCP connection succeeds but the TLS handshake
        # fails — the server expects a ClientHello, gets a plaintext
        # HTTP/2 preface. grpcurl surfaces this as "connection reset"
        # or a tls-related error. We only check that it FAILS; the
        # exact error text varies by grpcurl/tonic version.
        #
        # -max-time 5: don't hang if something's misconfigured. The
        # failure is immediate (handshake), not a timeout.
        result = control.fail(
            "grpcurl -plaintext -max-time 5 localhost:9001 "
            "grpc.health.v1.Health/Check 2>&1"
        )
        # Any of: "tls", "EOF", "connection reset", "handshake". We're
        # not picky — the point is it fails, and not with "connection
        # refused" (that would mean the port isn't listening at all,
        # which is a different failure).
        assert "refused" not in result.lower(), \
            f"T1: port should be OPEN (TLS rejects, not refused): {result[:200]}"
        print("T1 PASS: plaintext rejected on mTLS port 9001")

    with subtest("T2: mTLS connect with valid cert succeeds"):
        # grpc-health-probe with TLS: presents the server cert as a
        # CLIENT cert (both signed by the same CA, so the server
        # accepts it for mTLS). The health service is on the main
        # port too (scheduler adds it to the mTLS server), so this
        # proves full mTLS round-trip: client cert accepted, server
        # cert validated, gRPC call completes.
        #
        # -tls-server-name localhost: grpc-health-probe doesn't
        # derive SNI from -addr the way tonic does; we set it
        # explicitly to match a SAN (localhost is in the cert).
        control.succeed(
            "grpc-health-probe -addr localhost:9001 "
            "-tls -tls-ca-cert ${pki}/ca.crt "
            "-tls-client-cert ${pki}/server.crt "
            "-tls-client-key ${pki}/server.key "
            "-tls-server-name localhost"
        )
        # Same for the store.
        control.succeed(
            "grpc-health-probe -addr localhost:9002 "
            "-tls -tls-ca-cert ${pki}/ca.crt "
            "-tls-client-cert ${pki}/server.crt "
            "-tls-client-key ${pki}/server.key "
            "-tls-server-name localhost"
        )
        print("T2 PASS: mTLS with valid cert accepted on 9001 + 9002")

    with subtest("T3: plaintext health port works without TLS"):
        # The scheduler/store spawn a SECOND plaintext server on
        # health_addr (9101/9102) when TLS is enabled. K8s gRPC
        # probes can't do mTLS — this port is for them. It serves
        # ONLY grpc.health.v1.Health, sharing the same HealthReporter
        # as the main port (so leadership toggles propagate).
        control.succeed("grpc-health-probe -addr localhost:9101")
        control.succeed("grpc-health-probe -addr localhost:9102")
        print("T3 PASS: plaintext health ports 9101 + 9102 respond")

    # ════════════════════════════════════════════════════════════════
    # Section B: HMAC assignment tokens
    # ════════════════════════════════════════════════════════════════

    with subtest("B1: build succeeds with HMAC in the loop"):
        # Positive path: scheduler has HMAC signer, store has HMAC
        # verifier. A successful build proves the full token flow:
        #   1. scheduler signs Claims{worker_id, drv_hash,
        #      expected_outputs, expiry} at dispatch
        #   2. worker receives token in WorkAssignment
        #   3. worker forwards token as x-rio-assignment-token gRPC
        #      metadata on PutPath
        #   4. store verifies signature + expiry + output path ∈
        #      expected_outputs
        #
        # Negative path (PutPath without token → PERMISSION_DENIED)
        # needs crafting a raw PutPath gRPC stream with NAR chunks
        # — complex via grpcurl. Covered by 10 unit tests in
        # rio-store/src/grpc/put_path.rs hmac module.
        out_b1 = build_drv([worker], "${testDrvFileB1}", capture_stderr=False).strip()
        assert out_b1.startswith("/nix/store/"), \
            f"B1: HMAC-signed build should succeed: {out_b1!r}"

        # Metric: PutPath succeeded (token accepted). result="created"
        # means the path was NEW (not a cache hit) — so the HMAC check
        # actually ran (cache hits short-circuit before HMAC).
        control.succeed(
            "curl -sf http://localhost:9092/metrics | "
            "grep -E 'rio_store_put_path_total\\{result=\"created\"\\} [1-9]'"
        )
        print(f"B1 PASS: build with HMAC token succeeded, output {out_b1}")

    # ════════════════════════════════════════════════════════════════
    # Section S: scheduler state recovery
    # ════════════════════════════════════════════════════════════════

    with subtest("S1: scheduler restart triggers recovery"):
        # The recovery path (rio-scheduler/src/actor/recovery.rs):
        #   1. lease loop acquires → fires ActorCommand::LeaderAcquired
        #   2. actor handles it → recover_from_pg() runs → reads
        #      non-terminal builds/derivations/edges from PG
        #   3. recovery_complete.store(true)
        #   4. dispatch_ready gate: if !is_leader || !recovery_complete
        #      → no-op. Dispatch is BLOCKED until recovery completes.
        #
        # Proof strategy: restart scheduler, wait for it to re-register
        # the worker (mTLS handshake again), then do a FRESH build.
        # The build succeeding proves dispatch is unblocked, which
        # proves recovery_complete was set, which proves recovery ran.
        #
        # We don't test in-flight build recovery here — that needs
        # precise timing (submit, kill mid-build, restart, assert
        # completion). Unit-tested in rio-scheduler/src/actor/
        # tests/recovery.rs with seeded PG rows.

        control.succeed("systemctl restart rio-scheduler")
        control.wait_for_unit("rio-scheduler.service")
        # Main port (TLS) listening again. wait_for_open_port just
        # TCP-connects — works for TLS ports too (TLS handshake is
        # layer-above, TCP is enough to prove the socket is bound).
        control.wait_for_open_port(9001)
        # Plaintext health port also back up (re-spawned on restart).
        control.wait_for_open_port(9101)

        # Worker restart. The worker's main loop exits on scheduler
        # disconnect ("shutting down"); systemd restarts it. But early
        # startup races (worker before scheduler/store → connection
        # refused → exit) may have hit StartLimitBurst — systemd
        # then stops restarting. reset-failed clears the limit
        # counter; restart forces a fresh attempt.
        worker.succeed("systemctl reset-failed rio-worker || true")
        worker.succeed("systemctl restart rio-worker")
        worker.wait_for_unit("rio-worker.service")

        # Worker re-registers over fresh mTLS. Fresh scheduler process
        # = metrics reset to 0 → wait for "workers_active 1" again.
        control.wait_until_succeeds(
            "curl -sf http://localhost:9091/metrics | "
            "grep -E 'rio_scheduler_workers_active 1'"
        )

        # Post-restart build. DIFFERENT derivation than B1 (stamp
        # differs → output path differs) so this is NOT a cache hit
        # — it goes through dispatch, proving dispatch is unblocked.
        out_s1 = build_drv([worker], "${testDrvFileS1}", capture_stderr=False).strip()
        assert out_s1.startswith("/nix/store/"), \
            f"S1: post-restart build should succeed: {out_s1!r}"
        assert out_s1 != out_b1, \
            f"S1: should be a DIFFERENT path than B1 (not cache hit): {out_s1!r} == {out_b1!r}"
        print(f"S1 PASS: scheduler restart → recovery → dispatch unblocked, built {out_s1}")

    # ════════════════════════════════════════════════════════════════
    # Section C: GC (mark-sweep + pending_s3_deletes)
    # ════════════════════════════════════════════════════════════════

    with subtest("C1: TriggerGC dry-run via AdminService proxy"):
        # AdminService.TriggerGC (on the scheduler) proxies to
        # StoreAdminService.TriggerGC after populating extra_roots
        # with live-build output paths (rio-scheduler/src/admin/
        # mod.rs:416-435). dry_run=true → mark phase runs, sweep
        # does ROLLBACK + returns stats (no actual deletes).
        #
        # The store has ≥3 paths at this point: busybox (seed) +
        # B1's output + S1's output. None are reachable from gc_roots
        # (no pins), but all are within grace period — so with
        # grace_period_hours=24 nothing should be collected. We
        # just verify the RPC completes with is_complete=true.
        #
        # -protoset: no gRPC reflection on rio servers (see protoset
        # derivation above). grpcurl needs compiled descriptors for
        # rio.admin.AdminService.
        #
        # -d: proto3 JSON. grace_period_hours=24 = default grace.
        # dry_run=true → sweep rolls back (we don't want to actually
        # delete the test's own outputs).
        # -authority localhost: sets both TLS SNI (cert verification)
        # and the :authority HTTP/2 pseudo-header. localhost is in
        # the server cert's SANs. -servername is deprecated in favor
        # of -authority (grpcurl 1.9+).
        result = control.succeed(
            "grpcurl "
            "-cacert ${pki}/ca.crt "
            "-cert ${pki}/server.crt "
            "-key ${pki}/server.key "
            "-authority localhost "
            "-protoset ${protoset}/rio.protoset "
            """-d '{"dry_run": true, "grace_period_hours": 24}' """
            "localhost:9001 rio.admin.AdminService/TriggerGC 2>&1"
        )
        # GCProgress stream: at least one message with isComplete=true.
        # grpcurl emits proto3 JSON (camelCase field names) per
        # streamed message. We look for the completion flag —
        # proves the stream ran end-to-end through scheduler →
        # store → mark → sweep(rollback) → progress stream → proxy
        # back to client.
        assert '"isComplete": true' in result or '"isComplete":true' in result, \
            f"C1: expected GCProgress with isComplete=true, got: {result[:500]}"
        print("C1 PASS: TriggerGC dry-run completed via AdminService proxy")

    # ════════════════════════════════════════════════════════════════
    # Section G: gateway validation (from iteration 1)
    # ════════════════════════════════════════════════════════════════

    with subtest("G1: __noChroot derivation rejected"):
        # nix-build a derivation with __noChroot=true → gateway's
        # translate::validate_dag rejects with "sandbox escape" error
        # BEFORE SubmitBuild. The scheduler never sees it, so no TLS
        # on the scheduler path is exercised here — but that's fine,
        # G is about gateway-side validation not scheduling.
        result = client.fail("""
          nix-build ${noChrootDrvFile} --arg busybox \
            '(builtins.storePath ${common.busybox})' \
            --store ssh-ng://control 2>&1
        """)
        assert ("sandbox escape" in result or "noChroot" in result), \
            f"G1: expected __noChroot rejection, got: {result[:500]}"
        print("G1 PASS: __noChroot rejected at gateway")

    # ════════════════════════════════════════════════════════════════
    print("=" * 60)
    print("Phase 3b iteration 2: T1-T3 (mTLS), B1 (HMAC), S1 (recovery),")
    print("C1 (GC), G1 (__noChroot) — all PASS.")
    print()
    print("Skipped (k3s-dependent, iteration 3 scope):")
    print("  E (PDB/NetPol/Events/seccomp) — needs real K8s API")
    print("  F (WatchBuild reconnect)      — needs Build CRD")
    print("  D (FOD proxy)                 — needs Squid VM + NetworkPolicy")
    print("  A (cancel via cgroup.kill)    — timing-sensitive; 6 unit tests")
    print("  B2 (PutPath token reject)     — raw gRPC stream; 10 unit tests")
    print("=" * 60)
  '';
}
