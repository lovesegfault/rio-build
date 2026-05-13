//! FUSE store benchmark suite.
//!
//! Measures the rio-builder FUSE filesystem (`NixStoreFs`) under a
//! `bwrap`-isolated reader workload, compared against a local-filesystem
//! baseline (the same workload over the cache directory directly, with
//! no FUSE in the path). The local-fs baseline is the speed-of-light
//! number: any delta is FUSE round-trip + cache-miss fetch overhead.
//!
//! Synthetic cross-component latency is injected at the gRPC layer via
//! [`MockStoreFaults::rpc_latency_ms`] — typical builder ⇄ store RTTs
//! (same-AZ ≈ 1 ms, cross-AZ ≈ 5 ms) — so the cold-fetch group shows
//! how store latency amplifies through the FUSE singleflight path.
//!
//! Why `bwrap`: every real FUSE consumer is a sandboxed build process
//! in its own mount namespace, not the rio-builder process itself.
//! Reading the mount in-process would skip the cross-process FUSE
//! request queue + page-cache boundary that dominates real workloads.
//! `bwrap --unshare-all --bind <mount> /work …` reproduces the
//! sandbox-shaped access pattern.
//!
//! # Requirements (skipped gracefully if absent)
//!
//! - `/dev/fuse` (rw) + `fusermount3` on `$PATH` — to mount `NixStoreFs`
//! - `bwrap` on `$PATH` + unprivileged user namespaces — to sandbox the
//!   reader workload
//!
//! When unavailable, the FUSE groups log a skip and only the
//! local-fs baseline runs (still useful as a regression canary for
//! the workload script itself).
//!
//! # Groups
//!
//! - `local_fs/read_tree`: `bwrap` + reader over a plain directory.
//! - `fuse_warm/read_tree`: same workload over the FUSE mount with the
//!   cache pre-populated — pure FUSE round-trip cost, no gRPC.
//! - `fuse_cold/{0ms,1ms,5ms}`: cold cache, every `lookup` triggers a
//!   `GetPath` fetch from the mock store with synthetic RPC latency.
//!   Throughput-mode (`Throughput::Elements(N_PATHS)`) so the report
//!   reads as "fetches/sec".
//! - `fio_randread/{local_fs,fuse_warm}`: `fio` 4 KiB psync random
//!   reads against a single 16 MiB store path. Complements `read_tree`
//!   (which is dominated by per-path lookup/open) with a per-`read(2)`
//!   latency view of the FUSE data plane. Skipped when `fio` is not on
//!   `$PATH`.
//! - `python_closure/{local_fs,fuse_warm,fuse_cold}`: `python -c 'import
//!   numpy, requests, yaml, …'` over a real `python3.withPackages`
//!   closure (~250–400 store paths) NAR-packed from the host store at
//!   bench time. Exercises the deep-tree, many-small-file, symlink-heavy
//!   access pattern that the synthetic 50-flat-file `read_tree` does not.
//!   Skipped when `nix` is not on `$PATH` or the env build fails.
//! - `jq_build/{local_fs,fuse_warm,fuse_cold}`: `./configure && make`
//!   of jq with a real C toolchain (~50 store paths) read through the
//!   FUSE mount. Every `cc`/`cpp`/`ld` fork is a deep store-path lookup
//!   chain; `configure` repeats it hundreds of times. The heaviest
//!   store reader in the suite. Same skip rules as `python_closure`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};

use rio_builder::fuse::cache::Cache;
use rio_builder::fuse::{StoreClients, mount_fuse_background};
use rio_test_support::fixtures::{make_nar, make_path_info_for_nar, test_store_basename};
use rio_test_support::grpc::{MockStore, spawn_mock_store};

/// Number of distinct store paths in the workload. Keep modest: the
/// cold-fetch group does N_PATHS gRPC round-trips per iteration, and
/// at 5 ms injected latency that's already 250 ms/iter.
const N_PATHS: usize = 50;

/// File payload size. Small (4 KiB) so the bench measures per-path
/// dispatch overhead (lookup/open/read/release) rather than bulk
/// streaming throughput — that's what dominates real Nix builds, which
/// stat thousands of small inputs but bulk-read few.
const FILE_SIZE: usize = 4096;

/// Synthetic gRPC latencies to sweep in the cold-fetch group. 0 = colo
/// loopback, 1 ms ≈ same-AZ, 5 ms ≈ cross-AZ.
const LATENCIES_MS: &[u64] = &[0, 1, 5];

/// Size of the single "big" store path used by the fio random-read
/// group. Big enough that the kernel page cache doesn't trivially hold
/// the whole file across the bwrap→FUSE→cache_dir double mapping;
/// small enough that the warm-up materialization (one cold gRPC fetch
/// + NAR extract) stays under a second.
const BIG_FILE_SIZE: usize = 16 * 1024 * 1024;

