#import "/lib/rio.typ": *
#show: rio.with(domains: none)


This page provides resource sizing guidance for rio-build deployments. All estimates are approximate and should be validated against actual workload data.

= PostgreSQL Storage

#table(
  columns: 3,
  table.header([Data Type], [Size Per Record], [Notes]),
  [Derivation (scheduler)],
  [\~#qty("1", "KB")],
  [Includes metadata, edges, assignments],

  [@narinfo (store)],
  [\~#qty("500", "byte")],
  [Includes references, signatures],

  [Chunk manifest (store)],
  [\~#qty("200", "byte")],
  [List of (@blake3 hash, size) pairs per @nar],

  [Build history (scheduler)],
  [\~#qty("200", "byte")],
  [@ema duration, resource usage per pname/system],
)

*Worked example --- nixpkgs full rebuild:*
- \~#num("60000") derivations = \~#qty("60", "MB") scheduler state
- \~#num("80000") store paths = \~#qty("40", "MB") store metadata + \~#qty("16", "MB") chunk manifests
- Total: \~#qty("116", "MB") active data (plus indexes)

*Recommendation:* Start with #qty("10", "GB") allocated to PostgreSQL. A single nixpkgs rebuild cycle adds \~#qty("120", "MB"); with GC, steady-state usage plateaus. Monitor `pg_database_size()` and alert at #qty("80", "percent") capacity.

= S3 (Object Storage)

#table(
  columns: 3,
  table.header([Metric], [Estimate], [Notes]),
  [nixpkgs full closure (uncompressed NARs)],
  [\~#qty("200", "GB")],
  [All packages for one system],

  [With @fastcdc dedup],
  [\~#qtyrange("100", "140", "GB")],
  [#qtyrange("30", "50", "percent") chunk dedup savings],

  [Inline paths (\< #qty("256", "KB"))],
  [\~#qty("60", "percent") by count, \~#qty("5", "percent") by size],
  [Stored as single #glspl("blob"), no chunking overhead],

  [Average chunk size],
  [#qty("64", "KB")],
  [FastCDC target (min #qty("16", "KB"), max #qty("256", "KB"))],

  [Incremental rebuild delta],
  [\~#qtyrange("5", "20", "GB")],
  [Depends on what changed since last build],
)

*Recommendation:* Start with #qty("500", "GB"). Enable S3 lifecycle rules to transition old chunks to infrequent access storage after #qty("90", "day"). The store's two-phase GC reclaims unreachable chunks.

= Executors

== Sizing Per Executor

One build per pod (P0537). Size the pod for the build, not for a slot count.

#table(
  columns: 3,
  table.header([Resource], [Recommendation], [Notes]),
  [CPU],
  [4 vCPU minimum],
  [The build's CPU; Nix's `enableParallelBuilding` uses what's available],

  [Memory],
  [#qty("8", "GB") minimum],
  [rio-exec sandbox + overlay + @fuse daemon overhead],

  [Local SSD (FUSE cache)],
  [#qty("100", "GB")],
  [Covers \~#qty("50", "percent") of nixpkgs closure; larger = better hit rate],

  [Instance type (AWS)],
  [`m6id.xlarge` (small/medium)],
  [4 vCPU, #qty("16", "GB"), #qty("237", "GB") NVMe],

  [Instance type (AWS, large builds)],
  [`c6id.2xlarge`],
  [8 vCPU, #qty("16", "GB"), #qty("474", "GB") NVMe],
)

== Fleet Sizing

#table(
  columns: 3,
  table.header([Metric], [Formula], [Notes]),
  [Concurrent builds], [`executors`], [One build per pod],
  [Throughput (small builds, \~#qty("30", "s") avg)],
  [`executors` × 120/hr],
  [\~120 derivations/hr per executor],

  [Throughput (mixed, \~#qty("5", "min") avg)],
  [`executors` × 12/hr],
  [\~12 derivations/hr per executor],
)

*Worked example --- nixpkgs full rebuild (#num("60000") derivations):*
- 40 executors = 40 concurrent builds
- With #qty("30", "s") average build time: \~#num("4800") derivations/hour = \~#qty("12.5", "h") total
- With #qty("5", "min") average (including large packages): \~480 derivations/hour = \~#qty("125", "h") total
- Reality is bimodal: most builds are seconds, a few are hours. Expect #qtyrange("15", "25", "h") for a full nixpkgs rebuild on 40 executors.

*With per-derivation @sla sizing (ADR-023):* the controller spawns one-shot Jobs sized to each derivation's solved `(cores, mem, disk)`, so a `hello` build gets a 1-core/512Mi pod and `firefox` gets 16-core/32Gi without operator partitioning. @karpenter bin-packs the heterogeneous pods onto right-sized nodes. See #cross-link("/spec/components/controller.typ")[controller component spec] for the reconciler flow.

= Gateway and Scheduler

#table(
  columns: 5,
  table.header([Component], [Replicas], [CPU], [Memory], [Notes]),
  [Gateway],
  [2--3],
  [1 vCPU],
  [#qty("1", "GB")],
  [Scales with concurrent SSH connections (\~#qty("1", "KB") per connection)],

  [Scheduler],
  [1 active + 1 standby],
  [2 vCPU],
  [#qty("4", "GB")],
  [In-memory #gls("dag"): \~#qty("8", "byte")/node + \~#qty("16", "byte")/edge. #num("60000")-node DAG ≈ #qtyrange("50", "100", "MB")],

  [Store],
  [2--3],
  [2 vCPU],
  [#qty("4", "GB")],
  [LRU chunk cache: configured via `chunk_cache_capacity_bytes` (default #qty("2", "GB"))],

  [Controller],
  [1],
  [0.5 vCPU],
  [#qty("256", "MB")],
  [Lightweight; mostly waiting for reconcile intervals],
)

= Monitoring Thresholds

#table(
  columns: 3,
  table.header([Metric], [Warning], [Critical]),
  [PG connection pool utilization],
  [\> #qty("70", "percent")],
  [\> #qty("90", "percent")],

  [S3 request rate (429 errors)], [\> 0 sustained], [\> 10/min],
  [Executor queue depth], [\> 2× executor count], [\> 5× executor count],
  [Scheduler actor queue depth],
  [\> #qty("50", "percent") capacity (#num("5000"))],
  [\> #qty("80", "percent") capacity (#num("8000"))],

  [FUSE cache hit rate], [\< #qty("80", "percent")], [\< #qty("50", "percent")],
  [Build failure rate], [\> #qty("5", "percent")], [\> #qty("15", "percent")],
)
