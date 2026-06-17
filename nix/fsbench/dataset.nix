# The dataset: ghc's out path — ~1.9 GiB logical, ~6.5k
# files with a real small-file-heavy distribution (.hi interface
# files), 23 files above the 8 MiB stream threshold, largest ~476 MB
# (libHSghc_p.a) — re-rooted under a seeded layout. ghc rather than
# python3: the qa --load tiers build with python3, so its contents may
# already sit in node caches; nothing on the cluster builds Haskell.
#
# Generated ON the cluster: nothing big crosses the gateway — the
# output ingests through the normal PutPathChunked path and the bench
# run reads it back through the castore mount. The seed controls only
# the tree LAYOUT (contents are ghc's, pinned by flake.lock); with the
# fixed default seed the dataset is built and uploaded once, and
# "cold" means the bench node's local cache is empty — a fresh node,
# which the honesty gate and contended detection verify (against the
# manifest's unique-chunk byte counts) rather than assume.
#
# fsbench gen is deterministic per (seed, ghc): an infra retry of this
# drv reproduces byte-identical output, keeping digests stable within
# a run.
{
  runCommand,
  ghc,
  fsbenchBins,
  seed,
}:
runCommand "fsbench-dataset-${seed}" { } ''
  ${fsbenchBins}/bin/fsbench gen --seed ${seed} --harvest ${ghc} --out $out
''