/// Wall-clock budget per fio invocation. fio is `time_based`: it runs
/// exactly this long and reports IOPS/latency for whatever it managed.
/// Criterion then samples a handful of these. Short (1 s) so the suite
/// stays under a minute; the per-`read(2)` latency we care about
/// stabilizes within ~100 ms.
const FIO_RUNTIME_SECS: u32 = 1;

// ---------------------------------------------------------------------------
// Capability probes
// ---------------------------------------------------------------------------

fn have_fuse() -> bool {
    Path::new("/dev/fuse").exists()
        && (which("fusermount3").is_some() || which("fusermount").is_some())
}

fn have_fio() -> bool {
    which("fio").is_some()
}

fn have_bwrap() -> bool {
    which("bwrap").is_some()
        // bwrap needs unprivileged userns OR setuid root. Probe with a
        // no-op sandbox; failure here means the FUSE groups would also
        // fail, so skip the whole benchmark cleanly.
        && Command::new("bwrap")
            .args(["--unshare-all", "--bind", "/", "/", "true"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
}

fn which(prog: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(prog))
        .find(|p| p.is_file())
}

// ---------------------------------------------------------------------------
// Workload: bwrap-sandboxed reader
// ---------------------------------------------------------------------------

/// Run `bwrap --unshare-all --ro-bind <root> /work cat /work/*` and wait.
///
/// `cat` (not `find -exec stat`) so each path goes through the full
/// lookup→open→read→release cycle — that's what passthrough mode
/// optimizes and what the cache-miss path materializes.
///
/// Panics on failure: a broken sandbox is a benchmark setup bug, not a
/// measurable outcome.
fn bwrap_read_tree(root: &Path, basenames: &[String]) {
    let mut cmd = Command::new("bwrap");
    cmd.args(["--unshare-all", "--die-with-parent"]);
    // Minimal rootfs: /usr/bin for cat, the store tree at /work.
    cmd.args(["--ro-bind", "/usr", "/usr"]);
    cmd.args(["--ro-bind", "/bin", "/bin"]);
    if Path::new("/lib").exists() {
        cmd.args(["--ro-bind", "/lib", "/lib"]);
    }
    if Path::new("/lib64").exists() {
        cmd.args(["--ro-bind", "/lib64", "/lib64"]);
    }
    if Path::new("/nix").exists() {
        // NixOS / nix-shell hosts: cat lives in /nix/store.
        cmd.args(["--ro-bind", "/nix", "/nix"]);
    }
    cmd.args(["--ro-bind"]);
    cmd.arg(root).arg("/work");
    cmd.args(["--dev", "/dev", "--proc", "/proc"]);
    cmd.arg("cat");
    for b in basenames {
        cmd.arg(format!("/work/{b}"));
    }
    let out = cmd.output().expect("spawn bwrap");
    assert!(
        out.status.success(),
        "bwrap reader failed: status={:?} stderr={}",
        out.status,
        // Diagnostic-only path; non-UTF-8 stderr from bwrap is fine to drop.
        std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")
    );
    assert_eq!(
        out.stdout.len(),
        basenames.len() * FILE_SIZE,
        "reader didn't see expected bytes (FUSE serving truncated data?)"
    );
}

/// Run `fio` (psync 4 KiB random reads, no O_DIRECT) under bwrap
/// against `root/<basename>`. Returns nothing — criterion times the
/// whole invocation. fio's own latency histograms are the interesting
/// output but criterion can't ingest them; the wall-clock proxy is
/// good enough for a regression canary, and `--output-format=json` is
/// available for manual deep-dives (`RIO_FUSE_BENCH_FIO_VERBOSE=1`).
///
/// `psync` (not `io_uring`/`libaio`): synchronous `pread(2)` is what
/// real Nix builds issue — compilers and linkers don't use AIO — and
/// it's the path FUSE serves through the `read()` callback rather than
/// kernel-side page-cache readahead.
fn bwrap_fio_randread(root: &Path, basename: &str) {
    // Resolve fio outside the sandbox; bind its install dir so non-/usr
    // installs (e.g. nix profile, /tmp build artifacts) work.
    // Canonicalize: nix profile entries are symlinks under e.g.
    // /tmp/result/bin or ~/.nix-profile/bin, both shadowed by
    // `--tmpfs /tmp` and not bound. The /nix/store target IS bound.
    let fio = which("fio")
        .and_then(|p| p.canonicalize().ok())
        .expect("have_fio() guard should have caught this");
    let mut cmd = Command::new("bwrap");
    cmd.args(["--unshare-all", "--die-with-parent"]);
    cmd.args(["--ro-bind", "/usr", "/usr"]);
    cmd.args(["--ro-bind", "/bin", "/bin"]);
    if Path::new("/lib").exists() {
        cmd.args(["--ro-bind", "/lib", "/lib"]);
    }
    if Path::new("/lib64").exists() {
        cmd.args(["--ro-bind", "/lib64", "/lib64"]);
    }
    if Path::new("/nix").exists() {
        cmd.args(["--ro-bind", "/nix", "/nix"]);
    }
    if let Some(fio_dir) = fio.parent()
        && !fio_dir.starts_with("/usr")
        && !fio_dir.starts_with("/bin")
        && !fio_dir.starts_with("/nix")
    {
        cmd.args(["--ro-bind"]);
        cmd.arg(fio_dir).arg(fio_dir);
    }
    cmd.args(["--ro-bind"]);
    cmd.arg(root).arg("/work");
    cmd.args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"]);
    cmd.arg(&fio);
    cmd.args([
        "--name=randread",
        &format!("--filename=/work/{basename}"),
        "--readonly",
        "--rw=randread",
        "--bs=4k",
        "--ioengine=psync",
        // Time-based so each criterion sample does the same amount of
        // work regardless of how fast reads complete.
        "--time_based",
        &format!("--runtime={FIO_RUNTIME_SECS}"),
        // Keep the run quiet unless the operator asked for the histograms.
        if std::env::var_os("RIO_FUSE_BENCH_FIO_VERBOSE").is_some() {
            "--output-format=normal"
        } else {
            "--minimal"
        },
    ]);
    let out = cmd.output().expect("spawn bwrap fio");
    assert!(
        out.status.success(),
        "fio failed: status={:?} stderr={}",
        out.status,
        std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")
    );
    if std::env::var_os("RIO_FUSE_BENCH_FIO_VERBOSE").is_some() {
        eprintln!(
            "{}",
            std::str::from_utf8(&out.stdout).unwrap_or("<non-utf8 stdout>")
        );
    }
}

// ---------------------------------------------------------------------------
// Fixture: tree of single-file NAR store paths
// ---------------------------------------------------------------------------

struct Fixture {
    /// Store-path basenames, e.g. `<32hash>-bench-0007`.
    basenames: Vec<String>,
    /// One NAR per path (single regular file, FILE_SIZE bytes).
    nars: Vec<(String, Vec<u8>, [u8; 32])>,
    /// Single 16 MiB store path for the fio random-read group.
    big_basename: String,
    /// Raw payload of `big_basename` (NOT NAR-framed).
    big_payload: Vec<u8>,
    /// `(store_path, nar, hash)` of `big_basename` for seeding the mock.
    big_nar: (String, Vec<u8>, [u8; 32]),
}

impl Fixture {
    fn new() -> Self {
        let mut basenames = Vec::with_capacity(N_PATHS);
        let mut nars = Vec::with_capacity(N_PATHS);
        for i in 0..N_PATHS {
            let basename = test_store_basename(&format!("bench-{i:04}"));
            // Distinct contents per path so the kernel can't dedupe.
            let payload: Vec<u8> = (0..FILE_SIZE).map(|j| ((i + j) % 251) as u8).collect();
            let (nar, hash) = make_nar(&payload);
            let store_path = format!("/nix/store/{basename}");
            nars.push((store_path, nar, hash));
            basenames.push(basename);
        }
        let big_basename = test_store_basename("bench-big");
        // Pseudo-random payload: defeats kernel/zswap dedup of
        // zero-filled pages, which would make a 16 MiB file effectively
        // free to read.
        let big_payload: Vec<u8> = (0..BIG_FILE_SIZE)
            .map(|i| (i.wrapping_mul(2654435761) >> 17) as u8)
            .collect();
        let (nar, hash) = make_nar(&big_payload);
        let big_nar = (format!("/nix/store/{big_basename}"), nar, hash);
        Self {
            basenames,
            nars,
            big_basename,
            big_payload,
            big_nar,
        }
    }

    /// Materialize the same single-file payloads as plain files under
    /// `dir/<basename>` — what the FUSE cache would look like after a
    /// warm fetch, and what the local-fs baseline reads.
    fn materialize_local(&self, dir: &Path) {
        for (i, b) in self.basenames.iter().enumerate() {
            let payload: Vec<u8> = (0..FILE_SIZE).map(|j| ((i + j) % 251) as u8).collect();
            fs::write(dir.join(b), payload).expect("write local fixture");
        }
        fs::write(dir.join(&self.big_basename), &self.big_payload).expect("write big fixture");
    }
}

// ---------------------------------------------------------------------------
// Real-world fixtures: nixpkgs closures NAR-packed from the host store
// ---------------------------------------------------------------------------

/// `nix build` an expression and NAR-pack its runtime closure.
///
/// Returns `None` (with a logged reason) when the host can't supply the
/// closure — missing `nix`, offline, no cache hit. Bench groups skip
/// rather than panic: a host limitation, not a bug.
struct RealClosure {
    /// Out-path of the built derivation.
    env_path: PathBuf,
    /// `(basename, nar_bytes)` for the entire runtime closure.
    nars: Vec<(String, Vec<u8>)>,
}

impl RealClosure {
    fn build(group: &str, expr: &str) -> Option<Self> {
        if which("nix").is_none() {
            eprintln!("[fuse bench] nix not on $PATH, skipping {group}");
            return None;
        }
        let out = Command::new("nix")
            .args([
                "build",
                "--no-link",
                "--print-out-paths",
                "--impure",
                "--expr",
                expr,
            ])
            .output()
            .ok()?;
        if !out.status.success() {
            eprintln!(
                "[fuse bench] nix build failed, skipping {group}: {}",
                std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")
            );
            return None;
        }
        let env_path = PathBuf::from(String::from_utf8(out.stdout).ok()?.trim());

        let refs = Command::new("nix-store")
            .args(["-qR", env_path.to_str()?])
            .output()
            .ok()?;
        if !refs.status.success() {
            eprintln!("[fuse bench] nix-store -qR failed, skipping {group}");
            return None;
        }
        let closure: Vec<PathBuf> = String::from_utf8(refs.stdout)
            .ok()?
            .lines()
            .map(PathBuf::from)
            .collect();

        eprintln!(
            "[fuse bench] NAR-packing {} closure paths for {group} (this is slow)",
            closure.len()
        );
        let mut nars = Vec::with_capacity(closure.len());
        for p in &closure {
            let basename = p.file_name()?.to_str()?.to_owned();
            let nar = match rio_nix::nar::dump_path(p) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!(
                        "[fuse bench] dump_path({}) failed: {e}, skipping {group}",
                        p.display()
                    );
                    return None;
                }
            };
            nars.push((basename, nar));
        }

        Some(Self { env_path, nars })
    }

    fn basenames(&self) -> Vec<&str> {
        self.nars.iter().map(|(b, _)| b.as_str()).collect()
    }
}

