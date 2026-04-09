# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # jwt-mount-present — ConfigMap/Secret mounted, env var set
  # ══════════════════════════════════════════════════════════════════
  # Proves: with jwt.enabled=true, scheduler+store pods have the
  # rio-jwt-pubkey ConfigMap mounted at /etc/rio/jwt AND
  # RIO_JWT__KEY_PATH env set. Gateway has the rio-jwt-signing
  # Secret at /etc/rio/jwt.
  #
  # This closes the gap P0349 assumed was closed but wasn't: the
  # ConfigMap OBJECT existed (jwt-pubkey-configmap.yaml), the
  # volumeMount didn't. Without the mount, cfg.jwt.key_path stays
  # None → interceptor inert → every JWT passes unverified. Silent
  # fail-open when the operator thought jwt.enabled=true meant
  # enforcement.
  #
  # kubectl-get-jsonpath, NOT kubectl-exec. The rio-* images have
  # no coreutils (docker.nix baseContents = cacert+tzdata only —
  # minimal by design). `printenv`/`cat` aren't there. The Pod
  # spec (env + volumeMounts + volumes) IS the proof the Helm
  # template rendered the mount; K8s guarantees that if the Pod
  # is Running (waitReady proved this), the ConfigMap was mounted.
  # A bad mount = Pod stuck Pending with FailedMount event.
  #
  # The stronger proof — "scheduler successfully LOADED the key" —
  # is implicit: rio-scheduler/src/main.rs and rio-store/src/main.rs
  # call load_and_wire_jwt(...)? with `?` propagation. A bad key =
  # process exits non-zero = CrashLoopBackOff = waitReady never
  # returns. The prelude's waitReady already proved scheduler+store+
  # gateway are all Running, which means key-load succeeded in each.
  # (The gateway side loads the SIGNING seed, not the pubkey — same
  # fail-fast pattern via the same ?-propagation.)
  #
  # Precondition only: interceptor VERIFY behaviour is covered by
  # rust tests (jwt_interceptor.rs::tests); this proves the Helm
  # wiring → K8s Pod spec → container filesystem chain is intact.
  #
  # Tracey: r[verify sec.jwt.pubkey-mount] lives at the default.nix
  # subtests entry (P0341 convention — marker at wiring point, not
  # fragment header).
  with subtest("jwt-mount-present: scheduler+store+gateway have key mount + env"):
      # ── ConfigMap exists with content ─────────────────────────────
      # Template renders it, but WAS it applied? Was the content
      # non-empty? (empty → parse fails → scheduler CrashLoops →
      # waitReady catches that, but an explicit check is clearer).
      pubkey_cm = kubectl(
          "get cm rio-jwt-pubkey -o jsonpath='{.data.ed25519_pubkey}'"
      ).strip()
      assert len(pubkey_cm) >= 40, (
          f"rio-jwt-pubkey ConfigMap data too short: {pubkey_cm!r}. "
          f"32-byte key base64'd = 44 chars."
      )

      # ── Per-pod: env var + volumeMount + volume ───────────────────
      # Inspect the Pod spec (what K8s created from the Deployment
      # template). deploy/{name} resolves to the Deployment's Pod
      # template; `get pod -l ... -o jsonpath` reads a live pod.
      # Either proves the same thing; the pod spec is what the
      # kubelet actually used to build the container.
      for dep, dep_ns, vol_name, key_path in [
          ("rio-scheduler", "${ns}", "jwt-pubkey", "/etc/rio/jwt/ed25519_pubkey"),
          ("rio-store", "${nsStore}", "jwt-pubkey", "/etc/rio/jwt/ed25519_pubkey"),
          ("rio-gateway", "${ns}", "jwt-signing", "/etc/rio/jwt/ed25519_seed"),
      ]:
          # env: RIO_JWT__KEY_PATH set to the expected path.
          envs = kubectl(
              f"get deploy {dep} -o jsonpath="
              f"'{{.spec.template.spec.containers[0].env}}'",
              ns=dep_ns,
          )
          assert key_path in envs and "RIO_JWT__KEY_PATH" in envs, (
              f"{dep} missing RIO_JWT__KEY_PATH={key_path} in env "
              f"spec: {envs!r}"
          )

          # volumeMounts: named mount at /etc/rio/jwt.
          mounts = kubectl(
              f"get deploy {dep} -o jsonpath="
              f"'{{.spec.template.spec.containers[0].volumeMounts}}'",
              ns=dep_ns,
          )
          assert vol_name in mounts and "/etc/rio/jwt" in mounts, (
              f"{dep} missing {vol_name} volumeMount at "
              f"/etc/rio/jwt: {mounts!r}"
          )

          # volumes: entry exists. For scheduler/store it's a
          # configMap ref; gateway it's a secret ref. Just
          # checking the name — the ref type is template-defined
          # (_helpers.tpl), and helm-lint's yq check already
          # asserts the ref shape.
          vols = kubectl(
              f"get deploy {dep} -o jsonpath="
              f"'{{.spec.template.spec.volumes}}'",
              ns=dep_ns,
          )
          assert vol_name in vols, (
              f"{dep} missing {vol_name} volume: {vols!r}"
          )

      # ── Pods Running = key loaded successfully ────────────────────
      # waitReady already proved this, but make the inference
      # explicit. load_jwt_pubkey is fail-fast (`.await?` in
      # main.rs) — if the mounted key were invalid (bad base64,
      # wrong length, not a curve point), the process would exit
      # → CrashLoopBackOff → waitReady hangs. This subtest
      # reaching here = all three parsed their key OK.
      # ADR-019: store moved to rio-store namespace.
      for dep, dep_ns in [
          ("rio-scheduler", "${ns}"),
          ("rio-store", "${nsStore}"),
          ("rio-gateway", "${ns}"),
      ]:
          phase = kubectl(
              f"get pods -l app.kubernetes.io/name={dep} "
              f"-o jsonpath='{{.items[0].status.phase}}'",
              ns=dep_ns,
          ).strip()
          assert phase == "Running", (
              f"{dep} pod not Running (phase={phase!r}) — "
              f"load_jwt_pubkey likely failed (bad mount content?)"
          )
      print(
          "jwt-mount-present PASS: all 3 deployments have "
          "RIO_JWT__KEY_PATH + mount + volume; pods Running "
          "= load_jwt_pubkey succeeded"
      )
''
