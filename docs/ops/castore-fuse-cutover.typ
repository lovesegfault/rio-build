#import "/lib/rio.typ": *

#show: rio.with(domains: none)

The castore-FUSE cutover (ADR-022, implementation plan §P0560) replaces the
builder's whole-NAR input substitution with a per-build @fuse filesystem that
serves individual files by digest: directory metadata comes from the store's
castore Directory DAG, file opens are passthrough reads from a node-local
shared object cache owned by `rio-mountd`, and only the files a build actually
touches are fetched (whole files below the streaming threshold, content-defined
chunks above it). See #cross-link("/spec/components/lazy-store.typ")[the lazy
  store filesystem chapter] for the design and
#cross-link("/spec/components/builder.typ")[the builder chapter] for the mount
sequence.

This is a *clean cutover*: the old FUSE implementation is deleted at the same
commit, there is no fallback flag and no store-format migration. Rolling
forward and rolling back are both greenfield redeploys. Plan the window
accordingly — every build in flight is lost and the data plane starts empty on
either direction.

= Prerequisites

+ *A cutover commit.* The deploy must come from a commit where the §P0560
  cutover and the §P0562 post-cutover audit have landed and the full checks
  gate (`nix-fast-build --flake .#checks.x86_64-linux`) is green — that gate
  includes the castore end-to-end VM tests (`vm-castore-e2e-core`,
  `vm-castore-e2e-faults`) and `vm-mountd`.