/// Base bwrap sandbox for real-closure workloads: the supplied
/// `store_root` (a FUSE mount or the host's real store) is bound at
/// `/nix/store` so absolute store-path references in shebangs and
/// DT_RUNPATH resolve.
fn bwrap_with_store(store_root: &Path) -> Command {
    let mut cmd = Command::new("bwrap");
    cmd.args(["--unshare-all", "--die-with-parent"]);
    // Clear inherited env: the bench runs inside a `nix develop` shell
    // whose CC/CFLAGS/LD_LIBRARY_PATH point at the rio-build toolchain,
    // which breaks `./configure` for the cc-env workload (and is wrong
    // for python_closure too — a sandboxed build would never see it).
    cmd.args(["--clearenv"]);
    cmd.args(["--ro-bind"]);
    cmd.arg(store_root).arg("/nix/store");
    cmd.args(["--dev", "/dev", "--proc", "/proc", "--tmpfs", "/tmp"]);
    cmd.args(["--setenv", "HOME", "/tmp"]);
    cmd.args(["--setenv", "LC_ALL", "C"]);
    cmd
}

fn run_or_panic(out: std::process::Output, what: &str) {
    assert!(
        out.status.success(),
        "{what} failed: status={:?} stderr={}",
        out.status,
        std::str::from_utf8(&out.stderr).unwrap_or("<non-utf8 stderr>")
    );
}

