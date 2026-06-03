# The bench run: executes fsbench AS A BUILD, so the dataset, the
# python3 closure, and the jq-build toolchain all arrive through the
# production castore-FUSE mount — the thing being measured. stdout
# (PHASE/PERF lines) streams back through `nix build -L`; the raw-JSON
# twin lands in $out as a recovery path (`nix copy --from`) if log
# parsing ever degrades.
#
# python3: the open-storm closure — a real, deep store tree for the
# open/fstat walk. Chosen over the bench's own dataset because closure
# shape (many small files, deep dirs, symlinks) is what open-RTT
# distributions look like in production builds.
#
# jq.src + stdenv.cc/gnumake: the jq_build compile phases — a real
# compiler workload (many small toolchain reads, header opens, cc/ld
# execs) served through the mount; the build itself writes to $TMPDIR
# only. --without-oniguruma inside fsbench keeps the dependency
# surface to the toolchain itself.
#
# runNonce: per-run discriminator in the drv name — the dataset is
# deliberately stable across runs (fixed seed), so without the nonce
# this drv's output would already be valid remotely on the second run
# and nix would never execute the benchmark again.
{
  runCommand,
  python3,
  jq,
  gnumake,
  stdenv,
  fsbenchBins,
  seed,
  runNonce,
  dataset,
}:
runCommand "fsbench-run-${seed}-${runNonce}"
  {
    nativeBuildInputs = [
      stdenv.cc
      gnumake
    ];
  }
  ''
    echo "FSBENCH seed=${seed} nonce=${runNonce}"
    mkdir -p $out
    ${fsbenchBins}/bin/fsbench run \
      --dataset ${dataset} \
      --closure ${python3} \
      --scratch "$TMPDIR" \
      --jq-src ${jq.src} \
      --out $out/fsbench-raw.json
  ''
