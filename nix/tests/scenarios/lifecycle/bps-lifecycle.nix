# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # bps-lifecycle — apply BPS → 3 children → delete → children gone
  # ══════════════════════════════════════════════════════════════════
  # Proves the BuilderPoolSet reconciler end-to-end: apply a 3-class
  # BPS → reconciler's per-class loop (builderpoolset/mod.rs:131)
  # creates 3 child BuilderPools named `{bps}-{class}` → each child
  # carries sizeClass=class.name + ownerRef[0]=BuilderPoolSet
  # (controller=true). Delete BPS → finalizer cleanup() explicitly
  # deletes each child (mod.rs:375); ownerRef GC is the fallback.
  with subtest("bps-lifecycle: apply BPS → 3 children → delete → children gone"):
      # ── Apply 3-class BPS ─────────────────────────────────────────
      # poolTemplate.image=rio-builder + imagePullPolicy not-in-template
      # (builders.rs:137 hardcodes None). systems=[x86_64-linux]
      # (required by child WP CEL). resources are dummy — replicas.
      # min defaults to 0 (DEFAULT_MIN_REPLICAS in builders.rs:18)
      # so no pods scheduled; we only check CR structure.
      # privileged:true matches the fixture (vmtest-full.yaml).
      #
      # Heredoc-via-stdin, same pattern as ephemeral-pool's
      # BuilderPool apply. Inline YAML so the fragment is
      # self-contained (no external fixture file to drift).
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: BuilderPoolSet\n"
          "metadata:\n"
          "  name: test-bps\n"
          "  namespace: ${nsBuilders}\n"
          "spec:\n"
          "  classes:\n"
          "    - name: small\n"
          "      cutoffSecs: 60\n"
          "      resources:\n"
          "        requests: {cpu: '1', memory: 2Gi}\n"
          "    - name: medium\n"
          "      cutoffSecs: 300\n"
          "      resources:\n"
          "        requests: {cpu: '2', memory: 4Gi}\n"
          "    - name: large\n"
          "      cutoffSecs: 1800\n"
          "      resources:\n"
          "        requests: {cpu: '4', memory: 8Gi}\n"
          "  poolTemplate:\n"
          "    image: rio-builder\n"
          "    systems: [x86_64-linux]\n"
          "    privileged: true\n"
          "EOF"
      )

      # ── 3 children appear, correct sizeClass + ownerRef ───────────
      # child_name = "{bps}-{class.name}" (builders.rs:43). The
      # reconciler's .owns(BuilderPool) watch fires on CR create
      # (<1s); SSA-apply for each child is fast (no pod create
      # at replicas.min=0). 30s absorbs the reconcile tick +
      # 3× apiserver admission.
      for cls in ["small", "medium", "large"]:
          child = f"test-bps-{cls}"
          k3s_server.wait_until_succeeds(
              f"k3s kubectl -n ${nsBuilders} get builderpool {child} 2>/dev/null",
              timeout=30,
          )
          # Pull once, assert against parsed JSON — cheaper than
          # 4× kubectl roundtrips per child and keeps the ns kwarg
          # in one place (ADR-019: builderpools live in rio-builders).
          import json as _json
          bp = _json.loads(
              kubectl(f"get builderpool {child} -o json", ns="${nsBuilders}")
          )
          sc = bp["spec"].get("sizeClass")
          assert sc == cls, (
              f"expected {child}.spec.sizeClass={cls}, got {sc!r}. "
              f"builders.rs:114 sets size_class=class.name — if "
              f"these diverge, scheduler routing breaks."
          )
          owner = bp["metadata"]["ownerReferences"][0]
          assert owner["name"] == "test-bps", (
              f"expected {child} ownerRef[0].name=test-bps, "
              f"got {owner['name']!r}"
          )
          assert owner["kind"] == "BuilderPoolSet", (
              f"expected {child} ownerRef[0].kind=BuilderPoolSet, "
              f"got {owner['kind']!r}"
          )
      print("bps-lifecycle: 3 children created, sizeClass+ownerRef correct")

      # ── Delete BPS → children GC'd ────────────────────────────────
      # --wait=false: don't block on the BPS finalizer. cleanup()
      # (builderpoolset/mod.rs:371) explicitly deletes each child
      # with 404 tolerance. Each child then runs ITS OWN BuilderPool
      # finalizer (DrainWorker — trivially fast at replicas=0)
      # before K8s GC deletes the owned resources.
      kubectl("delete builderpoolset test-bps --wait=false", ns="${nsBuilders}")

      # BPS CR gone: finalizer removed → K8s deletes. 60s: cleanup
      # iterates 3 children × (delete RPC + child finalizer). At
      # replicas=0, child finalizers complete in <5s each (no pods
      # to drain). 60s absorbs k3s controller lag.
      k3s_server.wait_until_succeeds(
          "! k3s kubectl -n ${nsBuilders} get builderpoolset test-bps 2>/dev/null",
          timeout=60,
      )

      # Children gone. Either cleanup() deleted them explicitly OR
      # ownerRef GC caught them — both paths converge on "not found".
      # Checked per-child so a hang names which class stuck.
      for cls in ["small", "medium", "large"]:
          k3s_server.wait_until_succeeds(
              f"! k3s kubectl -n ${nsBuilders} get builderpool test-bps-{cls} 2>/dev/null",
              timeout=30,
          )

      print("bps-lifecycle PASS: 3 children created + GC'd on BPS delete")
''