// --- python_closure ---

/// `nix build` expression: a Python env with a few real C-extension
/// packages. Deep directory trees, lots of `.py`/`.so`/symlinks — the
/// access pattern that read_tree (50 flat files) does not exercise.
const PYTHON_ENV_EXPR: &str =
    "(import <nixpkgs> {}).python3.withPackages (p: [ p.numpy p.requests p.pyyaml ])";

/// Imports that touch every package in `PYTHON_ENV_EXPR` plus the
/// stdlib. Forces module bytecode loads and shared-object dlopen; the
/// realistic cold-start cost of a Python build step.
const PYTHON_WORKLOAD: &str = "import numpy, requests, yaml, json, ssl, sqlite3, decimal";

/// Run `python -c <workload>` over `store_root`.
fn bwrap_python(store_root: &Path, env_path: &Path) {
    let basename = env_path.file_name().expect("env basename");
    let interp = Path::new("/nix/store").join(basename).join("bin/python3");
    let mut cmd = bwrap_with_store(store_root);
    cmd.arg(&interp).arg("-c").arg(PYTHON_WORKLOAD);
    run_or_panic(cmd.output().expect("spawn bwrap python"), "python workload");
}

// --- jq_build ---

/// Toolchain env for the `jq_build` group, derived from jq's actual
/// derivation: `stdenv.cc`/`stdenv.shell`/`stdenv.initialPath` (the
/// implicit builder PATH every nix build gets) plus jq's declared
/// `nativeBuildInputs` and `buildInputs`. `buildEnv` so all `bin/`s
/// link under one path that bwrap can bind at `/usr/bin`.
///
/// Why not `jq.inputDerivation` directly: its runtime closure includes
/// the `gcc-wrapper` but the wrapper depends on stdenv's setup-hook env
/// (`NIX_CC`, `NIX_CFLAGS_COMPILE`, ...) when invoked by basename off
/// `$PATH`. `buildEnv` instead makes `gcc` a SYMLINK into the wrapper's
/// store path, so `dirname $0` resolves to the real `cc-wrapper/bin`
/// and the wrapper finds its `nix-support/` config without env. Same
/// store paths read either way — the symlink farm is just routing.
const JQ_TOOLCHAIN_EXPR: &str = "with import <nixpkgs> {}; buildEnv { \
    name = \"rio-fuse-bench-jq-toolchain\"; \
    paths = [ jq.stdenv.cc jq.stdenv.shellPackage ] \
      ++ jq.stdenv.initialPath \
      ++ (jq.nativeBuildInputs or []) \
      ++ (jq.buildInputs or []); }";

