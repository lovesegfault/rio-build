# castore-e2e subtest fragment — composed by scenarios/castore-e2e.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # scope-denied — a sibling build's token cannot read another build's
  # closure-private object (ADR-022 P0591, enforce by default)
  # ══════════════════════════════════════════════════════════════════
  # Two builds, one tenant, disjoint closures: build "A" is chunk-warm,
  # which already streamed big_f (24 MiB) under the enforce default —
  # that subtest (and every other real build in this suite) passing IS
  # the "normal builds still work under enforce" half of the evidence.
  # Build "B" is a sibling whose closure is disjoint from A's: its
  # assignment token is minted HERE with the cluster's own assignment
  # HMAC key (the rio-hmac Secret the scheduler and store share), with
  # claims shaped exactly like dispatch mints them — same tenant, role
  # Builder (default), and input_closure_digest committing to B's
  # closure = [post_seed]. The store cannot tell it from a real
  # dispatch token, which is the point: a leaked/replayed sibling
  # credential. The probe:
  #   1. PresentClosure(B's closure) with B's token  → accepted
  #   2. StatBlob(post_seed digest)  (in B's closure) → served (positive
  #      control: the token, tenant join, and scope all work)
  #   3. StatBlob(big_f digest)  (in A's closure only) → NOT_FOUND, and
  #      the store's out_of_scope deny counter moves — the same object a
  #      tenant-wide token could read before closure scoping.
  # State-based only: no builds run during the probe, nothing is
  # mutated, so this cannot poison later subtests.
  with subtest("scope-denied: sibling token gets NOT_FOUND on another build's closure-private object"):
      import base64
      import hashlib
      import hmac as hmac_mod

      # The cluster's assignment-token HMAC key (scheduler signs, store
      # verifies). Byte-trim the trailing newline exactly like
      # rio-auth's load_key does.
      key = base64.b64decode(
          json.loads(kubectl("get secret rio-hmac -o json"))["data"]["hmac.key"]
      )
      if key.endswith(b"\r\n"):
          key = key[:-2]
      elif key.endswith(b"\n"):
          key = key[:-1]

      # Sibling build B's closure: just post_seed — disjoint from the
      # probed object (big_f). Real sibling closures share common deps;
      # what matters is the probed digest's containing path is NOT in
      # B's closure. blake3(sorted closure joined by \n) — a single
      # entry, so just blake3 of the path string (b3sum, no trailing
      # newline), exactly AssignmentClaims::digest_input_closure.
      sibling_closure = [p_post_seed]
      closure_digest = client.succeed(
          f"printf '%s' '{p_post_seed}' | ${b3sum} --no-names"
      ).strip()

      claims = {
          "executor_id": "vm-castore-scope-sibling",
          # No derivations row for this hash → non-terminal → the
          # terminal-revocation probe does not reject it (the sibling
          # build is "still running").
          "drv_hash": f"vm-scope-sibling-{int(time.time())}",
          "expected_outputs": [],
          "is_ca": False,
          "expiry_unix": int(time.time()) + 3600,
          "tenant": tenant_id,
          "input_closure_digest": closure_digest,
      }
      claims_json = json.dumps(claims).encode()
      tag = hmac_mod.new(key, claims_json, hashlib.sha256).digest()

      def b64u(raw):
          return base64.urlsafe_b64encode(raw).rstrip(b"=").decode()

      sibling_token = f"{b64u(claims_json)}.{b64u(tag)}"
      print(f"scope-denied: minted sibling token (closure_digest {closure_digest[:12]}…)")

      def store_directory_grpc(method, payload, ok_nonzero=False):
          """One DirectoryService call against rio-store (port 9002) as
          the sibling build, via port-forward + grpcurl."""
          return pf_exec("svc/rio-store", 9002,
              f"${grpcurl} ${grpcurlTls} -max-time 30 "
              f"-H 'x-rio-assignment-token: {sibling_token}' "
              f"-protoset ${protoset}/rio.protoset "
              f"-d '{json.dumps(payload)}' "
              f"localhost:__PORT__ rio.store.DirectoryService/{method}",
              ns="${nsStore}", ok_nonzero=ok_nonzero)

      def stat_blob_payload(hex_digest):
          return {
              "fileDigest": base64.b64encode(bytes.fromhex(hex_digest)).decode(),
              "sendChunks": True,
          }

      denied_before = series(
          store_metrics(), "rio_store_castore_scope_denied_total", must=("out_of_scope",)
      )

      # (1) Present B's closure. The store verifies it against the
      # token's signed digest and caches the ScopeSet — succeed() makes
      # any rejection fail the test here, with grpcurl's error visible.
      store_directory_grpc("PresentClosure", {"closure": sibling_closure})
      print("scope-denied: sibling closure presented and accepted")

      # (2) Positive control: an object IN B's closure is served to B's
      # token. Proves the minted token verifies, the tenant join passes,
      # and the presented scope is live — so the denial below can only
      # be the closure scope, not a broken credential.
      in_scope = store_directory_grpc("StatBlob", stat_blob_payload(b3_post_seed))
      assert "chunks" in in_scope, (
          f"scope-denied positive control: StatBlob of post_seed (in the sibling's "
          f"closure) should return its chunk window, got: {in_scope!r}"
      )
      print("scope-denied: positive control served (in-closure read works)")

      # (3) The negative probe: big_f's file digest is reachable only
      # through build A's closure path — same tenant, NOT in B's
      # presented closure. Under the enforce default the store must
      # answer exactly like an absent digest (NOT_FOUND, no oracle).
      out = store_directory_grpc(
          "StatBlob", stat_blob_payload(b3_big_f), ok_nonzero=True
      )
      assert "NotFound" in out, (
          f"scope-denied: expected NOT_FOUND for build A's big_f under the sibling "
          f"token (closure-scoped reads, enforce default), got: {out!r}"
      )
      assert "chunks" not in out, (
          f"scope-denied: the out-of-scope StatBlob must not leak chunk metadata: {out!r}"
      )

      # The deny is attributed for triage: the wire said NOT_FOUND, the
      # metric (and structured deny log) carry the real reason.
      denied_after = series(
          store_metrics(), "rio_store_castore_scope_denied_total", must=("out_of_scope",)
      )
      assert denied_after >= denied_before + 1, (
          f"scope-denied: rio_store_castore_scope_denied_total{{reason=out_of_scope}} "
          f"must increment for the denied read (before={denied_before}, "
          f"after={denied_after})"
      )
      print(
          f"scope-denied PASS: sibling token denied on A's object (NOT_FOUND, "
          f"out_of_scope {denied_before} → {denied_after}) while in-closure reads serve"
      )
''
