# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # gc-sweep — pin + backdate + non-dry-run sweep PROVES commit
  # ══════════════════════════════════════════════════════════════════
  # THE test that proves sweep's for-batch loop body executes. With
  # all-in-grace paths, unreachable=vec![] → loop never runs → neither
  # commit NOR rollback fires → dry-run and non-dry-run are
  # indistinguishable. Backdating ONE path past grace makes it the
  # sole unreachable candidate → `pathsCollected: "1"` proves the
  # DELETE ran.
  #
  # Self-contained: builds its own pin-target + victim paths.
  # In the monolith, these came from the `initial` and `recovery`
  # subtests (ordering hack to avoid slow-build worker-slot block).
  # Split architecture: each fragment owns its data.
  with subtest("gc-sweep: pinned root + backdated sweep deletes EXACTLY 1"):
      # Build the two target paths. pinDrv = pinned, survives sweep.
      # gcVictimDrv = unpinned, backdated, deleted by sweep.
      out_pin = build("${pinDrv}", capture_stderr=False).strip()
      assert out_pin.startswith("/nix/store/"), f"pin build: {out_pin!r}"
      out_victim = build("${gcVictimDrv}", capture_stderr=False).strip()
      assert out_victim.startswith("/nix/store/"), f"victim build: {out_victim!r}"

      # Pin out_pin via scheduler_live_pins (mark-phase seed (e);
      # the dedicated explicit-pin table was dropped in migration
      # 036). pin_live looks up store_path_hash via narinfo so we
      # needn't compute the BYTEA hash here. Count is scoped to
      # drv_hash='vm-lifecycle' — the scheduler may concurrently
      # write/delete rows for real builds under other drv_hash
      # values; an unscoped count would be racy.
      pin_live(out_pin, "vm-lifecycle")
      pin_after = int(psql_k8s(k3s_server,
          "SELECT COUNT(*) FROM scheduler_live_pins "
          "WHERE drv_hash = 'vm-lifecycle'"
      ))
      assert pin_after == 1, (
          f"pin should add exactly 1 scheduler_live_pins row "
          f"tagged 'vm-lifecycle'; got {pin_after}"
      )

      # Backdate out_victim past grace. This is the path sweep will
      # delete. out_victim is unpinned (we only pinned out_pin) AND
      # unreferenced: gcVictimDrv is mkTrivial, its output is plain
      # text with no embedded store paths, so the ref scanner
      # correctly finds refs=[] — correct behavior for a leaf
      # derivation, not a gap (see line ~100 + ~225). created_at =
      # now() - 25h puts it 1h past a 24h grace window.
      #
      # Single-quote SQL avoids bash-escaping (psql_k8s wraps in
      # double quotes). Python f-string interpolates the path.
      psql_k8s(k3s_server,
          f"UPDATE narinfo SET created_at = now() - interval '25 hours' "
          f"WHERE store_path = '{out_victim}'"
      )

      # Non-dry-run sweep. dry_run=false → sweep COMMITs. grace=24h →
      # ONLY out_victim is past grace → unreachable={out_victim}
      # → for-batch loop iterates once → DELETE → COMMIT.
      #
      # force=true bypasses the empty-refs safety gate — mkTrivial
      # leaf outputs genuinely have refs=[] (scanner finds no store
      # paths, see comment above), which would otherwise trip the
      # gate (FailedPrecondition).
      result = sched_grpc(
          '{"dry_run": false, "grace_period_hours": 24, "force": true}',
          "rio.admin.AdminService/TriggerGC",
      )
      # proto3 JSON uint64 is a STRING ("1" not 1). pathsCollected
      # EXACTLY "1" is THE assertion: without backdate, it's 0 (or
      # absent — proto3 omits zero-value) and we've proven nothing
      # about the loop body.
      gc_msgs = grpcurl_json_stream(result)
      complete_msgs = [m for m in gc_msgs if m.get("isComplete")]
      assert complete_msgs, (
          f"expected GCProgress.isComplete=true; got {len(gc_msgs)} "
          f"messages: {result[:500]}"
      )
      final = complete_msgs[-1]
      assert final.get("pathsCollected") == "1", (
          f"expected EXACTLY pathsCollected='1' (backdated victim "
          f"output); got {final.get('pathsCollected')!r}. Full: {final!r}"
      )

      # out_victim GONE — nix path-info MUST fail.
      client.fail(
          f"nix path-info --store 'ssh-ng://k3s-server' {out_victim}"
      )
      # out_pin still there — pin protected it.
      client.succeed(
          f"nix path-info --store 'ssh-ng://k3s-server' {out_pin}"
      )

      # Unpin round-trip. ==0 is safe because the count is scoped
      # to drv_hash='vm-lifecycle' — only WE write that tag.
      unpin_live(out_pin, "vm-lifecycle")
      unpin_after = int(psql_k8s(k3s_server,
          "SELECT COUNT(*) FROM scheduler_live_pins "
          "WHERE drv_hash = 'vm-lifecycle'"
      ))
      assert unpin_after == 0, (
          f"unpin should remove the 'vm-lifecycle' pin row; "
          f"{unpin_after} remain"
      )
      # ── path_tenants end-to-end: tenant-key build → upsert fires ──
      # Proves the completion hook (completion.rs r[impl sched.gc.
      # path-tenants-upsert]) fires end-to-end in the k3s fixture:
      # SSH key comment → gateway tenant_name → scheduler resolves
      # UUID → builds.tenant_id → completion filter_map YIELDS →
      # upsert_path_tenants INSERTs.
      #
      # Also proves hash-encoding compat: scheduler writes
      # sha2::Sha256::digest(path.as_bytes()) (db.rs:650, raw
      # 32-byte Vec<u8> → BYTEA); this query reads
      # sha256(convert_to(path, 'UTF8')) (PG builtin, raw 32-byte
      # bytea). Same input bytes → same digest → same BYTEA. If
      # either side hex-encoded, this would be 0 forever.
      #
      # The earlier builds (out_pin/out_victim) used the default
      # key (empty comment → tenant_id=None → upsert skipped) —
      # path_tenants is currently empty. We set up a tenant key
      # now, bounce the gateway to load it, and do ONE build.

      # Tenant key with non-empty comment. Gateway's
      # load_authorized_keys parses the comment as tenant_name.
      client.succeed(
          "ssh-keygen -t ed25519 -N ''' -C 'gc-tenant-test' "
          "-f /root/.ssh/id_gc_tenant"
      )
      default_pub = client.succeed("cat /root/.ssh/id_ed25519.pub").strip()
      tenant_pub = client.succeed("cat /root/.ssh/id_gc_tenant.pub").strip()

      # Re-patch the Secret with BOTH keys. The default key must
      # stay — refs-end-to-end runs next and uses build() (default
      # key). printf '%s\n%s\n' writes both on separate lines;
      # pubkeys are base64+space+comment, no single-quotes → safe
      # in shell single-quotes.
      k3s_server.succeed(
          f"printf '%s\n%s\n' '{default_pub}' '{tenant_pub}' "
          f"> /tmp/ak_both && "
          "k3s kubectl -n ${ns} create secret generic rio-gateway-ssh "
          "--from-file=authorized_keys=/tmp/ak_both "
          "--dry-run=client -o yaml | k3s kubectl apply -f -"
      )

      # Scale-bounce gateway 0→1 — see fixture.bounceGatewayForSecret
      # (k3s-full.nix) for the SecretManager reflector-refcount
      # rationale. Inlined here (not interpolated via nix) because the
      # fixture is written for 4-space Python indent (sshKeySetup's
      # level) and nix heredoc-strip doesn't re-indent on
      # interpolation — splicing at 10-space indent breaks Python.
      k3s_server.succeed(
          "k3s kubectl -n ${ns} scale deploy/rio-gateway --replicas=0"
      )
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${ns} get pods "
          "-l app.kubernetes.io/name=rio-gateway "
          "--no-headers 2>/dev/null | grep -q .",
          timeout=90,
      )
      k3s_server.succeed(
          "k3s kubectl -n ${ns} scale deploy/rio-gateway --replicas=1"
      )
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${ns} rollout status deploy/rio-gateway --timeout=60s",
          timeout=90,
      )
      # kube-proxy endpoint sync lag — poll SSH banner. Same
      # rationale as fixture.sshKeySetup (k3s-full.nix): nc -z
      # only proves kube-proxy has a DNAT rule, not that the
      # gateway's SSH accept loop is responding end-to-end.
      # Banner grep proves the full chain. `|| true` guards
      # pipefail against nc's idle-timeout exit.
      client.wait_until_succeeds(
          "(${pkgs.netcat}/bin/nc -w2 k3s-server 32222 "
          "</dev/null 2>&1 || true) | grep -q ^SSH-",
          timeout=30,
      )

      # Seed tenant row (FK: path_tenants.tenant_id →
      # tenants.tenant_id). INSERT…RETURNING via psql_k8s (-qtA).
      tenant_uuid = psql_k8s(k3s_server,
          "INSERT INTO tenants (tenant_name) VALUES ('gc-tenant-test') "
          "RETURNING tenant_id"
      )
      k3s_server.log(f"path_tenants: seeded tenant gc-tenant-test = {tenant_uuid}")

      # SSH Host alias for the tenant key. common.nix:362: ?ssh-key=
      # URL param is unreliable across Nix versions; the Host alias
      # in /root/.ssh/config overrides IdentityFile while inheriting
      # nothing (explicit User/Port). programs.ssh.extraConfig went
      # to /etc/ssh/ssh_config (system); per-user config wins.
      #
      # IdentitiesOnly yes: without it ssh offers ~/.ssh/id_ed25519
      # (default key, empty comment) FIRST, gateway accepts THAT →
      # tenant_id=None → upsert never fires. The build succeeds
      # silently with the wrong identity.
      client.succeed(
          "cat >> /root/.ssh/config << 'EOF'\n"
          "Host k3s-server-tenant\n"
          "  HostName k3s-server\n"
          "  User rio\n"
          "  Port 32222\n"
          "  IdentityFile /root/.ssh/id_gc_tenant\n"
          "  IdentitiesOnly yes\n"
          "  StrictHostKeyChecking no\n"
          "  UserKnownHostsFile /dev/null\n"
          "EOF"
      )

      # Build tenantDrv via the tenant-key alias. Fresh marker →
      # fresh derivation → fresh build (no DAG-dedup). Completion
      # sees tenant_id=Some(uuid) → upsert fires. store_url routes
      # through the k3s-server-tenant ssh_config alias (tenant key
      # via IdentitiesOnly); strip_to_store_path (the default under
      # capture_stderr=True) skips SSH known_hosts warnings.
      out_tenant = build("${tenantDrv}",
                         store_url="ssh-ng://k3s-server-tenant")
      assert out_tenant.startswith("/nix/store/"), (
          f"tenant-key build should produce store path: {out_tenant!r}"
      )
      assert "lifecycle-gc-tenant" in out_tenant, (
          f"wrong drv (DAG-dedup?): {out_tenant!r}"
      )

      # THE assertion. ≥1 not ==1: composite PK is (hash, tenant),
      # and this is the only tenant in PG, so it's exactly 1 in
      # practice. ≥1 per plan-0206 T5 spec — future test paths
      # sharing this output shouldn't break us. Query matches on
      # BOTH hash and tenant_id to prove the row is OURS (not a
      # stray from some other tenant).
      pt_count = int(psql_k8s(k3s_server,
          f"SELECT COUNT(*) FROM path_tenants "
          f"WHERE store_path_hash = sha256(convert_to('{out_tenant}', 'UTF8')) "
          f"AND tenant_id = '{tenant_uuid}'"
      ))
      assert pt_count >= 1, (
          f"path_tenants should have >=1 row for out_tenant after "
          f"tenant-key build (completion hook fires upsert); got "
          f"{pt_count}. Check: gateway loaded id_gc_tenant? scheduler "
          f"resolved 'gc-tenant-test'? completion.rs filter_map yielded?"
      )
      print(f"path_tenants PASS: {pt_count} row(s) for {out_tenant} "
            f"tenant={tenant_uuid} — completion hook + hash compat proven")

      # ── tenant retention extends global grace (mark CTE seed f) ──
      # Reuses out_tenant + tenant_uuid from the upsert proof above.
      # tenant row at :1232 used DEFAULT gc_retention_hours=168 (7d).
      # out_tenant is mkTrivial → refs=[] → no transitive protection.
      # path_tenants.first_referenced_at is ~now() (just upserted).
      #
      # Two-phase proof:
      #   1. CONTROL (survives): backdate narinfo.created_at past 24h
      #      grace. first_referenced_at stays at ~now() → inside 168h
      #      → seed (f) protects → pathsCollected=0.
      #   2. EXPIRED (swept): backdate first_referenced_at past 168h
      #      too. Both windows expired → seed (f) no longer fires →
      #      pathsCollected=1.
      #
      # The survival case IS the spec assertion: tenant retention
      # EXTENDS global grace. Without seed (f), step 1 would collect
      # out_tenant (past grace, no pin, no refs, no other seed).

      # Backdate narinfo past grace. 25h past a 24h grace.
      # first_referenced_at stays fresh — the completion hook wrote
      # it moments ago.
      psql_k8s(k3s_server,
          f"UPDATE narinfo SET created_at = now() - interval '25 hours' "
          f"WHERE store_path = '{out_tenant}'"
      )

      # TriggerGC grace=24h. out_tenant is past global grace but
      # inside tenant retention. Everything else (out_pin, busybox
      # seed, etc.) is still in-grace (built this run, created_at
      # ~now()). So the unreachable set is {} — IF seed (f) works.
      # Without seed (f): unreachable={out_tenant}, pathsCollected=1
      # → nix path-info fails → THIS assertion catches it.
      def trigger_gc_get_collected():
          out = sched_grpc(
              '{"dry_run": false, "grace_period_hours": 24, "force": true}',
              "rio.admin.AdminService/TriggerGC",
          )
          msgs = grpcurl_json_stream(out)
          finals = [m for m in msgs if m.get("isComplete")]
          assert finals, f"no isComplete in GC stream: {out[:500]}"
          # proto3 omits zero-value uint64; .get → "0" default.
          return finals[-1].get("pathsCollected", "0")

      collected_control = trigger_gc_get_collected()
      assert collected_control == "0", (
          f"tenant retention CONTROL: out_tenant past global grace "
          f"but inside 168h tenant window — seed (f) must protect it. "
          f"Got pathsCollected={collected_control!r}. If this is '1', "
          f"mark CTE seed (f) isn't firing — check path_tenants JOIN."
      )
      # Belt-and-suspenders: path still resolvable.
      client.succeed(
          f"nix path-info --store 'ssh-ng://k3s-server' {out_tenant}"
      )
      print("tenant-retention CONTROL PASS: out_tenant past global "
            "grace but inside tenant window → survived sweep")

      # Now expire the tenant window too. 200h > 168h retention.
      # Match on BOTH hash and tenant_id — same specificity as the
      # upsert-proof query at :1290 (defensive against future test
      # paths sharing this output via a different tenant).
      psql_k8s(k3s_server,
          f"UPDATE path_tenants "
          f"SET first_referenced_at = now() - interval '200 hours' "
          f"WHERE store_path_hash = sha256(convert_to('{out_tenant}', 'UTF8')) "
          f"AND tenant_id = '{tenant_uuid}'"
      )

      # Both windows expired. No pin, no refs, no other seed →
      # out_tenant is the sole unreachable path → pathsCollected=1.
      collected_expired = trigger_gc_get_collected()
      assert collected_expired == "1", (
          f"tenant retention EXPIRED: both global grace AND tenant "
          f"window expired — out_tenant must be swept. Got "
          f"pathsCollected={collected_expired!r}. If '0', seed (f) "
          f"WHERE clause may be inverted or retention default drifted."
      )
      client.fail(
          f"nix path-info --store 'ssh-ng://k3s-server' {out_tenant}"
      )
      print("tenant-retention EXPIRED PASS: both windows expired → "
            "out_tenant swept")

      print("gc-sweep PASS: pin protected, backdated path deleted, unpin round-trip OK")
''