/// Source tarball for the project under test. jq is small (~360 files),
/// pure C, vanilla autoconf, and builds in ~20 s on a few cores — short
/// enough to fit a criterion sample loop.
const JQ_SRC_EXPR: &str = "(import <nixpkgs> {}).jq.src";

/// Unpacked jq source tree + the basename of its tarball store path.
///
/// Extracted once outside the bench loop. Per iteration we copy the
/// pristine tree into a tmpfs (`./configure` and `make` write build
/// artifacts), so we measure store reads, not source-tree disk I/O.
struct JqSource {
    /// Tempdir holding the pristine (read-only) extracted tree.
    _src_dir: tempfile::TempDir,
    /// Path to the top-level `jq-*` directory inside `_src_dir`.
    tree: PathBuf,
}

impl JqSource {
    fn extract(tarball: &Path) -> Option<Self> {
        let src_dir = tempfile::tempdir().ok()?;
        let st = Command::new("tar")
            .arg("-xf")
            .arg(tarball)
            .arg("-C")
            .arg(src_dir.path())
            .status()
            .ok()?;
        if !st.success() {
            eprintln!(
                "[fuse bench] tar -xf {} failed, skipping jq_build",
                tarball.display()
            );
            return None;
        }
        let tree = fs::read_dir(src_dir.path()).ok()?.next()?.ok()?.path();
        Some(Self {
            _src_dir: src_dir,
            tree,
        })
    }
}

/// `./configure && make -jN` jq under bwrap. The toolchain closure is
/// read through `store_root`; the source tree is copied to a tmpfs so
/// build artifacts don't leak between iterations.
///
/// `./configure` is the FUSE-stressing phase: hundreds of
/// `compile-and-run` probes, each forking `cc` (~10 store paths in
/// shebang/RUNPATH chains), `cpp` opening dozens of headers, and `ld`
/// scanning library dirs. `make` then reads the same toolchain plus the
/// project source for every compilation unit.
///
/// The `buildEnv` symlink farm is bound at `/usr/bin` from the host
/// (it's metadata-only). Every symlink target is an absolute
/// `/nix/store/...` path resolved through `store_root` — the actual
/// toolchain bytes (cc1, ld, headers, glibc, bash) all go through FUSE.
fn bwrap_jq_build(store_root: &Path, toolchain: &Path, src_tree: &Path, jobs: usize) {
    let mut cmd = bwrap_with_store(store_root);
    cmd.args(["--ro-bind"]);
    cmd.arg(toolchain.join("bin")).arg("/usr/bin");
    cmd.args(["--symlink", "/usr/bin", "/bin"]);
    // Pristine source tree, read-only. Build happens in /tmp (tmpfs).
    cmd.args(["--ro-bind"]);
    cmd.arg(src_tree).arg("/src");
    cmd.args(["--setenv", "PATH", "/usr/bin"]);
    cmd.args(["--chdir", "/tmp"]);
    cmd.arg("sh").arg("-c").arg(format!(
        // Out-of-tree build is not supported by jq's autoconf setup, so
        // copy the tree into the tmpfs. The copy itself reads from the
        // host bind, not the FUSE mount, so it doesn't pollute the
        // measurement.
        // `--no-preserve=ownership`: bwrap's userns can't chown to the
        // source tree's host uid; the build doesn't care.
        "cp -r --no-preserve=ownership /src ./jq && cd jq && \
         ./configure --without-oniguruma --disable-docs >/dev/null 2>&1 && \
         make -j{jobs} >/dev/null 2>&1"
    ));
    run_or_panic(cmd.output().expect("spawn bwrap jq build"), "jq build");
}

