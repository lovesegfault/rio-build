# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # store-rollout — scheduler store_client survives store pod rollout
  # ══════════════════════════════════════════════════════════════════
  # Proves: the scheduler's long-held StoreServiceClient channel
  # transparently reconnects after a store Deployment rollout. Before
  # the connect_lazy fix, the scheduler's eager-connected Channel
  # cached the OLD pod IP at startup; when that pod terminated,
  # FindMissingPaths RPCs failed with connection-refused and never
  # recovered — the cache-check circuit breaker tripped and
  # SubmitBuild rejected with Unavailable until the scheduler pod
  # was ALSO restarted.
  #
  # connect_lazy (rio-proto/src/client/mod.rs) re-resolves DNS on
  # each reconnect + HTTP/2 keepalive detects half-open connections
  # within ~40s. The channel reconnects to the new pod without
  # scheduler intervention.
  #
  # Test shape:
  #   1. build pre-rollout → scheduler→store FindMissingPaths OK
  #   2. rollout restart deploy/rio-store → old pod terminates,
  #      new pod comes up with a DIFFERENT IP
  #   3. wait for new store pod ready
  #   4. build post-rollout → scheduler→store must reconnect
  #   5. assert cache_check_failures_total stayed low (≤ circuit
  #      breaker threshold of 5) and post-build succeeded
  #
  # Tracey: r[verify sched.store-client.reconnect] at default.nix
  # subtests entry (P0341 convention).
  with subtest("store-rollout: scheduler reconnects to new store pod"):
      # ── Baseline: pre-rollout build ───────────────────────────────
      # Proves the scheduler→store channel works at all. mkTrivial
      # derivation: DAG merge → FindMissingPaths cache-check →
      # dispatch → build → upload.
      out_pre = build("${rolloutPreDrv}", capture_stderr=False).strip()
      assert out_pre.startswith("/nix/store/"), (
          f"pre-rollout build should succeed: {out_pre!r}"
      )

      # Capture the current store pod name. After rollout restart,
      # the Deployment controller creates a NEW pod with a DIFFERENT
      # name (and IP). Seeing a different name proves the rollout
      # actually cycled the pod.
      old_store = kubectl(
          "get pod -l app.kubernetes.io/name=rio-store "
          "-o jsonpath='{.items[0].metadata.name}'",
          ns="${nsStore}",
      ).strip()
      print(f"store-rollout: pre-rollout store pod = {old_store}")

      # Baseline cache-check failure count. Should be 0 (or low if a
      # transient blip happened during prelude's waitReady).
      m = sched_metrics()
      pre_failures = metric_value(
          m, "rio_scheduler_cache_check_failures_total"
      ) or 0.0

      # ── Rollout restart the store ─────────────────────────────────
      # `rollout restart` patches the Deployment's pod template with
      # a kubectl.kubernetes.io/restartedAt annotation → triggers
      # rolling update → old pod terminates, new pod (new name, new
      # IP) comes up. This is the exact operation helm upgrade
      # performs when store config changes.
      kubectl("rollout restart deploy/rio-store", ns="${nsStore}")

      # Wait for rollout to complete. `rollout status` blocks until
      # the new ReplicaSet is fully Available. NOTE: Available ≠ old
      # pods deleted — the Terminating pod lingers through its
      # grace period, so a bare label-selector query can still
      # return it as .items[0] (P0489).
      # 120s: store pod startup = image pull (cached) + sqlx migrate
      # + listen, ~30-60s under KVM.
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${nsStore} rollout status "
          "deploy/rio-store --timeout=90s",
          timeout=180,
      )

      # Verify the pod actually cycled — new name ≠ old name. If
      # rollout restart was a no-op (shouldn't be, but sanity), the
      # test is hollow.
      # Filter on deletionTimestamp is None to exclude the old
      # Terminating pod. Can't use --field-selector=status.phase
      # for this: "Terminating" is metadata.deletionTimestamp!=null,
      # not a phase — status.phase stays Running until the
      # container process actually exits. jq isn't in the VM, so
      # parse in the test driver.
      import json as _json
      pods = _json.loads(k3s_server.succeed(
          "k3s kubectl -n ${nsStore} get pod "
          "-l app.kubernetes.io/name=rio-store -o json"
      ))
      new_store = next(
          p["metadata"]["name"] for p in pods["items"]
          if p["metadata"].get("deletionTimestamp") is None
      )
      assert new_store != old_store, (
          f"store pod should have cycled: old={old_store} new={new_store}"
      )
      print(f"store-rollout: post-rollout store pod = {new_store}")

      # ── Post-rollout build ────────────────────────────────────────
      # THE TEST. Before the connect_lazy fix, this hangs or fails:
      # scheduler's cached Channel points at old_store's IP → TCP
      # connection-refused → FindMissingPaths fails → after 5
      # consecutive failures circuit breaker opens → SubmitBuild
      # rejected with Unavailable. With lazy: next RPC re-resolves
      # DNS → new_store's IP → reconnect → build succeeds.
      #
      # Different marker than rolloutPreDrv → fresh derivation →
      # DAG merge runs cache-check (not a dedup hit).
      out_post = build("${rolloutPostDrv}", capture_stderr=False).strip()
      assert out_post.startswith("/nix/store/"), (
          f"post-rollout build should succeed WITHOUT scheduler "
          f"restart (connect_lazy reconnect): {out_post!r}"
      )

      # ── Bounded failure delta ─────────────────────────────────────
      # cache_check_failures_total may tick once or twice during the
      # rollout window (keepalive timeout detection ~40s can overlap
      # a build attempt). But it MUST stay below the circuit-breaker
      # threshold (5 consecutive) — if it doesn't, the breaker opened
      # and the post-build would have failed above. Belt-and-braces:
      # explicit numeric check.
      m = sched_metrics()
      post_failures = metric_value(
          m, "rio_scheduler_cache_check_failures_total"
      ) or 0.0
      delta = post_failures - pre_failures
      assert delta < 5, (
          f"cache-check failures should stay below breaker threshold "
          f"across rollout; delta={delta} (pre={pre_failures}, "
          f"post={post_failures}). If this trips, connect_lazy is "
          f"not re-resolving DNS — check tonic Channel reconnect "
          f"semantics."
      )

      print(
          f"store-rollout PASS: {old_store}→{new_store}, "
          f"post-build={out_post}, cache_check_failures delta={delta}"
      )
''
