# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # bootstrap-job-ran — PSA-restricted exec + no EROFS
  # ══════════════════════════════════════════════════════════════════
  # Prod-parity fixture only (k3s-prod-parity.nix sets
  # bootstrap.enabled=true). vmtest-full.yaml's default is false —
  # the Job never renders under the base k3s-full fixture, so this
  # fragment under a non-prod-parity fixture would wait forever at
  # the Job-exists check.
  #
  # The bootstrap script (nix/docker.nix bootstrap attr) does:
  #   1. env-check (`: ''${AWS_REGION:?} ''${CHUNK_BUCKET:?}`)
  #   2. `aws secretsmanager describe-secret` → awscli2 init,
  #      may write $HOME/.aws/cli/cache/
  #   3. if not-found: `openssl rand 32 > /tmp/hmac`
  #   4. `aws secretsmanager create-secret` → UNREACHABLE in the
  #      airgapped VM (no IRSA, no endpoint) → set -e exits nonzero
  #
  # Step 4 means the Job NEVER reaches Complete here. That's
  # EXPECTED — we're testing PSA compatibility, not AWS. The
  # a28e4b65 regression was awscli2 writing $HOME/.aws/ with HOME
  # unset → falls back to / → tries /.aws → EROFS under
  # readOnlyRootFilesystem. The fix (HOME=/tmp) means step 2-3 run
  # without EROFS; step 4 fails with "Unable to locate credentials"
  # or "Could not connect". We assert:
  #   - Pod spec has readOnlyRootFilesystem=true (PSA rendered)
  #   - Logs contain "[bootstrap] generating rio/hmac" (past
  #     env-check + awscli2 describe-secret returned not-found)
  #   - Logs DON'T contain "Read-only file system" (HOME=/tmp fix)
  #
  # Tracey: r[verify sec.psa.control-plane-restricted] at
  # default.nix subtests entry.
  with subtest("bootstrap-job-ran: PSA-restricted exec + no EROFS"):
      # Job must exist (proves bootstrap.enabled=true rendered).
      # The Job's pod may still be running its first attempt or
      # already in backoff — we don't gate on Job status here,
      # just on its existence. k3s applies 02-workloads.yaml
      # during waitReady; by the time waitReady returns, the
      # Job object is in etcd.
      kubectl("get job rio-bootstrap")

      # Pod spec: readOnlyRootFilesystem=true proves the
      # rio.containerSecurityContext helper rendered PSA-
      # restricted. Without it, the fragment proves nothing
      # (no-readOnlyRoot → no EROFS possible → hollow test).
      # jsonpath on the Job's pod-template, not a running pod —
      # the pod may already be gone (backoff) but the template
      # persists.
      rorfs = kubectl(
          "get job rio-bootstrap -o jsonpath="
          "'{.spec.template.spec.containers[0].securityContext"
          ".readOnlyRootFilesystem}'"
      ).strip()
      assert rorfs == "true", (
          f"bootstrap Job pod-template must have "
          f"readOnlyRootFilesystem=true (PSA-restricted); got "
          f"{rorfs!r}. If this fails, rio.containerSecurityContext "
          f"(_helpers.tpl) isn't being included, or PSA was "
          f"bumped to privileged (coverage mode does this — "
          f"prod-parity fixture shouldn't)."
      )

      # HOME=/tmp env proves the a28e4b65 fix is present. Without
      # it, awscli2 falls back to HOME=/ under UID 65532.
      home = kubectl(
          "get job rio-bootstrap -o jsonpath="
          "\"{.spec.template.spec.containers[0].env[?(@.name=='HOME')].value}\""
      ).strip()
      assert home == "/tmp", (
          f"bootstrap Job should set HOME=/tmp (a28e4b65 fix); "
          f"got {home!r}. awscli2 writes cache to $HOME/.aws/ — "
          f"unset HOME → / → EROFS under readOnlyRootFilesystem."
      )

      # Wait for the Job to reach a terminal state. backoffLimit=2
      # → 3 pod attempts; each runs ~5-20s (awscli2 credential
      # chain: env→file→IMDS, IMDS probe timeout ~3s, then
      # describe-secret fails → else branch → echo → openssl →
      # create-secret also fails). With exponential backoff
      # (10s, 20s) between retries: ~(20+10+20+20+20) ≈ 90s to
      # Failed. 240s timeout is generous.
      #
      # Why not `kubectl logs job/NAME`: it picks the MOST RECENT
      # pod, which during backoff may still be ContainerCreating
      # → empty logs while the earlier (terminated) pods DO have
      # logs. Why not `grep -q bootstrap` as the wait: kubectl's
      # own stderr includes "using pod/rio-bootstrap-NNNN" → the
      # grep would match THAT, not script output, returning 10s
      # early while the newest pod is still Creating.
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${ns} wait --for=condition=Failed "
          "job/rio-bootstrap --timeout=10s",
          timeout=240,
      )

      # Logs from ALL bootstrap pods (label-selector, --prefix
      # tags each line with [pod/NAME]). All three are terminated
      # now (Job is Failed); at least one will have non-empty
      # logs. --tail=-1: everything (default is last 10 lines
      # for label-selector mode, which would miss the early
      # echo if the aws error is verbose).
      logs = k3s_server.succeed(
          "k3s kubectl -n ${ns} logs "
          "-l app.kubernetes.io/name=rio-bootstrap "
          "--prefix --tail=-1 2>&1"
      )
      print(f"bootstrap-job-ran: logs:\n{logs}")

      # Also dump pod terminal state for triage: exit code +
      # reason tells us WHERE the script died. exitCode=2 ≈
      # bash `set -e` abort; exitCode=1 ≈ explicit exit 1;
      # reason=OOMKilled ≈ awscli2 blew past memory.
      term = k3s_server.succeed(
          "k3s kubectl -n ${ns} get pod "
          "-l app.kubernetes.io/name=rio-bootstrap "
          "-o jsonpath="
          "'{range .items[*]}{.metadata.name} "
          "exit={.status.containerStatuses[0].state.terminated.exitCode} "
          "reason={.status.containerStatuses[0].state.terminated.reason}"
          "{\"\\n\"}{end}'"
      )
      print(f"bootstrap-job-ran: pod terminal states:\n{term}")

      # P0493 regression signature. The whole point of this
      # fragment. openssl's `> /tmp/hmac` redirect would emit
      # this via bash; awscli2's mkdir $HOME/.aws would emit it
      # via Python's OSError. Either way: fix verbatim,
      # unmistakable.
      assert "Read-only file system" not in logs, (
          f"bootstrap hit EROFS — P0493 regression. HOME=/tmp "
          f"should have routed awscli2's cache + the script's "
          f"/tmp/hmac write to the emptyDir mount. Logs:\n{logs}"
      )

      # Script progressed past the env-check (:? guards) AND past
      # the describe-secret call (returned not-found → fell into
      # the else branch → printed this line before openssl).
      # If this assert fires without EROFS, check: AWS_REGION/
      # CHUNK_BUCKET set? (k3s-prod-parity.nix extraValues)
      # Or did awscli2 hang >timeout on connection-refused?
      assert "[bootstrap] generating rio/hmac" in logs, (
          f"bootstrap script should have progressed to the "
          f"openssl path (proves env-check passed + awscli2 init "
          f"ran + describe-secret returned not-found). If logs "
          f"show the env-check failure instead, prod-parity "
          f"fixture's global.region/chunkBackend.bucket overrides "
          f"aren't reaching the Job env. Logs:\n{logs}"
      )

      print(
          f"bootstrap-job-ran PASS: readOnlyRootFilesystem={rorfs}, "
          f"HOME={home}, no EROFS, script reached openssl path"
      )
''