// ---------------------------------------------------------------------------
// FUSE harness: tokio runtime + mock store + mounted NixStoreFs
// ---------------------------------------------------------------------------

/// Owns the tokio runtime, mock store, and FUSE mount. Drop unmounts.
/// Field order matters: `_mount` (which talks to the runtime through
/// the gRPC channel) must drop before `_rt`.
struct FuseHarness {
    mount_dir: tempfile::TempDir,
    cache: Arc<Cache>,
    store: MockStore,
    _mount: rio_builder::fuse::FuseMount,
    cache_dir: tempfile::TempDir,
    _rt: tokio::runtime::Runtime,
}

impl FuseHarness {
    fn new(fixture: &Fixture) -> Self {
        Self::new_seeded(
            fixture
                .nars
                .iter()
                .zip(&fixture.basenames)
                .map(|((_, nar, _), b)| (b.clone(), nar.clone()))
                .chain(std::iter::once((
                    fixture.big_basename.clone(),
                    fixture.big_nar.1.clone(),
                ))),
        )
    }

    /// Generic constructor: spawn mock store, seed with `(basename, nar)`
    /// pairs, arm the JIT allowlist, and mount.
    ///
    /// Arming the allowlist mirrors the executor calling
    /// `Cache::register_inputs` before a build's first `lookup`. Without
    /// it, `lookup` classifies every name `NotArmed` → ENOENT without
    /// contacting the store, and the bench only measures a no-op.
    fn new_seeded(paths: impl IntoIterator<Item = (String, Vec<u8>)>) -> Self {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("tokio runtime");
        let handle = rt.handle().clone();

        let (store, addr) = rt.block_on(async {
            let (store, addr, _h) = spawn_mock_store().await.expect("spawn mock store");
            (store, addr)
        });

        let cache_dir = tempfile::tempdir().expect("cache tempdir");
        let mount_dir = tempfile::tempdir().expect("mount tempdir");
        let cache = Arc::new(Cache::new(cache_dir.path().to_path_buf()).expect("Cache::new"));

        let mut inputs = Vec::new();
        for (basename, nar) in paths {
            let store_path = format!("/nix/store/{basename}");
            inputs.push((basename, nar.len() as u64));
            store.seed(make_path_info_for_nar(&store_path, &nar), nar);
        }
        cache.register_inputs(inputs);

        let clients = rt.block_on(async {
            let ch = rio_proto::client::connect_channel(&addr.to_string())
                .await
                .expect("connect mock store");
            StoreClients::from_channel(ch)
        });

        let (mount, _circuit) = mount_fuse_background(
            mount_dir.path(),
            Arc::clone(&cache),
            clients,
            handle,
            // Passthrough on: it's the production read path.
            //
            // Note: as of kernel 6.12, `FUSE_DEV_IOC_BACKING_OPEN`
            // requires `capable(CAP_SYS_ADMIN)` in the INIT user
            // namespace (`fs/fuse/passthrough.c`, marked TODO upstream).
            // Inside an unprivileged container this fails with EPERM
            // and NixStoreFs falls back to the dispatched read path.
            // The bench still runs — the warm-group numbers are then
            // the fallback path's, not passthrough's. The tracing
            // subscriber installed in `benches()` surfaces the
            // "open_backing failed" warning so you can tell which.
            true,
            4,
            Duration::from_secs(30),
        )
        .expect("mount FUSE");

        Self {
            mount_dir,
            cache,
            store,
            _mount: mount,
            cache_dir,
            _rt: rt,
        }
    }

    /// Drop the named cached paths so the next read forces a `GetPath`
    /// fetch. Also nukes the on-disk cache. Note: the kernel dentry
    /// cache (ATTR_TTL=3600s) is NOT dropped, so for paths the kernel
    /// has already resolved this only forces the FUSE `read`/`open`
    /// path, not a fresh `lookup`.
    fn evict(&self, basenames: impl IntoIterator<Item = impl AsRef<str>>) {
        for b in basenames {
            let b = b.as_ref();
            self.cache.remove_stale(b);
            let _ = fs::remove_file(self.cache_dir.path().join(b));
            // Closure store paths are directories, not flat files.
            let _ = fs::remove_dir_all(self.cache_dir.path().join(b));
        }
    }

    fn evict_all(&self, fixture: &Fixture) {
        self.evict(&fixture.basenames);
    }
}

// ---------------------------------------------------------------------------
// Benchmark groups
// ---------------------------------------------------------------------------

