# Dev-profile Helm charts from nixhelm (FODs — hash-pinned, cached).
#
# nixhelm provides charts as Nix derivations: each is a `helm pull`
# wrapped as a fixed-output derivation. Output is an unpacked chart
# directory; helm's `charts/` accepts those. No .tgz in git.
#
# Update: bump nixhelm's rev in flake.lock (`nix flake update nixhelm`).
# nixhelm tracks latest chart versions via a nightly GitHub Action.
#
# Why not hand-roll `helm pull` FODs: nixhelm already did that work for
# 200+ charts and maintains the hashes. Why not vendor .tgz: binaries
# in git. Why not `helm dependency build` at check time: needs network,
# fails in the nix sandbox.
{
  nixhelm,
  system,
}:
let
  charts = nixhelm.chartsDerivations.${system};
in
{
  # Chart 18.x bundles PostgreSQL 18. deployment.md requires PG 15+;
  # Aurora prod is PG 15, so dev stays ahead (catches forward-compat
  # issues early).
  inherit (charts.bitnami) postgresql;

  # Rook operator (CRDs + controller). Installed BEFORE rook-ceph-cluster
  # — the cluster chart's CephCluster/CephObjectStore CRs need CRDs
  # present. `cargo xtask k8s up -p k3s` installs these in sequence
  # (operator → wait → cluster → wait for ObjectStoreUser secret → rio chart).
  # CephCluster + CephObjectStore + CephObjectStoreUser templates from
  # rook-ceph-cluster. Single-node dev topology via infra/helm/rook-dev-
  # values.yaml: 1 mon, 1 mgr, loop-device OSD. ObjectStore = RGW → S3-
  # compatible endpoint. The ObjectStoreUser's secret (in rook-ceph ns)
  # holds AccessKey/SecretKey; xtask k8s up -p k3s copies it to
  # rio-system as `rio-s3-creds`.
  inherit (charts.rook-release) rook-ceph rook-ceph-cluster;

  # Cilium CNI (eBPF datapath, WireGuard transparent encryption,
  # CiliumNetworkPolicy, Gateway API). Replaces flannel in the k3s
  # VM fixture and aws-vpc-cni on EKS. Gateway API CRDs are vendored
  # separately (cilium-render.nix gatewayApiCrds — Cilium expects them
  # pre-installed); the dashboard's gRPC-Web translation is in-process
  # at rio-scheduler via tonic-web (D3), so the Gateway is plain HTTP
  # routing.
  inherit (charts.cilium) cilium;
}
