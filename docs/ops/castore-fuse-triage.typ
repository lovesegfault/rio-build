#import "/lib/rio.typ": *

#show: rio.with(domains: none)

Node-level triage for the castore-FUSE read path after the cutover
(#cross-link("/ops/castore-fuse-cutover.typ")[previous page]): the
`rio-mountd` DaemonSet, the shared `/var/rio` object/chunk cache it owns, and
the per-build FUSE mounts the builders serve themselves. All three symptoms
below are per-node — prefer cordoning one node over fleet-level action, and
check the *rio-build: Castore FUSE* dashboard
(#(refs.gh)("infra/helm/rio-build/dashboards/castore-fuse.json"),
`cargo xtask k8s -p eks grafana`) before and after.

The store-outage case (every node degrading at once, cold opens blocking and
then failing fast once the fetch breaker opens) is a different animal — see
#cross-link("/spec/system/failure-modes.typ")[failure modes] for that
behaviour; this page is about one node misbehaving while the rest are fine.

= rio-mountd crash loop

*Symptom.* `kube_pod_container_status_restarts_total{namespace="rio-builders",
container="mountd"}` rising for one `rio-mountd-*` pod (the DaemonSet pod on
that node), paired with builds on that node failing castore opens with `EIO`
or stalling on promotes.

Builders tolerate short daemon absences: cache-hit opens keep being served
from already-brokered backing fds, the client reconnects with backoff
(#(refs.metric)("rio_builder_castore_fuse_mountd_reconnect_total")), and
promotes degrade to the build's own staged copy
(#(refs.metric)("rio_builder_castore_fuse_degraded_serve_total")) — so a
single restart is survivable. A crash *loop* is not: cold opens on that node
will start failing once their mountd round-trips time out.

*Actions.*

+ Find the pod and read the previous container's logs:

  ```bash
  kubectl -n rio-builders get pods -l app.kubernetes.io/name=rio-mountd -o wide
  kubectl -n rio-builders logs -p <rio-mountd-pod>
  ```

+ One-off causes (OOM kill from an oversized backing-id table, a bad
  `/var/rio` mount on that node) show up directly in the log tail or in
  `kubectl describe pod` events.
+ If the loop persists: cordon the node, delete the builder/fetcher pods on it
  (their builds re-dispatch — same reassignment path the
  #cross-link("/ops/eks-smoke.typ")[smoke test] exercises by killing a
  worker), and capture the node-side state for the post-mortem before
  recycling the node:

  ```bash
  kubectl cordon <node>
  kubectl -n rio-builders delete pod -l app.kubernetes.io/name=rio-builder \
    --field-selector spec.nodeName=<node> --wait=false
  # from a debug shell or SSM session on the node:
  ls -laR /var/rio/cache /var/rio/chunks /var/rio/staging | head -200
  ```

= Promote rejects with `reason="mismatch"`

*Symptom.* #(refs.metric)("rio_mountd_promote_reject_total")`{reason="mismatch"}`
is non-zero — #(refs.alert)("RioMountdPromoteRejectMismatch") fires. A builder
asked mountd to publish a staged file (or chunk) into the shared node cache
and the bytes' @blake3 hash did not match the digest the builder claimed.

Other `reason` values (`not-regular`, `too-large`, `race-timeout`) are
operational noise — retried or build-fatal on the offending build only. A
`mismatch` is the one with security weight: either the bytes the builder
fetched were already wrong (store-side corruption) or the builder itself is
producing bytes it should not (bug or compromised pod). The reject means the
shared cache was *not* poisoned — mountd re-hashes before publication and
refused — so the goal of triage is attribution, not cleanup.

*Actions.*

+ Identify the build: the mountd log line for the reject names the
  `build_id`; map it to the derivation/tenant via the dashboard build log
  viewer or the scheduler's build records.
+ Check the store side for matching trouble:
  #(refs.metric)("rio_store_narhash_mismatch_total") and
  #(refs.alert)("RioStoreFileDigestMismatch") (chunk re-hash on upload), plus
  #(refs.metric)("rio_builder_castore_fuse_integrity_fail_total") on other
  nodes. If those move too, treat it as a store/data problem, not a node
  problem.
+ If the store is clean, treat the builder pod as suspect: cordon the node,
  keep the pod's staging directory (`/var/rio/staging/<build_id>/`) for
  forensics — do not let the sweep or a node recycle delete it before it has
  been looked at — and only then kill the pod.

= Builds slow on a single node

Walk the tree top-down; each step uses a different panel of the castore
dashboard, filtered to the node's builder pods.

+ *Passthrough never engages*:
  #(refs.metric)("rio_builder_castore_fuse_open_mode_total")`{mode="passthrough"}`
  is zero on that node while other nodes show it. Warm reads are falling back
  to FUSE upcalls — check the node's kernel (`uname -r` ≥ 6.9, AMI built from
  `nix/nixos-node/kernel.nix`) and `dmesg` for FUSE errors; a node that came
  up on a stale pre-cutover AMI is the usual cause.
+ *Promote backlog*: #(refs.metric)("rio_mountd_promote_inflight") pegged at
  its ceiling and #(refs.metric)("rio_mountd_cache_free_bytes") low or falling
  — the node cache is under disk pressure and promotes are stuck behind the
  sweep (#(refs.metric)("rio_mountd_sweep_low_space_total"),
  #(refs.metric)("rio_mountd_sweep_bytes_freed_total")`{tier}`). Check what is
  eating `/var/rio` (orphaned staging from crashed builds is swept first;
  a single huge build can legitimately fill it) and whether the node's
  `/var/rio` sizing matches the fleet default.
+ *mountd starved*: p99 of
  #(refs.metric)("rio_mountd_request_seconds")`{op="backing_open"}` above
  \~1 ms (its normal cost is one ioctl). The DaemonSet pod is CPU-throttled or
  the node is saturated — check node CPU pressure and the mountd container's
  throttling stats before raising its requests.
+ *None of the above*: the slowness is upstream of the node — compare the
  node's #(refs.metric)("rio_builder_castore_fuse_fetch_bytes_total")`{hit="remote"}`
  rate and #(refs.metric)("rio_builder_castore_fuse_open_seconds") against the
  store-side latency panels on the *rio-build: Chunk Cache Tier* dashboard
  (#cross-link("/ops/tiered-cache-cutover.typ")[cache tier page]). A node that
  fetches remotely much more than its peers usually just has a colder cache
  (new node); sustained fleet-wide remote-fetch latency is a store or Express
  tier problem, not a node problem.
