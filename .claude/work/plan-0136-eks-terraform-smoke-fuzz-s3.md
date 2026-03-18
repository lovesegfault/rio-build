# Plan 0136: EKS terraform + smoke-test + fuzz corpus S3 + docs checkoff

## Design

The last of the initial feature burst. Three unrelated infrastructure pieces that share one property: they all live outside the Rust workspace.

**EKS terraform (`infra/eks/`):** VPC (3 AZ, NAT gateway), EKS managed cluster, two node groups — `system` (m5.large×3, for scheduler/store/controller/gateway) and `workers` (c6a.xlarge, 2-10 replicas, tainted `rio.build/worker:NoSchedule`). The worker launch template sets `metadata_options { http_put_response_hop_limit = 1 }` — IMDSv2 with hop-limit 1 blocks pods from reaching the instance metadata endpoint (credential theft vector). IRSA module creates the IAM role for `rio-store` ServiceAccount → S3 access without node-wide instance profile.

**smoke-test.sh:** deploys `overlays/eks` (with `envsubst` for the IRSA role ARN), waits for control-plane Ready, configures gateway SSH `authorized_keys`, builds `nixpkgs#hello` via `ssh-ng://`, captures `rio_scheduler_worker_disconnects_total` BEFORE killing a worker pod (lesson G6 from phase 3a — you need the baseline before the perturbation), deletes the pod, waits for build completion, asserts the metric delta. Reassignment proved end-to-end. Manual trigger only — real AWS resources, not in `.#ci`.

**Fuzz corpus S3 (`nix/fuzz.nix`):** `mkFuzzCheck` gets `s3CorpusSync` param. Nightly targets (not the 30s PR smoke) sync corpus from/to `$RIO_FUZZ_CORPUS_S3/<target>/` around the run. `awscli2` buildInput. `|| true` on sync failure — fuzz should still run if S3 is unreachable. PR CI may lack AWS creds; corpus persistence is a nice-to-have, not a gate.

**Docs checkoff (`3f67562`):** removed all `> **Phase 3b deferral:**` blocks from `security.md`, `errors.md`, `configuration.md`, `deployment.md`, `failure-modes.md` — mTLS, HMAC, recovery, backstop all implemented. Retagged remaining `TODO(phase3b)` → `TODO(phase4)` for `PutChunk` (stubbed UNIMPLEMENTED), `store_size_bytes`, FUSE circuit breaker, fine-grained BuildStatus conditions. `cad2bb4` fixed rustdoc warnings (unclosed HTML tag from `<Claims>`, broken intra-doc link).

**vm-phase3b iteration 1 scaffold:** `nix/tests/phase3b.nix` with 2 VMs (control + client), plaintext (no k3s yet). Single test section G1 (`__noChroot` rejection) end-to-end via `nix-build` fail + message grep. Iteration 2 (T/B/A/S/C/F/E/D sections) marked TODO in the file — lands in P0137.

## Files

```json files
[
  {"path": "infra/eks/main.tf", "action": "NEW", "note": "VPC + EKS + nodegroups + IRSA"},
  {"path": "infra/eks/variables.tf", "action": "NEW", "note": "region, cluster_name, worker sizing"},
  {"path": "infra/eks/outputs.tf", "action": "NEW", "note": "cluster endpoint, IRSA role ARN"},
  {"path": "infra/eks/smoke-test.sh", "action": "NEW", "note": "deploy + build hello + kill worker + verify metric delta"},
  {"path": "infra/eks/README.md", "action": "NEW", "note": "terraform apply instructions"},
  {"path": "nix/fuzz.nix", "action": "MODIFY", "note": "s3CorpusSync param + awscli2 buildInput"},
  {"path": "nix/tests/phase3b.nix", "action": "NEW", "note": "iteration 1 scaffold — G1 section only"},
  {"path": "flake.nix", "action": "MODIFY", "note": "add phase3b to vmTests"},
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "retag store_size_bytes TODO phase3b→phase4"},
  {"path": "rio-scheduler/src/actor/tests/recovery.rs", "action": "MODIFY", "note": "test stability fix"},
  {"path": "rio-store/src/grpc/chunk.rs", "action": "MODIFY", "note": "retag PutChunk TODO phase3b→phase4"},
  {"path": "rio-store/src/backend/chunk.rs", "action": "MODIFY", "note": "rustdoc link fix"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "rustdoc link fix"},
  {"path": "rio-scheduler/src/actor/recovery.rs", "action": "MODIFY", "note": "rustdoc <Claims> tag escape"},
  {"path": "docs/src/phases/phase3b.md", "action": "MODIFY", "note": "task list [x], deferred→phase4 section"},
  {"path": "docs/src/security.md", "action": "MODIFY", "note": "remove mTLS/HMAC deferral blocks"},
  {"path": "docs/src/errors.md", "action": "MODIFY", "note": "remove K8s-retry deferral block"},
  {"path": "docs/src/configuration.md", "action": "MODIFY", "note": "remove TLS config deferral block"},
  {"path": "docs/src/deployment.md", "action": "MODIFY", "note": "remove IRSA/NLB deferral + add IMDSv2 runbook"},
  {"path": "docs/src/failure-modes.md", "action": "MODIFY", "note": "remove recovery/backstop deferral blocks"},
  {"path": "docs/src/runbooks/eks-smoke.md", "action": "NEW", "note": "smoke-test.sh walkthrough"}
]
```

## Tracey

No tracey markers landed in these commits. This is infrastructure and docs — no normative spec requirements with `r[...]` identifiers.

## Entry

- Depends on P0127: phase 3a complete.
- Depends on P0132: `deploy/overlays/eks/` kustomization base exists.

## Exit

Merged as `d15dcd1..cad2bb4` (3 commits). `.#ci` green at merge — vm-phase3b iteration 1 passes (G1 only). EKS smoke-test.sh is manual trigger, not in `.#ci`. 961 total tests at this point.