/// Local-filesystem baseline: same bwrap workload, plain directory.
/// Always runs — needs no FUSE.
fn bench_local_fs(c: &mut Criterion, fixture: &Fixture) {
    if !have_bwrap() {
        eprintln!("[fuse bench] bwrap unavailable, skipping local_fs baseline");
        return;
    }
    let dir = tempfile::tempdir().expect("local fixture dir");
    fixture.materialize_local(dir.path());

    let mut g = c.benchmark_group("local_fs");
    g.throughput(Throughput::Elements(N_PATHS as u64));
    g.bench_function("read_tree", |b| {
        b.iter(|| bwrap_read_tree(dir.path(), &fixture.basenames));
    });
    g.finish();
}

/// Warm-cache FUSE: every path is already materialized; the bench
/// measures pure FUSE dispatch overhead vs. local_fs.
fn bench_fuse_warm(c: &mut Criterion, fixture: &Fixture) {
    if !(have_bwrap() && have_fuse()) {
        eprintln!("[fuse bench] FUSE/bwrap unavailable, skipping fuse_warm");
        return;
    }
    let h = FuseHarness::new(fixture);
    // Pre-warm: one full read pass populates the cache via cold fetches.
    bwrap_read_tree(h.mount_dir.path(), &fixture.basenames);

    let mut g = c.benchmark_group("fuse_warm");
    g.throughput(Throughput::Elements(N_PATHS as u64));
    g.bench_function("read_tree", |b| {
        b.iter(|| bwrap_read_tree(h.mount_dir.path(), &fixture.basenames));
    });
    g.finish();
}

/// Cold-cache FUSE swept over RPC latency: every iteration evicts the
/// cache, then the bwrap reader's `lookup` triggers `GetPath` against a
/// mock store that sleeps `latency_ms` before responding.
fn bench_fuse_cold(c: &mut Criterion, fixture: &Fixture) {
    if !(have_bwrap() && have_fuse()) {
        eprintln!("[fuse bench] FUSE/bwrap unavailable, skipping fuse_cold");
        return;
    }
    let h = FuseHarness::new(fixture);

    let mut g = c.benchmark_group("fuse_cold");
    g.throughput(Throughput::Elements(N_PATHS as u64));
    // Each iteration is N_PATHS gRPC round-trips; at 5 ms that's
    // already ~250 ms. Cap sample size so the suite finishes in
    // reasonable time; the stddev is dominated by sleep jitter anyway.
    g.sample_size(20);
    g.measurement_time(Duration::from_secs(15));

    for &lat in LATENCIES_MS {
        h.store.faults.rpc_latency_ms.store(lat, Ordering::SeqCst);
        g.bench_with_input(
            BenchmarkId::new("read_tree", format!("{lat}ms")),
            &lat,
            |b, _| {
                b.iter_batched(
                    || h.evict_all(fixture),
                    |()| bwrap_read_tree(h.mount_dir.path(), &fixture.basenames),
                    criterion::BatchSize::PerIteration,
                );
            },
        );
    }
    g.finish();
}

/// fio random-read: per-`read(2)` latency view of the FUSE data plane.
/// One group, two sub-benchmarks (`local_fs` baseline + `fuse_warm`)
/// so they sit side-by-side in the criterion report.
///
/// No cold variant: a 16 MiB NAR fetch would dwarf the per-read signal
/// and just measure gRPC streaming throughput, which `fuse_cold` already
/// covers indirectly (and the upload bench covers directly).
fn bench_fio_randread(c: &mut Criterion, fixture: &Fixture) {
    if !(have_bwrap() && have_fio()) {
        eprintln!("[fuse bench] fio/bwrap unavailable, skipping fio_randread");
        return;
    }

    let mut g = c.benchmark_group("fio_randread");
    // Each sample is one fio invocation = FIO_RUNTIME_SECS of real
    // wall time, so cap aggressively.
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(15));

    // Local baseline.
    let local = tempfile::tempdir().expect("local fio dir");
    fixture.materialize_local(local.path());
    g.bench_function("local_fs", |b| {
        b.iter(|| bwrap_fio_randread(local.path(), &fixture.big_basename));
    });
    drop(local);

    // FUSE warm. Skip silently if the host can't mount.
    if have_fuse() {
        let h = FuseHarness::new(fixture);
        // Pre-warm the big path so fio measures FUSE data-plane reads,
        // not the cold gRPC fetch. In-process (no bwrap) read — just
        // needs to drive lookup/open/read once.
        let warmed = fs::read(h.mount_dir.path().join(&fixture.big_basename))
            .expect("pre-warm big path through FUSE");
        assert_eq!(
            warmed.len(),
            BIG_FILE_SIZE,
            "FUSE served truncated big file"
        );
        g.bench_function("fuse_warm", |b| {
            b.iter(|| bwrap_fio_randread(h.mount_dir.path(), &fixture.big_basename));
        });
    } else {
        eprintln!("[fuse bench] /dev/fuse unavailable, skipping fio_randread/fuse_warm");
    }
    g.finish();
}