+ *Cache tier flipped.* The S3 Express read-through tier
  (#cross-link("/ops/tiered-cache-cutover.typ")[previous page]) should already
  be live. The castore read path works against plain S3-standard, but every
  cold chunk fetch then pays the standard-tier round-trip, so do the cheap
  flip first.
+ *NixOS node AMI from the same commit.* Castore passthrough needs kernel
  ≥ 6.9 with FUSE passthrough support; `nix/nixos-node/kernel.nix`
  asserts this at AMI evaluation, and the full `up` pipeline (its `--ami`
  phase) builds and registers the AMI the deploy then resolves. Do not reuse a
  pre-cutover AMI.
+ *HMAC Secrets in place.* The chart's bootstrap Job
  (#(refs.gh)("infra/helm/rio-build/templates/bootstrap-job.yaml")) generates
  `rio/hmac` and `rio/mountd-hmac` (plus signing and host keys) in Secrets
  Manager on first install, and the External Secrets sync materialises them as
  the `rio-hmac` Secret (rio-system + rio-store) and the `rio-mountd-hmac`
  Secret (rio-system + rio-builders). The EKS deploy pins
  `assignmentHmac.secretName=rio-hmac` and
  `mountdHmac.secretName=rio-mountd-hmac`. This is not optional hardening: the
  assignment token is the builder's only tenant credential on castore reads,
  so a missing `rio-hmac` makes every build fail closed at mount time
  ("DirectoryService requires a tenant"). If
  #(refs.metric)("rio_store_hmac_rejected_total") climbs right after the
  deploy, check the ExternalSecret status before anything else.

#info(title: [Pod isolation posture is unchanged])[
  Executor pods keep their current `hostUsers` setting through this cutover.
  The `rio-mountd` Mount-admission token (signed with the `rio-mountd-hmac`
  key) exists so that user-namespaced pods can be admitted later, but flipping
  builder pods to `hostUsers: false` is a separate follow-on gated on an EKS
  smoke test of the Mount round-trip from a real userns pod — the §P0559
  residual in the implementation plan. Do not bundle that flip into this
  window.
]

= Step 1: Capture the baseline

The post-cutover comparison needs a pre-cutover number, and there is no way to
recover it after the wipe (G6 lesson — capture before acting).

- Record the input-closure size of the validation workload. The pre-cutover
  path substitutes the full NAR of every input path, so the closure size is
  the bytes the old path transfers on a cold build:

  ```bash
  nix path-info -S nixpkgs#chromium   # closure NAR bytes (last column)
  ```

- If the current cluster already runs rio-build, note its Prometheus history
  (or screenshot the store/builder dashboards) for the same workload. The
  `--wipe` path below preserves the monitoring stack, so the old series stay
  queryable for a side-by-side; a full `destroy` does not.

= Step 2: Greenfield redeploy from the cutover commit

From a checkout of the cutover commit:

```bash
cargo xtask k8s -p eks up --wipe
```

`up --wipe` deletes the data plane — S3 chunks, the PostgreSQL schema,
tenants/builds, builder Jobs, the gateway `authorized_keys` Secret — and then
runs the full pipeline (AMI build/registration, image push, helm deploy) from
the working tree. Infra shape (VPC, RDS instance, the chunk bucket itself,
node pools, monitoring) is preserved. `cargo xtask k8s -p eks destroy --yes`
followed by a plain `up` is the heavier variant when the infra itself must be
rebuilt.

There is no index backfill step: `nar_index` and `directories` repopulate from
scratch as paths are uploaded — the chunked-upload commit writes the NAR index
inline, the legacy buffered path computes it eagerly after commit, and the
store's periodic indexer loop picks up anything that slipped through
(visible as #(refs.metric)("rio_store_nar_index_eager_total") /
#(refs.metric)("rio_store_nar_index_compute_total")).

After the deploy:

```bash
cargo xtask k8s -p eks qa --health
kubectl -n rio-builders rollout status ds/rio-mountd --timeout=120s
```

Re-grant any tenants/SSH keys beyond the operator default — the wipe dropped
them (`cargo xtask k8s -p eks grant <pubkey> --tenant <name>`).

= Step 3: Run a chromium-scale workload

Either (or both):

```bash
# one large closure end-to-end through the gateway
cargo xtask k8s -p eks rsb -- nixpkgs#chromium --no-link

# parallel cache-busted rebuilds of the nix-bench "large" tier
# (chromium + firefox + clang + kernel; needs ~/src/nix-bench/main checked out)
cargo xtask k8s -p eks qa --load --load-target large-shallow
```

The `large-shallow` tier rebuilds the top-level packages while every
dependency is substituted through the castore FUSE — exactly the
"build touches a small fraction of a huge input closure" shape the cutover is
meant to win on. Run the same workload at least twice: the first pass is the
cold-cache data point, the second exercises the warm node cache.

= Step 4: Expected outcome

Open the *rio-build: Castore FUSE* dashboard
(#(refs.gh)("infra/helm/rio-build/dashboards/castore-fuse.json")) via
`cargo xtask k8s -p eks grafana` and check, in order:

+ *Passthrough is actually negotiated.*
  #(refs.metric)("rio_builder_castore_fuse_open_mode_total")`{mode="passthrough"}`
  is non-zero while builds run. If it stays at zero, stop here and follow
  #cross-link("/ops/castore-fuse-triage.typ")[the triage page] — the fleet is
  silently running every read through FUSE upcalls.
+ *Remote fetch volume dropped.* Sum the remote JIT fetches over the workload
  window and compare against the Step-1 baseline:

  ```promql
  sum(increase(rio_builder_castore_fuse_fetch_bytes_total{hit="remote"}[1h]))
  ```

  The expectation — for builds that touch under \~10% of the files in their
  input closure (the chromium-class access pattern measured in ADR-022) — is a
  ≥10× reduction versus the whole-NAR baseline. Builds that read most of their
  inputs (link-everything, archive-everything jobs) see proportionally less;
  treat the 10× as the expected outcome under that precondition, not a
  guarantee the dashboard owes you on every workload.
+ *The node cache warms on repeat builds.* On the second pass the open-path
  hit share climbs and remote fetches shrink further. There is no ready-made
  ratio series; derive it from the dispatch counter:

  ```promql
  sum(rate(rio_builder_castore_fuse_open_case_total{case="hit"}[10m]))
    / sum(rate(rio_builder_castore_fuse_open_case_total[10m]))
  ```

  #(refs.metric)("rio_builder_objects_cache_hit_total") (the "Node objects
  cache" panel) is the same signal as an absolute rate, and
  #(refs.metric)("rio_mountd_cache_free_bytes") shows the cache filling.
+ *Nothing is on fire.* #(refs.alert)("RioBuilderCastoreIntegrityFail"),
  #(refs.alert)("RioBuilderCastoreEio"),
  #(refs.alert)("RioMountdPromoteRejectMismatch") and
  #(refs.alert)("RioBuilderDagPrefetchSlow") stay quiet, and the builds
  themselves succeed (`cargo xtask k8s -p eks status`).

= Rollback

There is no flag and no coexistence mode — the old FUSE path was deleted at
the cutover commit. Rolling back is the same greenfield operation in the other
direction: from a checkout of the last pre-cutover commit the cluster ran,

```bash
cargo xtask k8s -p eks up --wipe
```

Be explicit about what that costs before pulling the trigger: the data plane
is wiped *again*, so every path uploaded since the cutover is gone until
clients re-push it, tenants/keys must be re-granted, and the pipeline rebuilds
that commit's AMI and images. If the symptom is "slow" rather than "broken",
work through #cross-link("/ops/castore-fuse-triage.typ")[the triage page]
first — most single-node castore problems are recoverable by cordoning the
node, which is a much smaller hammer than a fleet-wide greenfield rollback.
