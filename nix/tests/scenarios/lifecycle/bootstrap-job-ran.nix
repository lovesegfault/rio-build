# lifecycle subtest fragment — composed by scenarios/lifecycle.nix mkTest.
scope: with scope; ''
  # ══════════════════════════════════════════════════════════════════
  # bootstrap-job-ran — PSA-restricted exec, no EROFS, full converge
  # ══════════════════════════════════════════════════════════════════
  # Prod-parity fixture only (k3s-prod-parity.nix sets
  # bootstrap.enabled=true). vmtest-full.yaml's default is false —
  # the Job never renders under the base k3s-full fixture, so this
  # fragment under a non-prod-parity fixture would wait forever at
  # the Job-exists check.
  #
  # The bootstrap script (nix/docker.nix bootstrap attr) runs the
  # REAL awscli2 against the in-VM Secrets Manager mock
  # (k3s-prod-parity.nix: AWS_ENDPOINT_URL + dummy creds via
  # bootstrap.extraEnv):
  #   1. env-check (`: ''${AWS_REGION:?} ''${CHUNK_BUCKET:?}`)
  #   2. secret_state probes → GENUINE ResourceNotFoundException
  #      from the mock (the CLI maps the wire __type verbatim)
  #   3. openssl rand → /tmp (the a28e4b65 EROFS regression site)
  #   4. create-secret → mock stores it; signing keys via rio-cli
  #      keygen; host key via ssh-keygen — the Job COMPLETES.
  #
  # The a28e4b65 regression was awscli2 writing $HOME/.aws/ with HOME
  # unset → falls back to / → tries /.aws → EROFS under
  # readOnlyRootFilesystem. The fix (HOME=/tmp) plus the emptyDir
  # mount lets steps 2-4 run; completion under PSA-restricted is the
  # strongest form of the original assertion set. (History: before
  # round 17 there was no mock and no creds; describe-secret died
  # NoCredentials and the old raw `if aws describe` guard collapsed
  # it into "missing", so this subtest asserted "describe returned
  # not-found" while never exercising it. The round-17 fail-closed
  # probe exposed that; the mock makes the stated intent real.)
  #
  # Tracey: r[verify sec.psa.control-plane-restricted] at
  # default.nix subtests entry.
  with subtest("bootstrap-job-ran: PSA-restricted exec + full converge"):
      # Mock must be listening before the Job's first attempt can
      # converge (Restart=always; started at multi-user, long before
      # k3s applies 02-workloads — this is belt-and-suspenders).
      k3s_server.wait_for_open_port(5000)

      # Job must exist (proves bootstrap.enabled=true rendered).
      kubectl("get job rio-bootstrap")

      # Pod spec: readOnlyRootFilesystem=true proves the
      # rio.containerSecurityContext helper rendered PSA-
      # restricted. Without it, the fragment proves nothing
      # (no-readOnlyRoot → no EROFS possible → hollow test).
      # jsonpath on the Job's pod-template, not a running pod —
      # the template persists across pod churn.
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

      # The Job COMPLETES against the mock: probes return genuine
      # not-found, creates succeed, rio-cli keygen + ssh-keygen run
      # in-image. One pod attempt is ~10-25s (awscli2 init dominates);
      # 240s rides image-load + scheduling tail under load. If this
      # times out, the logs dump below tells you WHERE it died:
      # connect errors → mock/address (k3s-prod-parity.nix serverV6);
      # 'refusing to guess' → endpoint returned a non-not-found
      # error; 'command not found' → the image tool envelope
      # regressed (docker.nix bootstrap contents).
      k3s_server.wait_until_succeeds(
          "k3s kubectl -n ${ns} wait --for=condition=Complete "
          "job/rio-bootstrap --timeout=10s",
          timeout=240,
      )

      # Logs from ALL bootstrap pods (label-selector, --prefix
      # tags each line with [pod/NAME]; --tail=-1: everything —
      # the default last-10 in selector mode would drop the early
      # lines the asserts below need).
      logs = k3s_server.succeed(
          "k3s kubectl -n ${ns} logs "
          "-l app.kubernetes.io/name=rio-bootstrap "
          "--prefix --tail=-1 2>&1"
      )
      print(f"bootstrap-job-ran: logs:\n{logs}")

      # P0493 regression signature — the original point of this
      # fragment, still asserted verbatim.
      assert "Read-only file system" not in logs, (
          f"bootstrap hit EROFS — P0493 regression. HOME=/tmp "
          f"should have routed awscli2's cache + the script's "
          f"/tmp writes to the emptyDir mount. Logs:\n{logs}"
      )

      # Genuine not-found path: the hmac guard fell into its else
      # branch because the MOCK said ResourceNotFoundException —
      # with the fail-closed probe, any other failure aborts
      # 'refusing to guess' and the Job never completes.
      assert "[bootstrap] generating rio/hmac" in logs, (
          f"bootstrap should have taken the openssl path off a "
          f"genuine not-found (env-check passed + awscli2 reached "
          f"the mock + ResourceNotFoundException classified). "
          f"Logs:\n{logs}"
      )

      # Full convergence: the signing block ran rio-cli keygen
      # in-image and printed the trusted-public-keys line; the
      # host-key block ran ssh-keygen. Together with Complete,
      # these pin the image tool envelope end to end.
      assert "trusted-public-keys" in logs, (
          f"signing-key block should have printed the public key "
          f"line (rio-cli keygen ran in-image). Logs:\n{logs}"
      )
      assert "gateway host key fingerprint" in logs, (
          f"host-key block should have printed the fingerprint "
          f"(ssh-keygen ran in-image). Logs:\n{logs}"
      )

      print(
          f"bootstrap-job-ran PASS: readOnlyRootFilesystem={rorfs}, "
          f"HOME={home}, no EROFS, Job Complete (full converge "
          f"against the in-VM mock)"
      )
''