/// Three-way benchmark group over a real closure:
///
/// - `local_fs`: bind the host's real `/nix/store` (speed-of-light).
/// - `fuse_warm`: same workload over the FUSE mount, cache pre-populated.
/// - `fuse_cold`: cache evicted between iterations — the worst-case
///   "build with nothing cached" path. Same ATTR_TTL caveat as
///   `fuse_cold/read_tree`: kernel dentry cache survives eviction.
///
/// `warm_secs`/`cold_secs` bound criterion's measurement window;
/// `sample_size` floor is 10, so bigger windows mean tighter CIs, not
/// more samples per second.
fn bench_real_closure(
    c: &mut Criterion,
    group: &str,
    closure: &RealClosure,
    warm_secs: u64,
    cold_secs: u64,
    workload: impl Fn(&Path),
) {
    if !have_bwrap() {
        eprintln!("[fuse bench] bwrap unavailable, skipping {group}");
        return;
    }
    let n_paths = closure.nars.len() as u64;
    let basenames = closure.basenames();

    let mut g = c.benchmark_group(group);
    g.throughput(Throughput::Elements(n_paths));
    g.sample_size(10);
    g.measurement_time(Duration::from_secs(warm_secs));

    // Local baseline: same workload, host's real /nix/store, no FUSE.
    g.bench_function("local_fs", |b| b.iter(|| workload(Path::new("/nix/store"))));

    if !have_fuse() {
        eprintln!("[fuse bench] FUSE unavailable, skipping {group}/fuse_*");
        g.finish();
        return;
    }

    let h = FuseHarness::new_seeded(closure.nars.iter().cloned());

    // Pre-warm: one full pass populates the cache via cold fetches.
    workload(h.mount_dir.path());
    g.bench_function("fuse_warm", |b| b.iter(|| workload(h.mount_dir.path())));

    g.measurement_time(Duration::from_secs(cold_secs));
    g.bench_function("fuse_cold", |b| {
        b.iter_batched(
            || h.evict(&basenames),
            |()| workload(h.mount_dir.path()),
            criterion::BatchSize::PerIteration,
        );
    });
    g.finish();
}

/// Real-world workload: cold-import a Python env (numpy/requests/yaml).
/// Many small `.py` reads, `.so` dlopen, deep symlink chains.
fn bench_python_closure(c: &mut Criterion) {
    let Some(closure) = RealClosure::build("python_closure", PYTHON_ENV_EXPR) else {
        return;
    };
    let env_path = closure.env_path.clone();
    bench_real_closure(c, "python_closure", &closure, 20, 60, |store_root| {
        bwrap_python(store_root, &env_path);
    });
}

/// Real-world workload: `./configure && make -jN` jq through FUSE.
/// Heaviest store reader in the suite — every `cc`/`as`/`ld` fork
/// resolves a chain of store paths, `cpp` opens dozens of headers per
/// translation unit, and `configure` does this hundreds of times in a
/// row for its compile-and-run probes.
fn bench_jq_build(c: &mut Criterion) {
    let Some(toolchain) = RealClosure::build("jq_build", JQ_TOOLCHAIN_EXPR) else {
        return;
    };
    // jq.src is the tarball, not part of the toolchain closure. Build
    // and unpack it once.
    let Some(src_tarball) = RealClosure::build("jq_build (jq.src)", JQ_SRC_EXPR) else {
        return;
    };
    let Some(src) = JqSource::extract(&src_tarball.env_path) else {
        return;
    };
    // ~20 s/iteration on a few cores — sample_size floor (10) means
    // ~3.5 min/group. Budget accordingly. Cap parallelism so the
    // benchmark is portable to small CI runners.
    let jobs = std::thread::available_parallelism().map_or(1, |n| n.get().min(4));
    let tc_path = toolchain.env_path.clone();
    let tree = src.tree.clone();
    bench_real_closure(c, "jq_build", &toolchain, 200, 400, move |store_root| {
        bwrap_jq_build(store_root, &tc_path, &tree, jobs);
    });
}

fn benches(c: &mut Criterion) {
    // Surface NixStoreFs's `setup_init` log: "FUSE passthrough enabled"
    // or "kernel lacks FUSE_PASSTHROUGH; disabling". Without a
    // subscriber the bench silently runs the fallback read path on
    // unsupported kernels and the warm/cold numbers look like a
    // passthrough regression. Filter to the fuse module only — the
    // cache and gRPC layers log per-request and would swamp criterion's
    // own output.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("rio_builder::fuse=info")),
        )
        .with_writer(std::io::stderr)
        .try_init();

    let fixture = Fixture::new();
    bench_local_fs(c, &fixture);
    bench_fuse_warm(c, &fixture);
    bench_fuse_cold(c, &fixture);
    bench_fio_randread(c, &fixture);
    bench_python_closure(c);
    bench_jq_build(c);
}

criterion_group!(fuse, benches);
criterion_main!(fuse);
