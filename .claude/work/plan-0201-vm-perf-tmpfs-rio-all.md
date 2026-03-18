# Plan 0201: VM perf — tmpfs containerd + rio-all single image (13-25× speedup)

## Design

Two compounding VM perf fixes. Combined: k3s-full cold boot ~8min → ~2min.

**tmpfs containerd (`24c8537`, 3.3-5×):** selective tmpfs — only the containerd image store goes to RAM. PG PVC stays on qcow2 (unbounded data growth in tmpfs would OOM). Before: containerd writes decompressed airgap layers to ext4→qcow2→builder-disk; `cache=writeback` helps but fsync still hits host `fdatasync`. Measured 3.3-3.6× per-image variance on IDENTICAL drvs (bitnami 29.5s vs 97s, `rio-gateway` 37s vs 130s — both I/O-bound, ~10-12% CPU). Earlier run hit 5×. After: writes are RAM-to-RAM; residual variance = 9p reads (compressed tarballs from nix store) + zstd decompress (CPU-bound). Memory: server 6144→8192 (10240 cov), agent 4096→6144 (8192 cov), tmpfs cap 3G (4G cov).

**rio-all single image (`6be7f52`, 5→1 tarball):** the five per-component images share the same `rio-workspace` closure and differ only in `Entrypoint`. k3s airgap-imports serially+alphabetically before kubelet starts, so each VM test decompressed ~618 MiB across five near-identical tarballs (×2 nodes). New `all` image = worker's contents (superset) with no `Entrypoint`. Pods set `command:` per container instead. One 134 MiB tarball per node. Helm templates get optional `{{- with $x.command }}` block (no-op when unset → prod unchanged). `builders.rs`: unconditional `command=/bin/rio-worker` (no-op for prod's `rio-worker` image — `command` overrides `Entrypoint` to same binary).

**Hash reset (`68a253f`):** the tmpfs/image changes invalidated the bitnami FOD hash.

## Files

```json files
[
  {"path": "nix/tests/fixtures/k3s-full.nix", "action": "MODIFY", "note": "tmpfs for containerd store (not PG); memory bumps; rioImages 5→1; gate rio-store→rio-all"},
  {"path": "nix/docker.nix", "action": "MODIFY", "note": "factor workerExtra* into let-bindings; `all` attr with no Entrypoint"},
  {"path": "infra/helm/rio-build/templates/gateway.yaml", "action": "MODIFY", "note": "{{- with $x.command }} block"},
  {"path": "infra/helm/rio-build/templates/scheduler.yaml", "action": "MODIFY", "note": "command block"},
  {"path": "infra/helm/rio-build/templates/store.yaml", "action": "MODIFY", "note": "command block"},
  {"path": "infra/helm/rio-build/templates/controller.yaml", "action": "MODIFY", "note": "command block"},
  {"path": "infra/helm/rio-build/values/vmtest-full.yaml", "action": "MODIFY", "note": "image=rio-all + command=/bin/rio-X per component"},
  {"path": "rio-controller/src/reconcilers/workerpool/builders.rs", "action": "MODIFY", "note": "unconditional command=/bin/rio-worker"}
]
```

## Tracey

No tracey markers — performance infrastructure.

## Entry

- Depends on P0178: k3s coverage collection (coverage mode gets the bigger tmpfs caps)

## Exit

Merged as `24c8537`, `6be7f52`, `68a253f` (3 commits). k3s-full scenarios: critical path ~8min (was ~14min before P0179 split; compound with split = ~4min p95).
