# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # pool-lifecycle — apply Builder+Fetcher Pools → status → delete
  # ══════════════════════════════════════════════════════════════════
  # D3: single Pool CRD with spec.kind discriminator. Apply one of
  # each kind → reconciler patches .status (replicas + conditions) →
  # delete → finalizer + ownerRef GC clean up Jobs.
  with subtest("pool-lifecycle: apply Builder+Fetcher → status → delete"):
      # ── Apply Builder + Fetcher Pools ─────────────────────────────
      # privileged:true matches the fixture (vmtest-full.yaml) for
      # Builder. Fetcher omits privileged/hostUsers/seccompProfile —
      # CRD CEL forbids them; the reconciler forces ADR-019 hardening.
      k3s_server.succeed(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: Pool\n"
          "metadata:\n"
          "  name: test-builder\n"
          "  namespace: ${nsBuilders}\n"
          "spec:\n"
          "  kind: Builder\n"
          "  image: rio-builder\n"
          "  systems: [x86_64-linux]\n"
          "  privileged: true\n"
          "---\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: Pool\n"
          "metadata:\n"
          "  name: test-fetcher\n"
          "  namespace: ${nsFetchers}\n"
          "spec:\n"
          "  kind: Fetcher\n"
          "  image: rio-builder\n"
          "  systems: [x86_64-linux, builtin]\n"
          "EOF"
      )

      # ── Status appears (reconciler patched) ───────────────────────
      # Finalizer added + .status.conditions written on first
      # reconcile (~10s tick). 30s absorbs apiserver admission +
      # controller watch latency.
      for ns, name in [("${nsBuilders}", "test-builder"), ("${nsFetchers}", "test-fetcher")]:
          k3s_server.wait_until_succeeds(
              f"k3s kubectl -n {ns} get pool {name} "
              "-o jsonpath='{.status.conditions[0].type}' | grep -q SchedulerUnreachable",
              timeout=30,
          )
          import json as _json
          p = _json.loads(kubectl(f"get pool {name} -o json", ns=ns))
          assert "pool.rio.build/drain" in p["metadata"].get("finalizers", []), (
              f"{name}: finalizer not added"
          )
      print("pool-lifecycle: both kinds reconciled, finalizer + status present")

      # ── CEL: Fetcher rejects privileged ───────────────────────────
      # Admission-time rejection of an ADR-019-forbidden field.
      rc, out = k3s_server.execute(
          "k3s kubectl apply -f - <<'EOF'\n"
          "apiVersion: rio.build/v1alpha1\n"
          "kind: Pool\n"
          "metadata: {name: cel-reject, namespace: ${nsFetchers}}\n"
          "spec:\n"
          "  kind: Fetcher\n"
          "  image: rio-builder\n"
          "  systems: [x86_64-linux]\n"
          "  privileged: true\n"
          "EOF"
      )
      assert rc != 0, (
          "CEL should reject kind=Fetcher with privileged:true at "
          f"admission; apply succeeded:\n{out}"
      )
      assert "Fetcher forbids privileged" in out, (
          f"CEL rejection message missing rule context:\n{out}"
      )

      # ── Delete → Pools GC'd ───────────────────────────────────────
      # cleanup() is a no-op (Jobs carry ownerRef); finalizer removed
      # immediately.
      kubectl("delete pool test-builder --wait=false", ns="${nsBuilders}")
      kubectl("delete pool test-fetcher --wait=false", ns="${nsFetchers}")
      for ns, name in [("${nsBuilders}", "test-builder"), ("${nsFetchers}", "test-fetcher")]:
          k3s_server.wait_until_succeeds(
              f"! k3s kubectl -n {ns} get pool {name} 2>/dev/null",
              timeout=60,
          )

      print("pool-lifecycle PASS: Builder+Fetcher reconciled + CEL + GC'd")
''
