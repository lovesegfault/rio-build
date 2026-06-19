# Pre-imported containerd state for airgapped k3s VM tests (issue #57 1f).
#
# k3s's airgap goroutine imports `services.k3s.images` SERIALLY, single-
# core, BEFORE kubelet starts. Even with #57 1a's uncompressed layers
# that's ~90-180s of 9p-read + tar-extract per node — the longest
# remaining serial step in every vm-*-k3s test. This derivation moves
# the import to BUILD time: a sandbox containerd ingests the same image
# set with `ctr image import --no-unpack`, then the resulting content
# store (blobs/sha256/*) is packed into an erofs image and the meta.db
# (image refs → manifest digests) is captured alongside.
#
# Runtime (k3s-full.nix k3sBase containerd-seed-mount oneshot, gated on
# `withContainerdSeed = true`):
#   - content.erofs loop-mounted ro, overlaid at
#     …/containerd/io.containerd.content.v1.content (upper/work on the
#     parent tmpfs — content store is append-only, the upper stays ~empty).
#   - meta.db copied into io.containerd.metadata.v1.bolt/ on the tmpfs.
# k3s then starts with images already REGISTERED + blobs already PRESENT;
# its airgap goroutine finds /var/lib/rancher/k3s/agent/images empty and
# returns immediately → kubelet starts at t≈0. Per-pod CRI PullImage sees
# the ref in meta.db, finds it not-yet-unpacked, and lazy-unpacks from the
# local erofs into the overlayfs snapshotter on tmpfs — parallel across
# pods instead of one serial pre-kubelet pass. Same mechanism class as
# r[infra.node.prebake-layer-warm] (the AMI's containerd-seed-warm), but
# the import itself is shifted to build time instead of boot time.
#
# `--no-unpack`: the build sandbox has no CAP_SYS_ADMIN, so the overlayfs
# snapshotter can't run its support-probe mount. Unpack is deferred to
# runtime. The snapshotter dir is deliberately ABSENT from the seed — it
# MUST live on real tmpfs (overlayfs rejects an overlayfs upperdir, so a
# whole-root overlay would break containerd's own snapshot mounts).
#
# meta.db reproducibility: boltdb embeds wall-clock created_at on every
# image/content record, so two builds of the same input set yield byte-
# different meta.db. This is an input-addressed runCommand — the store
# PATH is stable and nothing downstream pins the narHash (unlike
# executorSeed → AMI closureInfo). content.erofs IS reproducible
# (`-T 0 -U <zero-uuid>`; blobs are content-addressed).
{ pkgs }:
{
  name,
  # Heterogeneous archive list — exactly what `services.k3s.images`
  # carried. *.zst (k3sPinned.airgap-images) and *.gz (any buildZstd/
  # gzip docker-archive) are decompressed first: `ctr import --local`
  # reads the tar header directly and rejects a compressed outer.
  # Everything else (#57 1a's *.oci.tar from docker-pulled.nix /
  # vmTestSeed) goes through untouched.
  images,
}:
pkgs.runCommand "rio-${name}-containerd-seed"
  {
    nativeBuildInputs = [
      pkgs.containerd
      pkgs.erofs-utils
      pkgs.zstd
      pkgs.gzip
    ];
  }
  ''
    root=$TMPDIR/root
    sock=$TMPDIR/c.sock
    mkdir -p $root $TMPDIR/state

    cat > $TMPDIR/config.toml <<EOF
    version = 3
    root = "$root"
    state = "$TMPDIR/state"
    # No CRI (it mkdir's /etc/cni — EACCES in the sandbox), no overlayfs
    # snapshotter (leaves a probe dir under \$root). The other
    # snapshotters self-skip; --no-unpack touches none anyway.
    disabled_plugins = [
      "io.containerd.grpc.v1.cri",
      "io.containerd.snapshotter.v1.overlayfs",
    ]
    # uid/gid: containerd chown()s the listener sockets to the configured
    # owner (default 0/0); the nix build sandbox's user-ns mapping makes
    # chown-to-root EINVAL. Own them as the build user.
    [grpc]
      address = "$sock"
      uid = $(id -u)
      gid = $(id -g)
    [ttrpc]
      address = "$sock.ttrpc"
      uid = $(id -u)
      gid = $(id -g)
    EOF

    containerd --config $TMPDIR/config.toml &
    cd_pid=$!
    trap 'kill $cd_pid 2>/dev/null || true' EXIT
    for _ in $(seq 50); do
      ctr -a $sock version >/dev/null 2>&1 && break
      sleep 0.1
    done

    ${pkgs.lib.concatMapStringsSep "\n" (img: ''
      img=${img}
      case "$img" in
        *.zst) zstd -dc "$img" > $TMPDIR/unp.tar; img=$TMPDIR/unp.tar ;;
        *.gz)  gzip -dc "$img" > $TMPDIR/unp.tar; img=$TMPDIR/unp.tar ;;
      esac
      # --local: client-side importer — handles BOTH docker-archive
      # (RepoTags → ref) and multi-manifest oci-archive
      # (org.opencontainers.image.ref.name → ref) by magic; vmTestSeed
      # is multi-manifest. --no-unpack: see file header.
      ctr -a $sock -n k8s.io image import --local --no-unpack "$img"
    '') images}

    kill $cd_pid
    wait $cd_pid || true
    trap - EXIT

    # Drop in-flight ingest scratch (empty after clean shutdown, but
    # belt-and-suspenders for erofs reproducibility).
    rm -rf $root/io.containerd.content.v1.content/ingest

    mkdir $out
    # lz4hc: blobs are uncompressed tars (#57 1a) so erofs-level
    # compression claws back ~2-3× and decompresses kernel-side faster
    # than the 9p read it saves (lz4 ≈ 2-4 GB/s vs 9p ≈ 200 MB/s).
    # -T 0 / -U: fixed mtime + UUID → reproducible image.
    # -x -1: skip xattrs — the nix sandbox's build dir returns ENOTSUP
    # for listxattr(), and content-store blobs carry none anyway.
    mkfs.erofs -zlz4hc -x -1 -T 0 -U 00000000-0000-0000-0000-000000000000 \
      $out/content.erofs $root/io.containerd.content.v1.content
    cp $root/io.containerd.metadata.v1.bolt/meta.db $out/meta.db
  ''
