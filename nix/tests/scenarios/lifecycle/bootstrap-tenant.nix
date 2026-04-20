# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # bootstrap-tenant — leader-guard: standby rejects, leader accepts
  # ══════════════════════════════════════════════════════════════════
  # scheduler.replicas=2 (vmtest-full.yaml:99) → one leader + one
  # standby. AdminService writes (CreateTenant) are leader-only;
  # standby's interceptor rejects with UNAVAILABLE. Clients that
  # route via the ClusterIP Service (50% standby) must retry;
  # clients that route via Lease holderIdentity (tunnel_grpc in
  # xtask, leader_pod() here) are deterministic.
  #
  # abef66c7 fixed tunnel_grpc to use Lease-lookup instead of
  # Service forward — before that, `xtask k8s cli create-tenant`
  # failed ~50% depending on which replica the port-forward hit.
  # 5b98e311 fixed step_tenant's AlreadyExists handling — retry
  # after a transient failure should succeed OR return
  # already-exists, not crash.
  #
  # This fragment proves the standby-reject path WORKS (positive
  # test — the guard IS enforcing) AND the Lease-routed path is
  # deterministic (3 attempts, all hit leader). Unit test
  # (guards_tests.rs) proves interceptor shape; this proves it
  # end-to-end against the real 2-replica Deployment.
  #
  # Tracey: r[verify sched.grpc.leader-guard] at default.nix
  # subtests entry — first VM-level verify under replicas>1.
  with subtest("bootstrap-tenant: standby rejects, Lease-routed leader accepts"):
      # ── Find standby (all sched pods minus leader) ────────────────
      leader = leader_pod()
      all_sched = kubectl(
          "get pods -l app.kubernetes.io/name=rio-scheduler "
          "-o jsonpath='{.items[*].metadata.name}'"
      ).split()
      standby_pods = [p for p in all_sched if p != leader]
      assert len(standby_pods) >= 1, (
          f"expected ≥1 standby with scheduler.replicas=2; "
          f"leader={leader}, all={all_sched}. If this fires, "
          f"replicas may have been downscaled or podAntiAffinity "
          f"failed to spread across server+agent."
      )
      standby = standby_pods[0]
      print(f"bootstrap-tenant: leader={leader}, standby={standby}")

      # ── Standby MUST reject CreateTenant ──────────────────────────
      # Port-forward to the STANDBY. grpcurl CreateTenant fails —
      # leader-guard interceptor checks election state and returns
      # UNAVAILABLE on non-leader. grpcurl's exit is 64+status_code
      # (UNAVAILABLE=14 → exit 78). .fail() expects nonzero AND
      # returns stdout+stderr — same pattern health-shared uses
      # for the NOT_SERVING probe. Port-forward setup separate
      # (.succeed), grpcurl in .fail, cleanup in finally.
      pf_open(standby, 19301, 9001, tag="pf-standby")
      try:
          standby_out = k3s_server.fail(
              "${grpcurl} ${grpcurlTls} -max-time 15 "
              f"-H 'x-rio-service-token: {service_token}' "
              "-protoset ${protoset}/rio.protoset "
              '-d \'{"tenantName": "prod-parity-standby-reject"}\' '
              "localhost:19301 rio.admin.AdminService/CreateTenant 2>&1"
          )
          print(f"bootstrap-tenant: standby CreateTenant out:\n{standby_out}")
          # UNAVAILABLE is tonic's status for "not serving" — the
          # leader-guard intercept maps election_state != Leader
          # to it. grpcurl prints the status code name. .fail()
          # already proved nonzero exit; this proves it's the
          # RIGHT error (not a TLS mishap or port-forward race).
          assert "Unavailable" in standby_out, (
              f"standby rejection should be gRPC Unavailable (the "
              f"leader-guard status); got a different error. If "
              f"this is a TLS/transport error, the port-forward "
              f"raced or the PKI paths drifted. If it's a "
              f"different gRPC status, check the interceptor's "
              f"status mapping (actor_guards.rs). out:\n{standby_out}"
          )
          assert "not leader" in standby_out, (
              f"standby rejection message should say 'not leader' "
              f"(the exact message from actor_guards.rs); got a "
              f"different Unavailable — maybe the health reporter "
              f"not the leader-guard? out:\n{standby_out}"
          )
      finally:
          pf_close(tag="pf-standby")

      # ── Lease-routed CreateTenant — 3 attempts, deterministic ─────
      # Each attempt re-queries leader_pod() → fresh Lease read →
      # port-forward to the current leader. With replicas=2 and
      # the Lease stable (no failover during this ~15s window),
      # all 3 hit the same pod. First succeeds; 2nd/3rd return
      # AlreadyExists (db/tenants.rs: ON CONFLICT DO NOTHING →
      # None → admin/mod.rs maps to AlreadyExists). ok_nonzero
      # swallows the AlreadyExists so we assert on output, not
      # exit code. pf_exec auto-allocates a fresh port per call
      # (TIME_WAIT-safe).
      tenant_name = "prod-parity-test"
      attempt_outs = []
      for i in range(3):
          ldr = leader_pod()
          out = pf_exec(ldr, 9001,
              f"${grpcurl} ${grpcurlTls} -max-time 15 "
              f"-H 'x-rio-service-token: {service_token}' "
              f"-protoset ${protoset}/rio.protoset "
              f"-d '{{\"tenantName\": \"{tenant_name}\"}}' "
              f"localhost:__PORT__ rio.admin.AdminService/CreateTenant",
              ok_nonzero=True)
          attempt_outs.append(out)
          print(f"bootstrap-tenant: attempt {i+1} (leader={ldr}):\n{out}")

      # None of the 3 attempts should have hit the standby-reject
      # path. leader_pod()'s Lease-lookup is the same pattern
      # xtask's tunnel_grpc uses post-abef66c7 — if this fires,
      # the Lease holderIdentity isn't pointing at the actual
      # leader (lease renew race? Lease TTL expired between
      # query and connect?).
      for i, out in enumerate(attempt_outs):
          assert "Unavailable" not in out and "UNAVAILABLE" not in out, (
              f"attempt {i+1} hit the standby-reject path — "
              f"Lease-lookup returned a non-leader. Before "
              f"abef66c7 this was ~50% via Service forward; "
              f"Lease-routed should be 0%. out:\n{out}"
          )

      # ── ListTenants proves the create landed ──────────────────────
      # Via the same Lease-routed path. ListTenants is read-only
      # but also leader-guarded (admin reads need consistent
      # view). Response is JSON (grpcurl default output).
      list_out = pf_exec(leader_pod(), 9001,
          "${grpcurl} ${grpcurlTls} -max-time 15 "
          f"-H 'x-rio-service-token: {service_token}' "
          "-protoset ${protoset}/rio.protoset -d '{}' "
          "localhost:__PORT__ rio.admin.AdminService/ListTenants")
      print(f"bootstrap-tenant: ListTenants:\n{list_out}")
      assert tenant_name in list_out, (
          f"{tenant_name!r} should appear in ListTenants after 3 "
          f"create attempts (at least one succeeded). If standby-"
          f"reject passed above but this fails, the create was "
          f"accepted then lost — check PG commit path. "
          f"out:\n{list_out}"
      )

      print(
          f"bootstrap-tenant PASS: standby rejected, 3 Lease-routed "
          f"attempts all reached leader, {tenant_name!r} in "
          f"ListTenants"
      )
''
