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
# The bootstrap script (nix/docker.nix bootstrap attr) calls
# `aws secretsmanager describe-secret` / `create-secret` — unreachable
# in the airgapped VM. The Job will FAIL at the aws create-secret step
# after backoffLimit retries. That's EXPECTED: we're testing PSA
# compatibility, not AWS integration. The bootstrap-job-ran subtest
# (scenarios/lifecycle.nix) asserts the script progressed past the
# env-check + awscli2 init + openssl /tmp write WITHOUT hitting EROFS.
flakeArgs@{ dockerImages, ... }:
let
  k3sFull = import ./k3s-full.nix flakeArgs;
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
      # rio.image helper (_helpers.tpl) builds `{repo}:{global.image.tag}`.
      # vmtest-full.yaml sets global.image.tag=dev; dockerImages.bootstrap
      # (nix/docker.nix) builds rio-bootstrap:dev. String match → pod pulls
      # from the airgap preload below.
      "bootstrap.image" = "rio-bootstrap";
    }
    // extraValues;
    # dockerImages.vmTestSeed covers rio-{gateway,scheduler,store,
    # controller,builder,fetcher} but NOT rio-bootstrap (k3s-full.nix —
    # "fod-proxy/bootstrap excluded"). Without this preload the Job pod goes
    # ImagePullBackOff (airgapped — no registry to pull from).
    extraImages = [ dockerImages.bootstrap ] ++ extraImages;
  }
)
