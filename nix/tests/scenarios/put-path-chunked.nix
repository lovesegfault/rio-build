# PutPathChunked end-to-end (ADR-022 §6, P0586): a real `nix build`
# through the gateway → the real rio-builder binary doing the fused
# single-pass walk (NAR framing + SHA-256 + refscan + per-file FastCDC
# + castore Directory construction) → `HasChunks` → a `PutPathChunked`
# client-stream → the real rio-store verify-and-commit.
#
# Scope: only the subtests that need the REAL BUILDER are here. The
# plan's §P0586 scenario table lists 12 subtests; 9 of them exercise
# server-side rejection of malformed `Begin`/`Chunk` frames that a
# correct builder cannot produce (tampered bodies, wrong claimed
# hashes, out-of-order frames, oversize bounds, truncated streams,
# undeclared refs, CA-path squatting, durable-presence semantics).
# Those are covered by `rio-store/tests/grpc/put_path_chunked.rs`
# (hand-crafted streams against the real handler + ephemeral PG) and
# `rio-builder/tests/chunked_upload.rs` (the real upload code against
# the real handler). Re-driving them from a VM would need a
# fault-injection hook in the production builder that does not exist.
#
# What only a VM can prove:
#   roundtrip — a multi-output `nix build` is committed atomically via
#               PutPathChunked and is servable back through the full
#               gateway read path (subtests i, ii, x-positive).
#   dedup     — a second build whose output shares content with the
#               first sends only the missing chunks; shared chunks get
#               a second manifest reference instead of a second S3
#               object (subtest vi).
#
# The store always has a `[chunk_backend]` (required config since
# P0583); the `roundtrip` fragment asserts even the tiny single-file
# output landed with a chunk manifest.
{
  pkgs,
  common,
  fixture,
}:
let
  inherit (fixture) gatewayHost;
  drvs = import ../lib/derivations.nix { inherit pkgs; };

  # ~349 KiB of varied, deterministic bytes (`seq 1 60000`). Exceeds
  # CHUNK_MAX (256 KiB) so FastCDC must emit several per-file-aligned
  # chunks, and varied enough that content-defined boundaries are
  # meaningful (a constant byte stream would collapse to one repeated
  # chunk digest). Shared verbatim between the two derivations below —
  # the dedup subtest depends on the byte streams being identical.
  blobCmd = "\${busybox}/bin/busybox seq 1 60000";

  # Two-output derivation. `out` is a directory tree exercising every
  # NAR node kind the fused walk handles: a nested executable that
  # embeds a store path (reference detection), two byte-identical files
  # (repeated chunk_manifest digest, subtest ii), a multi-chunk blob,
  # and a symlink. `dev` is a tiny single-file output — exercises the
  # single-chunk-manifest shape alongside `out`'s multi-chunk one.
  multiDrv = drvs.mkCustom {
    name = "rio-chunked-multi";
    extraAttrs.outputs = [
      "out"
      "dev"
    ];
    script = ''
      ''${busybox}/bin/busybox mkdir -p $out/bin $out/share
      ''${busybox}/bin/busybox printf '#!%s/bin/sh\necho chunked-tool\n' "''${busybox}" > $out/bin/tool
      ''${busybox}/bin/busybox chmod +x $out/bin/tool
      ''${busybox}/bin/busybox echo duplicate-content > $out/share/a
      ''${busybox}/bin/busybox echo duplicate-content > $out/share/b
      ${blobCmd} > $out/blob
      ''${busybox}/bin/busybox ln -s bin/tool $out/default
      ''${busybox}/bin/busybox echo chunked-dev-marker > $dev
    '';
  };

  # Single-output derivation whose `blob` is byte-identical to
  # multiDrv's. Built second: every blob chunk is already durable, so
  # the builder's HasChunks probe excludes them from `novel` and the
  # store bumps their refcount instead of receiving them again.
  dedupDrv = drvs.mkCustom {
    name = "rio-chunked-dedup";
    script = ''
      ''${busybox}/bin/busybox mkdir -p $out
      ${blobCmd} > $out/blob
      ''${busybox}/bin/busybox echo dedup-second-build > $out/marker
    '';
  };

  prelude = ''
    ${common.mkBootstrap {
      inherit fixture gatewayHost;
      withSeed = true;
    }}

    all_workers = [worker]

    ${common.mkBuildHelperV2 {
      inherit gatewayHost;
      dumpLogsExpr = "dump_all_logs([${gatewayHost}] + all_workers)";
    }}

    def manifest_row(store_path, column):
        """One column from the manifests row joined to narinfo for a
        full store path. Empty string if the row does not exist."""
        return psql(${gatewayHost},
            f"SELECT {column} FROM manifests m "
            f"JOIN narinfo n USING (store_path_hash) "
            f"WHERE n.store_path = '{store_path}'")

    def chunk_count():
        return int(psql(${gatewayHost}, "SELECT count(*) FROM chunks"))
  '';

  scope = {
    inherit
      pkgs
      common
      drvs
      gatewayHost
      multiDrv
      dedupDrv
      ;
  };
  fragments = builtins.mapAttrs (_: f: f scope) (common.importDir ./put-path-chunked);

  mkTest = common.mkFragmentTest {
    scenario = "put-path-chunked";
    inherit prelude fragments fixture;
    # Boot ~60s + seed + two builds (~20s each under TCG) + psql
    # assertions. 600s is the standard standalone ceiling.
    defaultTimeout = 600;
    chains = [
      {
        before = "roundtrip";
        after = "dedup";
        msg = "dedup requires roundtrip earlier (its chunks must already be durable)";
      }
    ];
  };
in
{
  inherit fragments mkTest;
}
