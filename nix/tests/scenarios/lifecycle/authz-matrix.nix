# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # authz-matrix — tenant-authenticated surfaces enforce END-TO-END
  # ══════════════════════════════════════════════════════════════════
  # The deployed-cluster leg of the bughunt-2 slot-4 authz matrix
  # (bug_237 / merged_bug_122 / merged_bug_064 / bug_213): the unit
  # matrix (rio-store/src/authz.rs composed_authz_matrix_layer_tier)
  # proves the kernel's verdict table; THIS proves the helm-rendered
  # deployment actually wires it — jwt pubkey mounted (jwtEnabled
  # fixture), per-method classes enforced at the store's real port,
  # ownership checked against rows the production write path created.
  #
  # Production-truth note (merged_bug_064): before the build-membership
  # re-key, the OWN-tenant leg below was impossible — TailLog ownership
  # keyed on derivations.tenant_id, which NO production write path
  # populated (dropped by migration 095), so a gateway-submitted
  # build's log was unreadable by its own tenant the moment a JWT
  # pubkey was configured. The own-tenant assert is the leg the old
  # gate could never pass; the unit reds are in the slot-4 chain.
  #
  # Deny-shape note: the foreign TailLog leg asserts NotFound (the
  # absence-shaped deny of store.log.tail-ownership), NOT
  # PermissionDenied. Foreign WatchBuild accepts either PermissionDenied
  # (build still actor-resident) or NotFound (post-cleanup, tenant-bound
  # terminal row) — admit/deny is the law, the status split per phase
  # is the spec-pinned asymmetry pinned by unit tests.
  #
  # Tracey: r[verify ...] markers live at the default.nix subtests
  # entry (P0341 convention — marker at wiring point, not here).
  with subtest("authz-matrix: tenant surfaces enforce end-to-end"):
      # ── Own build through the production path ─────────────────────
      # ssh-ng via the gateway: authorized_keys comment = vm-lifecycle
      # → gateway resolves tenant → mints JWT → SubmitBuild stamps
      # builds.tenant_id. This writes the EXACT ownership chain
      # authorize_tail reads (builds → build_derivations →
      # assignments/drv_executions).
      out_authz = build("${authzDrv}", capture_stderr=False).strip()
      assert out_authz.startswith("/nix/store/"), f"authz build: {out_authz!r}"

      # The drv path + build id from the scheduler's own rows (PG is
      # the source of truth the gate reads; LIKE-scoped to this
      # fragment's unique marker so concurrent rows can't race us).
      drv_authz = psql_k8s(k3s_server,
          "SELECT drv_path FROM derivations "
          "WHERE drv_path LIKE '%-rio-test-lifecycle-authz.drv' LIMIT 1"
      )
      assert drv_authz.startswith("/nix/store/"), f"drv lookup: {drv_authz!r}"
      build_authz = psql_k8s(k3s_server,
          "SELECT b.build_id FROM builds b "
          "JOIN build_derivations bd ON bd.build_id = b.build_id "
          "JOIN derivations d ON d.derivation_id = bd.derivation_id "
          f"WHERE d.drv_path = '{drv_authz}' LIMIT 1"
      )
      assert build_authz, f"build lookup for {drv_authz!r}: {build_authz!r}"

      # ── Foreign tenant (created server-side, real JWT) ─────────────
      foreign_id = psql_k8s(k3s_server,
          "INSERT INTO tenants (tenant_name, gc_retention_hours) "
          "VALUES ('vm-authz-foreign', 0) RETURNING tenant_id"
      )
      foreign_jwt = k3s_server.succeed(f"${signJwt} {foreign_id}").strip()

      tail_payload = (
          '{"derivation": "' + drv_authz + '", "execId": "", '
          '"sinceLine": "0", "follow": false}'
      )

      def store_tail(token_header):
          """TailLog against the store's real service port (9002)
          through the deployed layer stack (authz layer + JWT
          interceptor + gRPC). Empty token_header = tokenless."""
          return pf_exec("deploy/rio-store", 9002,
              "${grpcurl} ${grpcurlTls} -max-time 30 "
              + token_header +
              "-protoset ${protoset}/rio.protoset "
              f"-d '{tail_payload}' "
              "localhost:__PORT__ rio.store.LogService/TailLog",
              ns="${nsStore}", ok_nonzero=True)

      # ── OWN tenant: admitted + served (the leg the dead-column gate
      #    could never pass) ──────────────────────────────────────────
      own = store_tail(f"-H 'x-rio-tenant-token: {tenant_jwt}' ")
      assert "Code:" not in own, (
          f"own-tenant TailLog must serve (build-membership ownership "
          f"over builds.tenant_id); got error: {own[:400]!r}"
      )

      # ── FOREIGN tenant: absence-shaped NotFound ────────────────────
      foreign = store_tail(f"-H 'x-rio-tenant-token: {foreign_jwt}' ")
      assert "NotFound" in foreign, (
          f"foreign TailLog must be the absence-shaped NotFound "
          f"(store.log.tail-ownership), got: {foreign[:400]!r}"
      )
      assert "PermissionDenied" not in foreign, (
          f"foreign TailLog must NOT be a distinguishable "
          f"PermissionDenied (existence oracle), got: {foreign[:400]!r}"
      )

      # ── TOKENLESS: layer Unauthenticated (TenantJwt class keyed) ───
      tokenless = store_tail("")
      assert "Unauthenticated" in tokenless, (
          f"tokenless TailLog must be rejected at the credential-class "
          f"layer (store.log.method-credential), got: {tokenless[:400]!r}"
      )

      # ── TenantQuota: same class, same layer ────────────────────────
      quota_payload = '{"tenantName": "vm-lifecycle"}'
      def store_quota(token_header):
          return pf_exec("deploy/rio-store", 9002,
              "${grpcurl} ${grpcurlTls} -max-time 30 "
              + token_header +
              "-protoset ${protoset}/rio.protoset "
              f"-d '{quota_payload}' "
              "localhost:__PORT__ rio.store.StoreService/TenantQuota",
              ns="${nsStore}", ok_nonzero=True)
      q_tokenless = store_quota("")
      assert "Unauthenticated" in q_tokenless, (
          f"tokenless TenantQuota must be layer-rejected, "
          f"got: {q_tokenless[:400]!r}"
      )
      q_own = store_quota(f"-H 'x-rio-tenant-token: {tenant_jwt}' ")
      assert "Code:" not in q_own, (
          f"own-tenant TenantQuota must be admitted, got: {q_own[:400]!r}"
      )

      # ── Foreign WatchBuild: denied in EVERY lifecycle phase ────────
      # PermissionDenied while actor-resident, NotFound once the
      # tenant-bound terminal row is the answer (bug_213) — admit/deny
      # is the law; the per-phase status split is unit-pinned.
      watch = pf_exec(leader_pod(), 9001,
          f"${grpcurl} ${grpcurlTls} -max-time 30 "
          f"-H 'x-rio-tenant-token: {foreign_jwt}' "
          f"-protoset ${protoset}/rio.protoset "
          f"-d '{{\"buildId\": \"{build_authz}\"}}' "
          f"localhost:__PORT__ rio.scheduler.SchedulerService/WatchBuild",
          ok_nonzero=True)
      assert ("PermissionDenied" in watch) or ("NotFound" in watch), (
          f"foreign WatchBuild must deny (resident PermissionDenied or "
          f"terminal-row NotFound), got: {watch[:400]!r}"
      )
      assert '"buildId"' not in watch or "Code:" in watch, (
          f"foreign WatchBuild must not stream snapshots: {watch[:400]!r}"
      )

      print(
          "authz-matrix PASS: own-tenant tail served via "
          "build-membership; foreign tail absence-shaped NotFound; "
          "tokenless TailLog/TenantQuota layer-rejected; foreign "
          "WatchBuild denied"
      )
''
