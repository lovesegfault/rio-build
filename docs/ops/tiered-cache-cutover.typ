#import "/lib/rio.typ": *

#show: rio.with(domains: none)

The store's chunk backend has two production shapes: `kind: s3` (every chunk
read hits S3 standard) and `kind: tiered` (S3 standard stays authoritative;
each store replica adds the per-#gls("az") S3 Express One Zone directory
bucket for its own zone as a read-through cache). ADR-023 records the design;
this page is the operator procedure for flipping a live EKS deployment to
`tiered`, verifying it took, and flipping back.

The flip is config-only and lossless in both directions: S3 standard is always
the authoritative copy, the Express buckets hold nothing that cannot be
re-read from it.

= How the flip is wired

- Terraform owns the per-AZ directory buckets
  (#(refs.gh)("infra/eks/s3-express.tf")): one bucket per AZ in the
  intersection of `express_supported_az_ids` (defaults to the AZ-IDs where
  Express is offered) and the AZs the cluster's subnets actually land in.
  Setting the variable to `[]` disables the tier cluster-wide (no buckets, no
  IAM grant, empty output).
- `cargo xtask k8s -p eks up --deploy` reads the `express_bucket_by_zone`
  tofu output. Non-empty → it deploys the chart with
  `store.chunkBackend.kind=tiered` plus the per-zone bucket map; empty → it
  leaves the chart default `kind: s3`. There is no separate flag to remember —
  provisioning the buckets is the decision.
- Each store pod selects its own AZ's bucket at startup by matching
  #(refs.cfg)("store", "node_zone") (rendered from the pod's
  `topology.kubernetes.io/zone` label, which the `PodTopologyLabelsAdmission`
  plugin copies from the node) against the bucket map. Any missing piece —
  no buckets, plugin not enabled, replica in an AZ without Express — degrades
  that replica to direct S3-standard reads (`local=None`, one log line),
  never to an outage.

= Cutover

+ Provision the buckets (no-op when they already exist):

  ```bash
  cargo xtask k8s -p eks up --provision
  # confirm the map is non-empty:
  tofu -chdir=infra/eks output -json express_bucket_by_zone
  ```

+ Redeploy from the working tree so the deploy picks the map up:

  ```bash
  cargo xtask k8s -p eks up --push --deploy
  ```

+ Verify (the deploy-time checklist from implementation plan §P0554):

  - The zone label reached a store pod (empty means the admission plugin is
    off and every replica runs `local=None`):

    ```bash
    kubectl -n rio-store get pod -l app.kubernetes.io/name=rio-store \
      -o jsonpath='{.items[0].metadata.labels.topology\.kubernetes\.io/zone}'
    ```

  - Each replica's startup log names the zone/bucket pair it selected, and the
    pair agrees with the pod's actual placement (`kubectl get pod -o wide`).
  - A warm read moves #(refs.metric)("rio_store_tiered_local_hits_total")
    without churning #(refs.metric)("rio_store_tiered_local_errors_total");
    write-through failures show up as
    #(refs.metric)("rio_store_tiered_writethrough_errors_total").
  - Watch the *rio-build: Chunk Cache Tier* dashboard
    (#(refs.gh)("infra/helm/rio-build/dashboards/chunk-cache-tier.json")) via
    `cargo xtask k8s -p eks grafana` — the hit-ratio panel should climb as
    builders re-read the same closures.

The tiered VM test (`vm-store-tiered`, implementation plan §P0555) covers the
cache semantics in CI; the steps above only verify the AWS-side wiring that CI
cannot reach (Express IAM, the admission plugin, per-AZ placement).

= Rollback

Single flag, instant and lossless — the Express buckets are a disposable
cache:

```bash
helm upgrade rio infra/helm/rio-build -n rio-system \
  --reuse-values --set store.chunkBackend.kind=s3
```

To remove the tier entirely (buckets and IAM grant included), set
`express_supported_az_ids = []` in the tofu variables and run the provision +
deploy steps again — the deploy sees an empty map and stays on `kind: s3`.

The castore-FUSE cutover (#cross-link("/ops/castore-fuse-cutover.typ")[next
  page]) assumes this flip is already live; it works without it, but every cold
castore read then pays the S3-standard round-trip.
