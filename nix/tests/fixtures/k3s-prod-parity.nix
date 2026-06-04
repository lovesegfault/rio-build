# Prod-parity overlay on k3s-full: HA scheduler + bootstrap Job enabled.
#
# Three prod regressions (a28e4b65, abef66c7, 5b98e311) all had the
# same root cause: VM tests use minimal config; prod uses HA +
# bootstrap.enabled. k3s-full.nix's vmtest-full.yaml already runs
# scheduler.replicas=2 (line 99 — leader-election.nix needs it), but
# bootstrap.enabled=false means the bootstrap Job template never
# renders in CI. This overlay flips it on so the Job runs under
# PSA-restricted (readOnlyRootFilesystem + runAsNonRoot) and the
# a28e4b65 EROFS regression class is caught at merge-gate.
#
# The bootstrap script (nix/docker.nix bootstrap attr) talks to a
# faithful in-VM Secrets Manager mock (mock-secretsmanager.py on the
# k3s-server node; AWS_ENDPOINT_URL + dummy creds via
# bootstrap.extraEnv). The REAL awscli2 gets GENUINE
# ResourceNotFoundException wire shapes for the first-run probes,
# creates every secret, and the Job COMPLETES — bootstrap-job-ran
# asserts full convergence under PSA-restricted with no EROFS.
# History: before round 17 this fixture had no credentials at all;
# every describe died NoCredentials and the old raw `if aws describe`
# guard collapsed that into "missing", so the scenario asserted
# "describe returned not-found" while never exercising it. The
# round-17 fail-closed probe (refusing to guess on non-not-found
# errors) exposed the latent collapse; the mock makes the stated
# intent real.
flakeArgs@{ dockerImages, pkgs, ... }:
let
  k3sFull = import ./k3s-full.nix flakeArgs;
  # k3s-server's test-driver-assigned v6 address. Deterministic for
  # THIS fixture's node set (sorted: client-v6=1, edge=2, k3s-agent=3,
  # k3s-server=4, upstream-v6=5 → 2001:db8:1::4 — the same address
  # the apiserver advertises). If the node set changes, the
  # mock-reachable assert in bootstrap-job-ran fails loudly (the Job
  # logs show connect errors, not not-found).
  serverV6 = "2001:db8:1::4";
in
{
  extraValuesTyped ? { },
  extraValues ? { },
  extraImages ? [ ],
  ...
}@innerArgs:
k3sFull (
  innerArgs
  // {
    extraValuesTyped = {
      # Already vmtest-full.yaml's default (line 99), stated here so
      # prod-parity intent is explicit and holds even if the base
      # values file ever reverts. scheduler.replicas = 2 → one leader
      # + one standby → leader-guard path reachable.
      "scheduler.replicas" = 2;
      # bootstrap.enabled = true renders bootstrap-job.yaml (SA + Job,
      # both helm.sh/hook annotated — under `helm template` those are
      # just metadata; k3s applies SA in 01-rbac then Job in
      # 02-workloads per the yq kind-split in helm-render.nix).
      "bootstrap.enabled" = true;
    }
    // extraValuesTyped;
    extraValues = {
      # Bootstrap script line 385 (nix/docker.nix): `: ${AWS_REGION:?}
      # ${CHUNK_BUCKET:?}` — bash `:?` exits immediately on empty. The
      # Job template pulls AWS_REGION from .Values.global.region and
      # CHUNK_BUCKET from .Values.store.chunkBackend.bucket; both are
      # unset in vmtest-full.yaml (region="", chunkBackend.kind=inline
      # → no bucket). Dummy values let the script progress to the
      # awscli2 init + openssl /tmp write where the EROFS regression
      # would manifest. The `aws secretsmanager` call after that still
      # fails (no creds, no endpoint) — bootstrap-job-ran expects it.
      "global.region" = "vm-test";
      "store.chunkBackend.bucket" = "vm-test-bucket";
      # Real awscli2 against the in-VM mock: dummy SigV4 creds (the
      # mock doesn't verify signatures — they just satisfy the
      # credential chain so the CLI signs and sends) + the explicit
      # endpoint (which awscli2 honors over AWS_USE_DUALSTACK_ENDPOINT
      # — explicit URLs skip endpoint resolution entirely).
      "bootstrap.extraEnv[0].name" = "AWS_ACCESS_KEY_ID";
      "bootstrap.extraEnv[0].value" = "vm-test-dummy-key";
      "bootstrap.extraEnv[1].name" = "AWS_SECRET_ACCESS_KEY";
      "bootstrap.extraEnv[1].value" = "vm-test-dummy-secret";
      "bootstrap.extraEnv[2].name" = "AWS_ENDPOINT_URL";
      "bootstrap.extraEnv[2].value" = "http://[${serverV6}]:5000";
      # rio.image helper (_helpers.tpl) builds `{repo}:{global.image.tag}`.
      # vmtest-full.yaml sets global.image.tag=dev; dockerImages.bootstrap
      # (nix/docker.nix) builds rio-bootstrap:dev. String match → pod pulls
      # from the airgap preload below.
      "bootstrap.image" = "rio-bootstrap";
    }
    // extraValues;
    # dockerImages.vmTestSeed covers rio-{gateway,scheduler,store,
    # controller,builder} but NOT rio-bootstrap (k3s-full.nix —
    # "bootstrap excluded"). Without this preload the Job pod goes
    # ImagePullBackOff (airgapped — no registry to pull from).
    extraImages = [ dockerImages.bootstrap ] ++ extraImages;
    # The Secrets Manager mock, listening on [::]:5000 of the server
    # node (pods reach node addresses via cilium host routing).
    # Stdlib-only python; Restart=always rides any blip; started at
    # multi-user so it's up long before k3s applies 02-workloads.
    extraServerModule = {
      systemd.services.mock-secretsmanager = {
        wantedBy = [ "multi-user.target" ];
        serviceConfig = {
          ExecStart = "${pkgs.python3}/bin/python3 ${./mock-secretsmanager.py}";
          Restart = "always";
          DynamicUser = true;
        };
      };
      networking.firewall.allowedTCPPorts = [ 5000 ];
    };
  }
)
