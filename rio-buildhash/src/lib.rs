//! Hash out-of-band proc-macro inputs into tracked rustc env-deps.
//!
//! Some proc macros read files that rustc's dep-info never sees on stable:
//! `sqlx::query!` reads `.sqlx/query-*.json` in offline mode, and
//! `sqlx::migrate!` embeds `migrations/*.sql` at expansion time. Cargo
//! papers over the gap with `cargo:rerun-if-changed`, which re-runs rustc —
//! but a content-keyed `RUSTC_WRAPPER` cache (kache; sccache-likes) then
//! sees byte-identical key inputs and restores the artifact compiled
//! against the *old* file contents: deterministic staleness that survives
//! `cargo clean`.
//!
//! The structural fix is to make the content reach rustc itself: build.rs
//! hashes the macro-visible files into `cargo:rustc-env=<NAME>=<hash>`, and
//! the consuming crate reads it with `const _: &str = env!("<NAME>");`.
//! rustc records the read as a `# env-dep:` line in dep-info, which is part
//! of both cargo's fingerprint and any dep-info-derived wrapper cache key —
//! a change re-keys the crate everywhere, and every dependent re-keys
//! transitively through the changed rlib hash.
//!
//! # The `SQLX_OFFLINE_DIR` contract
//!
//! [`track_sqlx`] performs NO discovery: it reads the offline cache
//! location exclusively from `SQLX_OFFLINE_DIR` — the variable
//! sqlx-macros-core 0.9 itself checks *first* in its own chain — so when
//! the variable points at the real cache, the hash and the macros agree by
//! construction. Every supported build context sets it explicitly:
//!
//! - the dev shell exports `<worktree>/.sqlx` (nix/devshell.nix shellHook,
//!   gated on a rio checkout marker, with an else-unset so stale inherited
//!   values cannot survive);
//! - crate2nix sandbox builds set it as a derivation env var pointing at
//!   the `.sqlx` fileset (nix/crate2nix.nix `sqlxOffline`);
//! - the cargo-mutants derivation exports it from its buildPhase once the
//!   unpacked workspace's `$PWD` is known, guarded on `.sqlx` existing so
//!   the mutants-smoke variant (which stages no `.sqlx` and compiles no
//!   sqlx crate) stays inert (nix/mutants.nix);
//! - the pre-commit `sqlx-prepare-check` re-pins it from its own toplevel.
//!
//! # Degraded states are unkeyed, never aliased
//!
//! sqlx-macros' offline lookup is per-query find-first-existing over
//! `[SQLX_OFFLINE_DIR, <manifest>/.sqlx, <workspace>/.sqlx]` — so in every
//! degraded state (variable unset, empty, relative, or pointing at
//! nothing) the compile may still SUCCEED against a real cache this
//! tracker cannot see. A constant sentinel there would be a *replayable*
//! cache key for an unobserved input — the exact staleness class this
//! crate exists to close. Instead, every degraded arm and every observed
//! race keys the build with a per-run UNIQUE value and emits an
//! always-stale watch (a `rerun-if-changed` on a never-existing path —
//! the only reliable always-rerun primitive: cargo does not re-run a
//! script because its emitted `rustc-env` VALUE changed, and a watched
//! mtime older than the script stamp is not stale). Consequence: degraded
//! sessions rebuild the sqlx crates on every build, with a warning, and
//! nothing they produce can ever be replayed from a wrapper cache — but
//! kache still STORES each unique-keyed miss (rlibs, dependents,
//! executables), so a prolonged degraded session floods the store with
//! write-only entries; prefer `KACHE_DISABLED=1` for such sessions. That
//! cost is deliberate — supported contexts always set the variable, and a
//! cache key must never claim to cover an input it cannot observe.
//!
//! The same per-query fallthrough cuts the other way in the Track arm:
//! resolving `SQLX_OFFLINE_DIR` to *a* directory does not make it the
//! *only* directory the macros can read. A replayable settled hash is
//! emitted only when the tracked dir is provably the only cache the
//! macros can read — when a fallthrough candidate (`<manifest>/.sqlx` or
//! the workspace root's `.sqlx`) exists with diverging macro-visible
//! content, or exists but cannot be verified, the build is unkeyed
//! exactly like the degraded arms (see `divergent_fallthrough`).
//!
//! Hashing covers exactly the macro-visible sets — top-level
//! `query-*.json` for the offline cache, top-level `<i64>_*.sql` for
//! migrations (`sqlx::migrate!` parses the version with `i64::from_str`,
//! so signed forms count) — so editor swap files and stray artifacts never
//! churn the key. Concurrency: the hash is computed twice; a file or the
//! directory vanishing mid-pass, or any disagreement between passes, is
//! treated as churn and keyed uniquely (a partially-read set must never
//! alias the legitimate hash of a smaller set). Residual race, by
//! construction unobservable from a build script: the macros re-read the
//! directory later, inside rustc — a swap landing in *that* window
//! mislabels one compile. Exposure requires an exit-0 compile against a
//! half-rewritten cache; `regen sqlx` itself runs kache-disabled, so the
//! racing writer is typically a background checker (rust-analyzer).
//!
//! The hash is FNV-1a 64: dependency-free and deterministic across
//! platforms, runs, and toolchains. It only needs to *change* when content
//! changes; it is not a security boundary.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Never-existing path watched to force the build script to re-run on
/// every build (cargo treats a missing watched path as always-stale).
/// Relative to the build script's cwd (the package root). If this file is
/// ever actually created, always-rerun silently degrades to a plain mtime
/// watch — keep the name obscure and do not create it.
const ALWAYS_RERUN_SENTINEL: &str = ".rio-buildhash-always-rerun";

/// Process-global latch for the store-cost advisory in [`unreplayable`]:
/// one build-script run is one process, so the advisory prints at most
/// once per script run even if a future script calls BOTH trackers (or
/// hits unkeyed and churn in the same run).
static ADVISORY_PRINTED: AtomicBool = AtomicBool::new(false);

fn fnv1a(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |acc, b| {
        (acc ^ u64::from(*b)).wrapping_mul(FNV_PRIME)
    })
}

/// Cargo-directive emitter: every `cargo:` line this crate produces goes
/// through one of these methods, so tests can assert exact emission per
/// arm against a `Vec<u8>` and production code cannot fork the format.
/// `advisory_printed` backs the once-per-run store-cost advisory in
/// [`unreplayable`]; production uses the process-global
/// [`ADVISORY_PRINTED`], tests pass a fresh `AtomicBool`.
struct Emitter<'a, W: Write> {
    out: W,
    advisory_printed: &'a AtomicBool,
}

impl<W: Write> Emitter<'_, W> {
    /// `cargo:rerun-if-env-changed=<var>`.
    fn watch_env(&mut self, var: &str) {
        self.line(format_args!("cargo:rerun-if-env-changed={var}"));
    }

    /// `cargo:rerun-if-changed=<path>`.
    fn watch(&mut self, path: &Path) {
        self.line(format_args!("cargo:rerun-if-changed={}", path.display()));
    }

    /// Watch a never-existing path: cargo re-runs the script on EVERY
    /// build (verified primitive — `stale: missing <path>` in cargo's
    /// fingerprint log). Emitting a different `rustc-env` value alone
    /// does NOT re-run a script, so this is the only way a
    /// degraded/churned state can heal on the next build.
    fn always_rerun(&mut self) {
        self.line(format_args!(
            "cargo:rerun-if-changed={ALWAYS_RERUN_SENTINEL}"
        ));
    }

    /// `cargo:warning=rio-buildhash: <msg>`.
    fn warning(&mut self, msg: &str) {
        self.line(format_args!("cargo:warning=rio-buildhash: {msg}"));
    }

    /// `cargo:rustc-env=<name>=<value>`.
    fn rustc_env(&mut self, name: &str, value: &str) {
        self.line(format_args!("cargo:rustc-env={name}={value}"));
    }

    fn line(&mut self, args: std::fmt::Arguments<'_>) {
        writeln!(self.out, "{args}").expect("rio-buildhash: writing a cargo directive failed");
    }
}

/// Production emitter: locked stdout (the cargo directive channel) plus
/// the process-global advisory latch.
fn stdout_emitter() -> Emitter<'static, std::io::StdoutLock<'static>> {
    Emitter {
        out: std::io::stdout().lock(),
        advisory_printed: &ADVISORY_PRINTED,
    }
}

/// Track the sqlx offline query cache as the `RIO_SQLX_HASH` env-dep.
///
/// Resolution is the `SQLX_OFFLINE_DIR` contract described in the module
/// docs — no discovery. The consuming crate MUST read the variable
/// (`const _: &str = env!("RIO_SQLX_HASH");`) or no env-dep is recorded
/// and the tracking silently does nothing.
pub fn track_sqlx() {
    let mut em = stdout_emitter();
    em.watch_env("SQLX_OFFLINE_DIR");
    let value = match sqlx_resolution(std::env::var_os("SQLX_OFFLINE_DIR").map(PathBuf::from)) {
        SqlxResolution::Untracked => unkeyed(
            &mut em,
            "SQLX_OFFLINE_DIR unset — the sqlx macros may still find a cache via their own \
             discovery, so this build is keyed uniquely (uncacheable); build inside the dev \
             shell or a nix sandbox for keyed builds",
        ),
        SqlxResolution::EmptyValue => unkeyed(
            &mut em,
            "SQLX_OFFLINE_DIR is set but empty — the sqlx macros fall through to their own \
             discovery; keying this build uniquely (uncacheable); unset it or point it at \
             the real .sqlx",
        ),
        SqlxResolution::NonAbsolute(dir) => unkeyed(
            &mut em,
            &format!(
                "refusing relative SQLX_OFFLINE_DIR ({}) — the build script resolves it against \
                 the package dir but sqlx-macros resolve it inside rustc against the workspace \
                 root; keying this build uniquely (uncacheable); set an absolute path",
                dir.display()
            ),
        ),
        SqlxResolution::Absent(dir) => unkeyed(
            &mut em,
            &format!(
                "SQLX_OFFLINE_DIR={} is not a directory — the sqlx macros fall through to their \
                 own discovery and may still compile, so this build is keyed uniquely \
                 (uncacheable); fix the variable (a dangling export from a removed worktree?) \
                 or restore the cache (git checkout -- .sqlx)",
                dir.display()
            ),
        ),
        SqlxResolution::Track(dir) => {
            em.watch(&dir);
            // Package root, the same assumption track_migrations relies
            // on for its cwd fallback: cargo and buildRustCrate both set
            // CARGO_MANIFEST_DIR and run the script from the package dir.
            let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();
            tracked_value(&mut em, &dir, &manifest_dir)
        }
    };
    em.rustc_env("RIO_SQLX_HASH", &value);
}

/// Track the crate's `migrations/` directory (top-level `<i64>_*.sql`,
/// the set `sqlx::migrate!` accepts) as the `RIO_MIGRATIONS_HASH` env-dep.
///
/// Contract split: absence at ENTRY is a broken checkout (the directory is
/// committed) and panics deterministically; a vanish DURING hashing is a
/// race with a concurrent writer and degrades to a unique churn key like
/// every other observed race.
pub fn track_migrations() {
    let dir = Path::new("migrations");
    assert!(
        dir.is_dir(),
        "rio-buildhash: migrations/ missing next to {}",
        std::env::current_dir().unwrap_or_default().display(),
    );
    let mut em = stdout_emitter();
    em.watch(dir);
    let value = settled_hash(&mut em, dir, is_migration_file);
    em.rustc_env("RIO_MIGRATIONS_HASH", &value);
}

/// Where the sqlx offline cache is, per the `SQLX_OFFLINE_DIR` contract.
/// Pure over the variable's value (filesystem checked at call time) so the
/// matrix is unit-testable without env mutation.
enum SqlxResolution {
    /// Variable unset — unsupported context.
    Untracked,
    /// Variable set but empty — sqlx falls through; refuse to guess.
    EmptyValue,
    /// Variable set but relative — the two readers would resolve it
    /// against different working directories; refuse.
    NonAbsolute(PathBuf),
    /// Variable set, absolute, but not an existing directory (missing, or
    /// an existing non-directory — either way the macros fall through).
    Absent(PathBuf),
    /// Variable set and resolvable.
    Track(PathBuf),
}

fn sqlx_resolution(var: Option<PathBuf>) -> SqlxResolution {
    match var {
        None => SqlxResolution::Untracked,
        Some(dir) if dir.as_os_str().is_empty() => SqlxResolution::EmptyValue,
        Some(dir) if dir.is_relative() => SqlxResolution::NonAbsolute(dir),
        Some(dir) if dir.is_dir() => SqlxResolution::Track(dir),
        Some(dir) => SqlxResolution::Absent(dir),
    }
}

/// The Track arm's value: route the fallthrough verdict to the settled
/// hash (threading the comparison snapshot into the three-way check) or
/// to the matching unkeyed diagnosis. Extracted from [`track_sqlx`] so
/// tests can drive verdict→emission against a captured buffer.
fn tracked_value<W: Write>(em: &mut Emitter<'_, W>, dir: &Path, manifest_dir: &Path) -> String {
    match divergent_fallthrough(em, dir, manifest_dir) {
        FallthroughVerdict::Divergent(twin) => unkeyed(
            em,
            &format!(
                "SQLX_OFFLINE_DIR={} diverges from the fallthrough cache at {} — \
                 sqlx-macros resolve each query-<hash>.json independently \
                 (find-first-existing), so a json missing under the tracked dir \
                 silently loads from the fallthrough copy this hash never saw; \
                 keying this build uniquely (uncacheable); re-point \
                 SQLX_OFFLINE_DIR at the cache you mean, or remove the divergent \
                 copy",
                dir.display(),
                twin.display()
            ),
        ),
        FallthroughVerdict::ProbeError(path, err) => unkeyed(
            em,
            &format!(
                "could not verify the fallthrough cache at {}: {err} — sqlx-macros \
                 resolve each query-<hash>.json independently and may still read \
                 from it, so a replayable key cannot claim to cover it; keying \
                 this build uniquely (uncacheable); fix the path's permissions or \
                 remove it",
                path.display()
            ),
        ),
        FallthroughVerdict::Benign(prior) => {
            settled_hash_against(em, dir, is_sqlx_query_file, prior.as_ref())
        }
    }
}

/// What the fallthrough probe concluded about the Track arm's right to
/// emit a replayable key.
///
/// Policy is by ROLE, not by error site: the TRACKED dir is an input the
/// build NEEDS — environmental failure there stays a loud deterministic
/// panic (via [`hash_matching_files`]). A fallthrough CANDIDATE is an
/// input the build does not need but cannot rule out — any probe failure
/// there degrades to an unkeyed build with a could-not-verify diagnosis:
/// never a panic, and never the content-divergence wording (an error is
/// a failed observation, not evidence of divergence).
#[derive(Debug)]
enum FallthroughVerdict {
    /// No surviving foreign candidate diverges. Carries the tracked
    /// snapshot hashed during candidate comparison (`None` when no
    /// comparison happened) so [`settled_hash_against`] can require
    /// three-way agreement — the settled hash must describe the same
    /// tracked state the benign verdict was computed against.
    Benign(Option<DirHash>),
    /// A foreign candidate exists with diverging macro-visible content.
    Divergent(PathBuf),
    /// A candidate exists (or appears to) but could not be verified:
    /// canonicalize or hashing failed at the carried path with the
    /// carried error.
    ProbeError(PathBuf, std::io::Error),
}

/// Probe the fallthrough `.sqlx` candidates and decide whether the Track
/// arm may emit a replayable key (see [`FallthroughVerdict`]).
///
/// Why: sqlx-macros-core 0.9.0 resolves the offline cache PER QUERY at
/// the FILE level — `src/query/mod.rs:97-101` builds the candidate list
/// `[SQLX_OFFLINE_DIR, <manifest>/.sqlx, workspace_root()/.sqlx]` and
/// `:107-108` does `.map(|path| path.join(&filename))
/// .find(|path| path.exists())`. Each `query-<hash>.json` is looked up
/// independently, so a json missing under the tracked dir silently loads
/// from a later candidate this tracker never hashed — a replayable key
/// over an unobserved input, the staleness class this crate exists to
/// close. The Track arm therefore emits a settled hash only when the
/// tracked dir is provably the only cache the macros can read.
///
/// Candidates probed: `<manifest>/.sqlx` (sqlx candidate 2, exact in
/// every layout) and `<manifest>/../.sqlx` (proxy for sqlx candidate 3 —
/// sqlx resolves the workspace root lazily by spawning `$CARGO metadata`,
/// `src/query/metadata.rs:29,:35,:38`, which a build script must not
/// replicate and buildRustCrate cannot: it never exports CARGO). The
/// `..` proxy is exact ONLY while the workspace stays flat — every
/// member directly under the root, true today for all 17 members. A
/// future nested member (e.g. `crates/foo`) silently stops probing the
/// real workspace root for that crate: move this probe with the layout.
/// Fuzz workspaces need no extra candidate — the tracker crates in any
/// fuzz tree are path deps whose manifest dirs are the real workspace
/// member dirs (never under `fuzz/<ws>/`) in the dev shell, and isolated
/// single-crate sources in the sandbox.
///
/// Watch rule, by construction: a candidate that EXISTS in any form is
/// watched at its LEXICAL spelling (single emission point, before any
/// verdict); a NotFound candidate is NEVER watched — cargo treats a
/// missing watched path as always-stale, and `<manifest>/.sqlx` never
/// exists in the dev shell, so watching it would force a rebuild on
/// every build. The lexical (not canonical) path keeps the watch alive
/// through the same-dir and non-dir skip arms, so a candidate later
/// replaced by a real cache dir, or a candidate symlink repointed at a
/// foreign tree, re-fires this script via the path's followed mtime. In
/// the dev shell this watches `<manifest>/../.sqlx` — a second lexical
/// spelling of the already-watched tracked dir; harmless, cargo stats
/// each watched path independently.
///
/// Per candidate: canonicalize NotFound => skip (an absent dir serves no
/// file — sqlx's `exists()` runs on the joined FILE path); any other
/// canonicalize error (ELOOP self-loop, EACCES on an unsearchable
/// parent) => `ProbeError`; canonicalize-equal to the tracked dir =>
/// skip (it IS the tracked dir, the devshell shape; canonical comparison
/// is robust to symlinks); non-directory => skip (nothing joins under
/// it). A surviving foreign candidate is compared by macro-visible
/// content hash against the tracked snapshot — hashed lazily AT MOST
/// ONCE per run via the panicking wrapper (tracked-side environmental
/// failure stays loud). The candidate side uses the non-panicking core:
/// hash error => `ProbeError`; `DirVanished` => skip (the same fact the
/// NotFound arm observes, just later — the already-emitted watch on a
/// now-missing path costs exactly one extra re-run, after which
/// canonicalize reports NotFound and the watch is no longer emitted);
/// `FileVanished` => `ProbeError` (a dangling entry in the CANDIDATE is
/// a failed observation, not content divergence). When the TRACKED
/// snapshot itself is non-`Hash` (the tracked dir is churning or has a
/// dangling entry), comparison is skipped for this and all remaining
/// candidates — they still get watches — and the snapshot flows out in
/// `Benign` so the three-way settled check routes to churn with correct
/// attribution (the persistent-dangling diagnostic then names the
/// tracked entry, not the candidate). Both hashes settled and equal =>
/// benign (identical bytes cannot leak anything the tracked hash does
/// not cover); settled and unequal => `Divergent`. Whole-set equality
/// deliberately over-fires when the divergence is confined to files the
/// tracked dir also contains (per-query masking means those bytes cannot
/// leak) — precision there would only buy cacheability inside an
/// already-misconfigured state, and the over-fire is loud and self-heals
/// once the twin is removed.
///
/// Returns `Benign(None)` when `tracked` itself fails to canonicalize
/// (vanished after the `is_dir` check) — `settled_hash_against`'s double
/// pass then reports the vanish with the churn wording.
///
/// Accepted residuals: a twin CREATED after a settled compile is unseen
/// until a watched input changes — watching a not-yet-existing path IS
/// the always-stale primitive (`ALWAYS_RERUN_SENTINEL`) and would defeat
/// caching entirely; the motivating stale-sibling case has the twin
/// existing at script time and is caught. Symlink-repoint coverage of
/// the lexical watches is PARTIAL under cargo's followed-mtime
/// semantics: repointing a candidate symlink at a tree whose mtime is
/// older than the script stamp does not re-fire — not closable without
/// that same always-stale watch. A `Divergent`/`ProbeError` early return
/// skips watching any LATER candidate that run; healing relies on the
/// always-rerun sentinel the unkeyed arm emits (one-build latency).
fn divergent_fallthrough<W: Write>(
    em: &mut Emitter<'_, W>,
    tracked: &Path,
    manifest_dir: &Path,
) -> FallthroughVerdict {
    let tracked_canon = match tracked.canonicalize() {
        Ok(canon) => canon,
        Err(_) => return FallthroughVerdict::Benign(None),
    };
    let candidates = [
        manifest_dir.join(".sqlx"),
        manifest_dir.join("..").join(".sqlx"),
    ];
    let mut snapshot: Option<DirHash> = None;
    for candidate in candidates {
        let canon_result = candidate.canonicalize();
        if matches!(&canon_result, Err(e) if e.kind() == std::io::ErrorKind::NotFound) {
            // NEVER watch a missing candidate (see the watch rule above).
            continue;
        }
        // The candidate exists in some form: the single watch-emission
        // point, lexical spelling, before any verdict.
        em.watch(&candidate);
        let canon = match canon_result {
            Ok(canon) => canon,
            Err(e) => return FallthroughVerdict::ProbeError(candidate, e),
        };
        if canon == tracked_canon || !canon.is_dir() {
            continue;
        }
        let prior =
            snapshot.get_or_insert_with(|| hash_matching_files(tracked, is_sqlx_query_file));
        let DirHash::Hash(tracked_hash) = prior else {
            // Tracked dir churning or dangling: no comparison against it
            // is meaningful. Keep watching the remaining candidates and
            // let the three-way settled check route to churn.
            continue;
        };
        match try_hash_matching_files(&canon, is_sqlx_query_file) {
            Err(e) => return FallthroughVerdict::ProbeError(canon, e),
            Ok(DirHash::DirVanished) => continue,
            Ok(DirHash::FileVanished(path)) => {
                return FallthroughVerdict::ProbeError(
                    path,
                    std::io::Error::new(
                        std::io::ErrorKind::NotFound,
                        "listed but unreadable (broken symlink?)",
                    ),
                );
            }
            Ok(DirHash::Hash(candidate_hash)) => {
                if *tracked_hash != candidate_hash {
                    return FallthroughVerdict::Divergent(canon);
                }
            }
        }
    }
    FallthroughVerdict::Benign(snapshot)
}

/// Degraded-state emission: warn, force a re-run on every build, and key
/// this build with a value nothing can ever replay. See the module docs
/// for why degraded states must be unkeyed rather than pinned to a
/// constant sentinel.
fn unkeyed<W: Write>(em: &mut Emitter<'_, W>, why: &str) -> String {
    unreplayable(em, "unkeyed", why)
}

/// The single unique-key emitter: every state that must not be replayable
/// — the degraded arms ([`unkeyed`]) and the settled churn arm — routes
/// through here. Emits the store-cost advisory at most once per
/// build-script run (the `advisory_printed` latch), the state-specific
/// warning, the always-stale watch, and returns a per-run-unique value.
fn unreplayable<W: Write>(em: &mut Emitter<'_, W>, prefix: &str, why: &str) -> String {
    if !em.advisory_printed.swap(true, Ordering::Relaxed) {
        em.warning(
            "builds in this state are uncacheable but still written to the kache store \
             under never-replayable keys — for prolonged sessions in this state, set \
             KACHE_DISABLED=1",
        );
    }
    em.warning(why);
    em.always_rerun();
    unique_value(prefix)
}

/// A value that is unique per build-script run: wall-clock nanos plus OS
/// hasher entropy, so even a frozen clock cannot repeat it. Used wherever
/// the tracked input was not fully observed — such a build must never be
/// replayable from a content-keyed cache.
fn unique_value(prefix: &str) -> String {
    use std::hash::{BuildHasher, RandomState};
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let entropy = RandomState::new().hash_one(0u64);
    format!("{prefix}-{nanos:x}-{entropy:016x}")
}

fn is_sqlx_query_file(name: &str) -> bool {
    name.starts_with("query-") && name.ends_with(".json")
}

/// `sqlx::migrate!` filename grammar: `<version>_<description>.sql`
/// (including the `.up.sql`/`.down.sql` reversible forms, which still end
/// in `.sql`). The resolver parses the version with `i64::from_str`, which
/// accepts signed forms (`+001_x.sql`) — match that, not just digits.
/// Anything else — `scratch.sql`, editor swap files, README — is silently
/// skipped by the macro and must not churn the hash.
fn is_migration_file(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(".sql") else {
        return false;
    };
    let Some((version, _description)) = rest.split_once('_') else {
        return false;
    };
    version.parse::<i64>().is_ok()
}

/// Outcome of one hashing pass. A partial read has no hash value to leak:
/// vanishing files and vanishing directories are distinct variants, not
/// silently-absorbed smaller sets.
#[derive(Debug, PartialEq)]
enum DirHash {
    Hash(String),
    DirVanished,
    FileVanished(PathBuf),
}

/// [`settled_hash_against`] with no fallthrough-comparison snapshot — the
/// migrations tracker and every shape where no candidate survived.
fn settled_hash<W: Write>(em: &mut Emitter<'_, W>, dir: &Path, pred: fn(&str) -> bool) -> String {
    settled_hash_against(em, dir, pred, None)
}

/// Hash the matching set twice; emit the hash only on three-way
/// agreement: both passes `Hash` and equal, AND equal to `prior` when the
/// fallthrough comparison took a snapshot (the benign verdict described
/// THAT tracked state — a settled hash differing from it would be a
/// replayable key the comparison never vouched for). Any disagreement,
/// vanish, or partial read is churn: warn, force a re-run next build, and
/// key this build uniquely (it can never be replayed; the next build
/// re-hashes the settled state because of the always-stale watch — NOT
/// because the env value changed, which cargo ignores).
fn settled_hash_against<W: Write>(
    em: &mut Emitter<'_, W>,
    dir: &Path,
    pred: fn(&str) -> bool,
    prior: Option<&DirHash>,
) -> String {
    let first = hash_matching_files(dir, pred);
    let second = hash_matching_files(dir, pred);
    match (first, second) {
        (DirHash::Hash(a), DirHash::Hash(b))
            if a == b && prior.is_none_or(|p| matches!(p, DirHash::Hash(h) if *h == a)) =>
        {
            a
        }
        (first, second) => {
            let why = churn_warning(dir, prior, &first, &second);
            unreplayable(em, "churn", &why)
        }
    }
}

/// Diagnostic for a settling disagreement (callers: [`settled_hash_against`]
/// from both trackers' settle paths). BOTH passes failing on the SAME
/// listed-but-unreadable path is not transient churn: only
/// `ErrorKind::NotFound` reaches `FileVanished` (other read errors are
/// `Err` in `try_hash_matching_files` — the tracked-dir wrapper panics on
/// them, so "permission denied" is never this arm), and a NotFound that
/// holds across two passes is a persistent dangling entry — typically a
/// broken symlink. The transient wording's "the next build re-hashes the
/// settled state" would be a misdiagnosis there: nothing settles until
/// the entry is removed. Passes that AGREE but contradict the
/// fallthrough-comparison snapshot are the compare-then-settle window —
/// named as such, since "changed while hashing" would point at the wrong
/// window. Every other combination keeps the transient-churn wording.
fn churn_warning(dir: &Path, prior: Option<&DirHash>, first: &DirHash, second: &DirHash) -> String {
    match (first, second) {
        (DirHash::FileVanished(a), DirHash::FileVanished(b)) if a == b => format!(
            "{} is listed but absent on read in both hashing passes — a persistent \
             dangling entry (broken symlink?), not transient churn; keying this build \
             uniquely (uncacheable) on every build until the entry is removed",
            a.display()
        ),
        (DirHash::Hash(a), DirHash::Hash(b))
            if a == b && prior.is_some_and(|p| !matches!(p, DirHash::Hash(h) if h == a)) =>
        {
            format!(
                "{} changed between the fallthrough comparison and settling — keying \
                 this build uniquely (uncacheable); the next build re-hashes the \
                 settled state",
                dir.display()
            )
        }
        _ => format!(
            "{} changed while hashing — keying this build uniquely (uncacheable); the \
             next build re-hashes the settled state",
            dir.display()
        ),
    }
}

/// Content hash of the top-level files in `dir` whose names match `pred`:
/// names + contents, independent of mtimes, ownership, listing order, and
/// non-matching files.
///
/// Error policy: a directory or listed file vanishing mid-pass returns the
/// corresponding `Ok` variant (the caller treats it as churn — a hash over
/// a partially-read set would alias the legitimate hash of a smaller set
/// and replay a stale artifact with zero diagnostics). Every other probe
/// failure — `read_dir` on an existing-but-unreadable dir, a per-entry
/// iterator error, a non-NotFound read — is `Err`, with the failing op and
/// path in the message and the original `ErrorKind` preserved for the
/// caller's taxonomy. POLICY LIVES IN THE CALLERS: the tracked dir goes
/// through the panicking wrapper [`hash_matching_files`]; fallthrough
/// candidates call this directly and degrade to
/// [`FallthroughVerdict::ProbeError`].
fn try_hash_matching_files(dir: &Path, pred: fn(&str) -> bool) -> Result<DirHash, std::io::Error> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DirHash::DirVanished),
        Err(e) => {
            return Err(std::io::Error::new(
                e.kind(),
                format!("read_dir({}) failed: {e}", dir.display()),
            ));
        }
    };
    let mut names: Vec<String> = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| {
            std::io::Error::new(
                e.kind(),
                format!("dir entry under {} failed: {e}", dir.display()),
            )
        })?;
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        if pred(&name) && !entry.path().is_dir() {
            names.push(name);
        }
    }
    names.sort();

    let mut state = FNV_OFFSET;
    for name in names {
        let path = dir.join(&name);
        let contents = match std::fs::read(&path) {
            Ok(c) => c,
            // Listed but unreadable: deleted (or a dangling symlink)
            // between listing and read — a churn signal, never a skip.
            // Carries the path so the caller can tell a persistent
            // dangling entry (same path both passes) from transient
            // churn.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(DirHash::FileVanished(path));
            }
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!("read({}) failed: {e}", path.display()),
                ));
            }
        };
        state = fnv1a(state, name.as_bytes());
        // Separator that cannot appear in UTF-8 name bytes, so
        // ("ab", "c") never collides with ("a", "bc").
        state = fnv1a(state, &[0xFF]);
        state = fnv1a(state, &(contents.len() as u64).to_le_bytes());
        state = fnv1a(state, &contents);
    }
    Ok(DirHash::Hash(format!("{state:016x}")))
}

/// Tracked-dir policy wrapper over [`try_hash_matching_files`]:
/// environmental failure on an input the build NEEDS (the tracked sqlx
/// cache, `migrations/`) stays a loud deterministic panic, with the
/// failing op, path, and errno preserved in the message.
fn hash_matching_files(dir: &Path, pred: fn(&str) -> bool) -> DirHash {
    try_hash_matching_files(dir, pred)
        .unwrap_or_else(|e| panic!("rio-buildhash: hashing {} failed: {e}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::{
        DirHash, Emitter, FallthroughVerdict, SqlxResolution, churn_warning, divergent_fallthrough,
        hash_matching_files, is_migration_file, is_sqlx_query_file, settled_hash,
        settled_hash_against, sqlx_resolution, tracked_value, try_hash_matching_files,
        unique_value, unkeyed,
    };
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::AtomicBool;

    fn sqlx_hash(dir: &Path) -> String {
        match hash_matching_files(dir, is_sqlx_query_file) {
            DirHash::Hash(h) => h,
            other => panic!("expected settled hash, got {other:?}"),
        }
    }

    /// Buffer-backed emitter for asserting exact directive emission.
    fn capture(flag: &AtomicBool) -> Emitter<'_, Vec<u8>> {
        Emitter {
            out: Vec::new(),
            advisory_printed: flag,
        }
    }

    fn output(em: Emitter<'_, Vec<u8>>) -> String {
        String::from_utf8(em.out).expect("utf8 directives")
    }

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("query-aa.json"), b"{\"q\": 1}").unwrap();
        fs::write(dir.path().join("query-bb.json"), b"{\"q\": 2}").unwrap();
        dir
    }

    /// chmod `dir` to 0o000 and hand back a guard restoring 0o755 on drop
    /// (BEFORE the TempDir drops, so cleanup never leaks). Returns `None`
    /// — after restoring — when the kernel does not enforce the mode
    /// (root / CAP_DAC_OVERRIDE), so callers can skip gracefully.
    struct ModeGuard(PathBuf);
    impl Drop for ModeGuard {
        fn drop(&mut self) {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&self.0, fs::Permissions::from_mode(0o755));
        }
    }
    fn deny_all(dir: &Path) -> Option<ModeGuard> {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).unwrap();
        let guard = ModeGuard(dir.to_path_buf());
        if fs::read_dir(dir).is_ok() {
            return None; // not enforced; guard drop restores the mode
        }
        Some(guard)
    }

    #[test]
    fn stable_across_calls() {
        let dir = setup();
        assert_eq!(sqlx_hash(dir.path()), sqlx_hash(dir.path()));
        // And the settled (double-pass) wrapper agrees on a quiet dir.
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        assert_eq!(
            settled_hash(&mut em, dir.path(), is_sqlx_query_file),
            sqlx_hash(dir.path())
        );
        assert!(em.out.is_empty(), "a quiet settle emits no directives");
    }

    #[test]
    fn hash_value_pinned_canary() {
        // Structural guard on the FNV accumulation: this literal was
        // computed by the PRE-refactor hash_matching_files over this
        // exact fixture (the `setup()` files). If it moves, every settled
        // key in every kache store shifts — that is a cache-wide
        // invalidation, not a refactor.
        let dir = setup();
        assert_eq!(
            hash_matching_files(dir.path(), is_sqlx_query_file),
            DirHash::Hash("f63e7c0a1d10f318".into())
        );
    }

    #[test]
    fn mtime_does_not_matter() {
        let dir = setup();
        let before = sqlx_hash(dir.path());
        fs::write(dir.path().join("query-aa.json"), b"{\"q\": 1}").unwrap();
        assert_eq!(before, sqlx_hash(dir.path()));
    }

    #[test]
    fn content_change_changes_hash() {
        let dir = setup();
        let before = sqlx_hash(dir.path());
        fs::write(dir.path().join("query-aa.json"), b"{\"q\": 9}").unwrap();
        assert_ne!(before, sqlx_hash(dir.path()));
    }

    #[test]
    fn file_add_and_remove_change_hash() {
        let dir = setup();
        let before = sqlx_hash(dir.path());
        fs::write(dir.path().join("query-cc.json"), b"{}").unwrap();
        let added = sqlx_hash(dir.path());
        assert_ne!(before, added);
        fs::remove_file(dir.path().join("query-cc.json")).unwrap();
        assert_eq!(before, sqlx_hash(dir.path()));
    }

    #[test]
    fn rename_changes_hash() {
        let dir = setup();
        let before = sqlx_hash(dir.path());
        fs::rename(
            dir.path().join("query-aa.json"),
            dir.path().join("query-zz.json"),
        )
        .unwrap();
        assert_ne!(before, sqlx_hash(dir.path()));
    }

    #[test]
    fn only_macro_visible_files_are_hashed() {
        let clean = setup();
        let noisy = setup();
        fs::write(noisy.path().join("README.md"), b"docs").unwrap();
        fs::write(noisy.path().join(".query-aa.json.swp"), b"vim").unwrap();
        fs::write(noisy.path().join("query-aa.json~"), b"backup").unwrap();
        fs::create_dir(noisy.path().join("sub")).unwrap();
        fs::write(noisy.path().join("sub/query-cc.json"), b"{}").unwrap();
        assert_eq!(sqlx_hash(clean.path()), sqlx_hash(noisy.path()));
    }

    #[test]
    fn migration_filter_matches_migrate_grammar() {
        assert!(is_migration_file("001_init.sql"));
        assert!(is_migration_file("20240101000000_widgets.up.sql"));
        assert!(is_migration_file("2_two.down.sql"));
        // sqlx parses the version with i64::from_str — signed forms count.
        assert!(is_migration_file("+001_init.sql"));
        assert!(is_migration_file("-1_weird.sql"));
        // Skipped by sqlx::migrate! — must not churn the hash.
        assert!(!is_migration_file("scratch.sql"));
        assert!(!is_migration_file("001_init.sql~"));
        assert!(!is_migration_file("_no_version.sql"));
        assert!(!is_migration_file("a1_bad.sql"));
        assert!(!is_migration_file("99999999999999999999_overflow.sql"));
        assert!(!is_migration_file("README.md"));
    }

    #[test]
    fn try_hash_missing_dir_is_dir_vanished() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            try_hash_matching_files(&root.path().join("gone"), is_sqlx_query_file).unwrap(),
            DirHash::DirVanished
        );
    }

    #[test]
    fn try_hash_dangling_entry_is_file_vanished() {
        let dir = setup();
        let dangling = dir.path().join("query-dd.json");
        std::os::unix::fs::symlink("nonexistent", &dangling).unwrap();
        assert_eq!(
            try_hash_matching_files(dir.path(), is_sqlx_query_file).unwrap(),
            DirHash::FileVanished(dangling)
        );
    }

    #[test]
    fn try_hash_unreadable_dir_is_err_with_context() {
        let dir = setup();
        let Some(_guard) = deny_all(dir.path()) else {
            eprintln!("skipping: directory permissions not enforced (root or CAP_DAC_OVERRIDE)");
            return;
        };
        let err = try_hash_matching_files(dir.path(), is_sqlx_query_file).unwrap_err();
        // Kind survives for the caller's taxonomy; op + path survive for
        // the diagnostic.
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        let msg = err.to_string();
        assert!(msg.contains("read_dir("), "{msg}");
        assert!(msg.contains(&dir.path().display().to_string()), "{msg}");
    }

    #[test]
    fn tracked_policy_wrapper_panics_loud() {
        let dir = setup();
        let shown = dir.path().display().to_string();
        let Some(_guard) = deny_all(dir.path()) else {
            eprintln!("skipping: directory permissions not enforced (root or CAP_DAC_OVERRIDE)");
            return;
        };
        let path = dir.path().to_path_buf();
        let payload =
            std::panic::catch_unwind(move || hash_matching_files(&path, is_sqlx_query_file))
                .expect_err("tracked-dir policy must stay a loud panic");
        let msg = payload
            .downcast_ref::<String>()
            .cloned()
            .unwrap_or_else(|| "non-string panic payload".into());
        assert!(msg.contains("rio-buildhash: hashing"), "{msg}");
        assert!(msg.contains("read_dir("), "{msg}");
        assert!(msg.contains(&shown), "{msg}");
        assert!(msg.contains("Permission denied"), "{msg}");
    }

    #[test]
    fn vanished_file_routes_to_churn() {
        // A listed-but-unreadable file (dangling symlink stands in for the
        // delete-between-list-and-read race) must NOT silently alias the
        // hash of the smaller set — it is churn, keyed uniquely. The
        // variant carries the offending path for the persistent-entry
        // diagnostic.
        let dir = setup();
        std::os::unix::fs::symlink("nonexistent", dir.path().join("query-dd.json")).unwrap();
        assert_eq!(
            hash_matching_files(dir.path(), is_sqlx_query_file),
            DirHash::FileVanished(dir.path().join("query-dd.json"))
        );
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let churned = settled_hash(&mut em, dir.path(), is_sqlx_query_file);
        assert!(churned.starts_with("churn-"), "{churned}");
        // And it is unique per call, never replayable.
        assert_ne!(
            churned,
            settled_hash(&mut em, dir.path(), is_sqlx_query_file)
        );
    }

    #[test]
    fn churn_warning_same_path_double_vanish_is_persistent() {
        let dir = PathBuf::from("/cache/.sqlx");
        let gone = dir.join("query-dd.json");
        let msg = churn_warning(
            &dir,
            None,
            &DirHash::FileVanished(gone.clone()),
            &DirHash::FileVanished(gone.clone()),
        );
        // Names the dangling entry itself and diagnoses persistence.
        assert!(msg.contains("/cache/.sqlx/query-dd.json"), "{msg}");
        assert!(msg.contains("persistent dangling entry"), "{msg}");
        assert!(msg.contains("until the entry is removed"), "{msg}");
        assert!(!msg.contains("changed while hashing"), "{msg}");
    }

    #[test]
    fn churn_warning_mixed_pair_is_transient() {
        let dir = PathBuf::from("/cache/.sqlx");
        for (first, second) in [
            // Vanish in one pass only — transient.
            (
                DirHash::FileVanished(dir.join("query-dd.json")),
                DirHash::Hash("abc".into()),
            ),
            // Different paths across passes — concurrent rewrite, not a
            // single persistent entry.
            (
                DirHash::FileVanished(dir.join("query-dd.json")),
                DirHash::FileVanished(dir.join("query-ee.json")),
            ),
            // Plain hash disagreement.
            (DirHash::Hash("abc".into()), DirHash::Hash("def".into())),
            // Whole directory vanished.
            (DirHash::DirVanished, DirHash::DirVanished),
        ] {
            let msg = churn_warning(&dir, None, &first, &second);
            assert!(msg.contains("changed while hashing"), "{msg}");
            assert!(msg.contains("/cache/.sqlx"), "{msg}");
            assert!(!msg.contains("persistent"), "{msg}");
        }
    }

    #[test]
    fn churn_warning_vanished_prior_then_agreeing_hash_is_prior_mismatch() {
        // A dangling entry observed during the fallthrough comparison
        // that HEALS before settling (both passes agree on a hash) must
        // route to the prior-mismatch arm — the state did change in the
        // window — and, per settled_hash_against, never emit that hash.
        let dir = PathBuf::from("/cache/.sqlx");
        let msg = churn_warning(
            &dir,
            Some(&DirHash::FileVanished(dir.join("query-aa.json"))),
            &DirHash::Hash("bbbb".into()),
            &DirHash::Hash("bbbb".into()),
        );
        assert!(
            msg.contains("changed between the fallthrough comparison and settling"),
            "{msg}"
        );
    }

    #[test]
    fn churn_warning_prior_mismatch_names_the_window() {
        let dir = PathBuf::from("/cache/.sqlx");
        // Passes agree with each other but contradict the snapshot the
        // fallthrough comparison was benign against.
        let msg = churn_warning(
            &dir,
            Some(&DirHash::Hash("aaaa".into())),
            &DirHash::Hash("bbbb".into()),
            &DirHash::Hash("bbbb".into()),
        );
        assert!(
            msg.contains("changed between the fallthrough comparison and settling"),
            "{msg}"
        );
        assert!(msg.contains("/cache/.sqlx"), "{msg}");
        assert!(!msg.contains("changed while hashing"), "{msg}");
        // Pass disagreement stays the transient wording even with a prior.
        let transient = churn_warning(
            &dir,
            Some(&DirHash::Hash("bbbb".into())),
            &DirHash::Hash("bbbb".into()),
            &DirHash::Hash("cccc".into()),
        );
        assert!(transient.contains("changed while hashing"), "{transient}");
    }

    #[test]
    fn vanished_dir_routes_to_churn() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        fs::create_dir(&gone).unwrap();
        fs::remove_dir(&gone).unwrap();
        assert_eq!(
            hash_matching_files(&gone, is_sqlx_query_file),
            DirHash::DirVanished
        );
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let churned = settled_hash(&mut em, &gone, is_sqlx_query_file);
        assert!(churned.starts_with("churn-"), "{churned}");
        // The settled churn arm routes through the same unique-key
        // emitter as the degraded arms — store-cost advisory included.
        let out = output(em);
        assert!(
            out.contains("uncacheable but still written to the kache store"),
            "{out}"
        );
        assert!(out.contains(".rio-buildhash-always-rerun"), "{out}");
    }

    #[test]
    fn unique_values_never_repeat() {
        let a = unique_value("unkeyed");
        let b = unique_value("unkeyed");
        assert_ne!(a, b);
        assert!(a.starts_with("unkeyed-"));
    }

    #[test]
    fn advisory_printed_once_per_emitter_session() {
        let root = tempfile::tempdir().unwrap();
        let gone = root.path().join("gone");
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        // Two unique-key routes in one script run: a degraded arm, then
        // the settled churn arm.
        let a = unkeyed(&mut em, "first degraded state");
        let b = settled_hash(&mut em, &gone, is_sqlx_query_file);
        assert!(a.starts_with("unkeyed-"), "{a}");
        assert!(b.starts_with("churn-"), "{b}");
        let out = output(em);
        assert_eq!(
            out.matches("uncacheable but still written to the kache store")
                .count(),
            1,
            "{out}"
        );
        assert!(out.contains("first degraded state"), "{out}");
        assert!(out.contains("changed while hashing"), "{out}");
        // Both routes still emit their own always-stale watch.
        assert_eq!(
            out.matches(".rio-buildhash-always-rerun").count(),
            2,
            "{out}"
        );
    }

    #[test]
    fn name_content_boundary_no_collision() {
        let d1 = tempfile::tempdir().unwrap();
        fs::write(d1.path().join("query-ab.json"), b"c").unwrap();
        let d2 = tempfile::tempdir().unwrap();
        fs::write(d2.path().join("query-a.json"), b"b.jsonc").unwrap();
        assert_ne!(sqlx_hash(d1.path()), sqlx_hash(d2.path()));
    }

    /// A two-file macro-visible query set; `tag` differentiates content
    /// generations (v1 vs v2) without changing the name set.
    fn write_queries(dir: &Path, tag: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("query-aa.json"), format!("{{\"q\": \"{tag}-1\"}}")).unwrap();
        fs::write(dir.join("query-bb.json"), format!("{{\"q\": \"{tag}-2\"}}")).unwrap();
    }

    #[test]
    fn divergence_absent_candidates_benign() {
        // The crate2nix sandbox shape: neither <manifest>/.sqlx nor
        // <manifest>/../.sqlx exists — nothing to fall through to, and
        // (the bug_005 NEVER-rule) nothing watched: a missing watched
        // path is always-stale to cargo.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        assert!(
            matches!(verdict, FallthroughVerdict::Benign(None)),
            "{verdict:?}"
        );
        assert!(em.out.is_empty(), "NotFound candidates are never watched");
    }

    #[test]
    fn divergence_manifest_dotsqlx_identical_benign() {
        // A byte-identical manifest/.sqlx serves nothing the tracked
        // hash does not already cover; the comparison snapshot flows out
        // for the three-way settled check.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        write_queries(&manifest.join(".sqlx"), "v1");
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        assert!(
            matches!(
                &verdict,
                FallthroughVerdict::Benign(Some(DirHash::Hash(h))) if *h == sqlx_hash(&tracked)
            ),
            "{verdict:?}"
        );
        let out = output(em);
        assert!(
            out.contains(&format!(
                "cargo:rerun-if-changed={}",
                manifest.join(".sqlx").display()
            )),
            "{out}"
        );
    }

    #[test]
    fn divergence_workspace_self_benign() {
        // The devshell no-op in unit form: tracked IS the workspace
        // root's .sqlx, reached via the manifest/../.sqlx probe. The
        // same-dir skip still watches the candidate at its LEXICAL
        // spelling — a duplicate spelling of the tracked-dir watch,
        // harmless (cargo stats each watched path independently), and
        // the hook that re-fires the script if the path is ever
        // repointed at a foreign tree.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join(".sqlx");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        assert!(
            matches!(verdict, FallthroughVerdict::Benign(None)),
            "{verdict:?}"
        );
        assert_eq!(
            output(em),
            format!(
                "cargo:rerun-if-changed={}\n",
                manifest.join("..").join(".sqlx").display()
            )
        );
    }

    #[test]
    fn divergence_symlinked_tracked_benign() {
        // Same-dir detection must survive symlinks: manifest/.sqlx is a
        // symlink TO the tracked dir, not a second cache. The candidate
        // is still watched (lexically) so a later repoint re-fires.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("real-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        std::os::unix::fs::symlink(&tracked, manifest.join(".sqlx")).unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        assert!(
            matches!(verdict, FallthroughVerdict::Benign(None)),
            "{verdict:?}"
        );
        let out = output(em);
        assert!(
            out.contains(&format!(
                "cargo:rerun-if-changed={}",
                manifest.join(".sqlx").display()
            )),
            "{out}"
        );
    }

    #[test]
    fn divergence_manifest_extra_query_fires() {
        // One query json present in the fallthrough but not in the
        // tracked dir: sqlx would load it, the hash never saw it.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        let twin = manifest.join(".sqlx");
        write_queries(&twin, "v1");
        fs::write(twin.join("query-cc.json"), b"{\"q\": \"extra\"}").unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        let FallthroughVerdict::Divergent(path) = verdict else {
            panic!("expected Divergent, got {verdict:?}");
        };
        assert_eq!(path, twin.canonicalize().unwrap());
        let out = output(em);
        assert!(
            out.contains(&format!("cargo:rerun-if-changed={}", twin.display())),
            "{out}"
        );
    }

    #[test]
    fn divergence_stale_sibling_fires() {
        // The motivating case: SQLX_OFFLINE_DIR points at a stale
        // sibling worktree's .sqlx while building in THIS worktree —
        // a json missing under the tracked sibling silently loads from
        // this worktree's root .sqlx.
        let sibling = tempfile::tempdir().unwrap();
        let this_worktree = tempfile::tempdir().unwrap();
        let tracked = sibling.path().join(".sqlx");
        write_queries(&tracked, "v1");
        fs::remove_file(tracked.join("query-bb.json")).unwrap();
        let workspace_sqlx = this_worktree.path().join(".sqlx");
        write_queries(&workspace_sqlx, "v2");
        let manifest = this_worktree.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        let FallthroughVerdict::Divergent(path) = verdict else {
            panic!("expected Divergent, got {verdict:?}");
        };
        assert_eq!(path, workspace_sqlx.canonicalize().unwrap());
        // Watched at the lexical ../.sqlx spelling it was probed under.
        let out = output(em);
        assert!(
            out.contains(&format!(
                "cargo:rerun-if-changed={}",
                manifest.join("..").join(".sqlx").display()
            )),
            "{out}"
        );
    }

    #[test]
    fn divergence_non_macro_files_benign() {
        // Only the macro-visible set matters: a fallthrough dir with an
        // identical query set plus editor noise is not divergent.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        let twin = manifest.join(".sqlx");
        write_queries(&twin, "v1");
        fs::write(twin.join("README.md"), b"docs").unwrap();
        fs::write(twin.join(".query-aa.json.swp"), b"vim").unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        assert!(
            matches!(verdict, FallthroughVerdict::Benign(Some(DirHash::Hash(_)))),
            "{verdict:?}"
        );
    }

    #[test]
    fn divergence_non_dir_candidate_benign() {
        // A regular FILE named .sqlx serves no query json (sqlx joins
        // filenames under it and exists() fails) — skip, don't hash it.
        // It IS watched: if it is later replaced by a real cache dir,
        // the script must re-run.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        fs::write(manifest.join(".sqlx"), b"not a directory").unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        assert!(
            matches!(verdict, FallthroughVerdict::Benign(None)),
            "{verdict:?}"
        );
        let out = output(em);
        assert!(
            out.contains(&format!(
                "cargo:rerun-if-changed={}",
                manifest.join(".sqlx").display()
            )),
            "{out}"
        );
    }

    #[test]
    fn unreadable_candidate_is_probe_error_not_panic() {
        // The verified F.i case: an existing-but-000 candidate twin used
        // to PANIC the consumer's build via hash_matching_files; it is a
        // probe failure on an input the build does not need — degrade.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        let twin = manifest.join(".sqlx");
        write_queries(&twin, "v2");
        let twin_canon = twin.canonicalize().unwrap();
        let Some(_guard) = deny_all(&twin) else {
            eprintln!("skipping: directory permissions not enforced (root or CAP_DAC_OVERRIDE)");
            return;
        };
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        let FallthroughVerdict::ProbeError(path, err) = verdict else {
            panic!("expected ProbeError, got {verdict:?}");
        };
        assert_eq!(path, twin_canon);
        assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
        assert!(err.to_string().contains("read_dir("), "{err}");
        // Watched before the verdict: the candidate exists.
        let out = output(em);
        assert!(
            out.contains(&format!("cargo:rerun-if-changed={}", twin.display())),
            "{out}"
        );
        // Full Track arm: unkeyed could-not-verify, never the
        // divergence wording, no panic.
        let flag2 = AtomicBool::new(false);
        let mut em2 = capture(&flag2);
        let value = tracked_value(&mut em2, &tracked, &manifest);
        assert!(value.starts_with("unkeyed-"), "{value}");
        let out2 = output(em2);
        assert!(
            out2.contains("could not verify the fallthrough cache at"),
            "{out2}"
        );
        assert!(out2.contains(&twin_canon.display().to_string()), "{out2}");
        assert!(!out2.contains("diverges from"), "{out2}");
    }

    #[test]
    fn self_loop_candidate_is_probe_error_with_watch() {
        // The verified F.iii case: ELOOP at candidate canonicalize used
        // to take the divergence arm with the divergence wording; it is
        // a probe failure — could-not-verify, with the lexical path
        // carried and watched.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        let candidate = manifest.join(".sqlx");
        std::os::unix::fs::symlink(&candidate, &candidate).unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        let FallthroughVerdict::ProbeError(path, err) = verdict else {
            panic!("expected ProbeError, got {verdict:?}");
        };
        assert_eq!(path, candidate);
        // ErrorKind::FilesystemLoop is still unstable (io_error_more);
        // assert the preserved raw errno instead: ELOOP == 40 on Linux.
        assert_eq!(err.raw_os_error(), Some(40), "{err}");
        assert_ne!(err.kind(), std::io::ErrorKind::NotFound);
        let out = output(em);
        assert!(
            out.contains(&format!("cargo:rerun-if-changed={}", candidate.display())),
            "{out}"
        );
        // And the Track arm degrades with the could-not-verify wording.
        let flag2 = AtomicBool::new(false);
        let mut em2 = capture(&flag2);
        let value = tracked_value(&mut em2, &tracked, &manifest);
        assert!(value.starts_with("unkeyed-"), "{value}");
        let out2 = output(em2);
        assert!(
            out2.contains("could not verify the fallthrough cache at"),
            "{out2}"
        );
        assert!(!out2.contains("diverges from"), "{out2}");
    }

    #[test]
    fn dangling_candidate_entry_is_probe_error_not_divergent() {
        // The candidate half of verified F.ii: a dangling entry inside
        // the CANDIDATE used to fall into the divergence arm — it is a
        // failed observation of the candidate, not content divergence.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        let twin = manifest.join(".sqlx");
        write_queries(&twin, "v1");
        std::os::unix::fs::symlink("nonexistent", twin.join("query-dd.json")).unwrap();
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        let FallthroughVerdict::ProbeError(path, err) = verdict else {
            panic!("expected ProbeError, got {verdict:?}");
        };
        assert_eq!(path, twin.canonicalize().unwrap().join("query-dd.json"));
        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
        assert!(err.to_string().contains("listed but unreadable"), "{err}");
    }

    #[test]
    fn tracked_dangling_entry_settles_to_persistent_churn() {
        // The tracked half of verified F.ii: a dangling entry in the
        // TRACKED dir with a healthy foreign candidate used to be
        // misattributed as candidate divergence; the snapshot now flows
        // into the three-way settled check and the persistent-dangling
        // diagnostic fires naming the TRACKED entry.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let dangling = tracked.join("query-dd.json");
        std::os::unix::fs::symlink("nonexistent", &dangling).unwrap();
        let manifest = root.path().join("crate");
        write_queries(&manifest.join(".sqlx"), "v1");
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        let FallthroughVerdict::Benign(Some(prior)) = verdict else {
            panic!("expected Benign(Some(FileVanished)), got {verdict:?}");
        };
        assert_eq!(prior, DirHash::FileVanished(dangling.clone()));
        // The candidate is still watched.
        let watched = format!(
            "cargo:rerun-if-changed={}",
            manifest.join(".sqlx").display()
        );
        assert!(
            String::from_utf8(em.out.clone())
                .unwrap()
                .contains(&watched),
            "missing candidate watch"
        );
        // Settling against that prior fires the persistent-dangling
        // diagnostic with tracked-side attribution.
        let value = settled_hash_against(&mut em, &tracked, is_sqlx_query_file, Some(&prior));
        assert!(value.starts_with("churn-"), "{value}");
        let out = output(em);
        assert!(out.contains("persistent dangling entry"), "{out}");
        assert!(out.contains(&dangling.display().to_string()), "{out}");
        assert!(!out.contains("diverges from"), "{out}");
    }

    #[test]
    fn three_way_mismatch_routes_to_churn() {
        // Benign comparison takes a snapshot; the tracked dir changes
        // before settling; the three-way check refuses the replayable
        // key and names the window.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        write_queries(&manifest.join(".sqlx"), "v1");
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let verdict = divergent_fallthrough(&mut em, &tracked, &manifest);
        let FallthroughVerdict::Benign(Some(prior)) = verdict else {
            panic!("expected Benign(Some), got {verdict:?}");
        };
        assert!(matches!(prior, DirHash::Hash(_)), "{prior:?}");
        fs::write(tracked.join("query-aa.json"), b"{\"q\": \"mutated\"}").unwrap();
        let value = settled_hash_against(&mut em, &tracked, is_sqlx_query_file, Some(&prior));
        assert!(value.starts_with("churn-"), "{value}");
        let out = output(em);
        assert!(
            out.contains("changed between the fallthrough comparison and settling"),
            "{out}"
        );
    }

    #[test]
    fn settled_with_agreeing_prior_matches_plain() {
        // Three-way agreement is the no-op case: same hash as the plain
        // settled path, no directives emitted.
        let dir = setup();
        let h = sqlx_hash(dir.path());
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let against = settled_hash_against(
            &mut em,
            dir.path(),
            is_sqlx_query_file,
            Some(&DirHash::Hash(h.clone())),
        );
        assert_eq!(against, h);
        assert!(em.out.is_empty(), "agreement emits no directives");
    }

    #[test]
    fn tracked_value_settles_when_benign() {
        // End-to-end Track arm, healthy identical-twin shape: the
        // comparison snapshot agrees with both settled passes and the
        // replayable hash is emitted with zero warnings.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        write_queries(&manifest.join(".sqlx"), "v1");
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let value = tracked_value(&mut em, &tracked, &manifest);
        assert_eq!(value, sqlx_hash(&tracked));
        let out = output(em);
        assert!(!out.contains("cargo:warning"), "{out}");
    }

    #[test]
    fn tracked_value_divergent_keeps_divergence_wording() {
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        let twin = manifest.join(".sqlx");
        write_queries(&twin, "v2");
        let flag = AtomicBool::new(false);
        let mut em = capture(&flag);
        let value = tracked_value(&mut em, &tracked, &manifest);
        assert!(value.starts_with("unkeyed-"), "{value}");
        let out = output(em);
        assert!(
            out.contains("diverges from the fallthrough cache at"),
            "{out}"
        );
        // Names both paths.
        assert!(out.contains(&tracked.display().to_string()), "{out}");
        assert!(
            out.contains(&twin.canonicalize().unwrap().display().to_string()),
            "{out}"
        );
    }

    #[test]
    fn resolution_matrix() {
        assert!(matches!(sqlx_resolution(None), SqlxResolution::Untracked));
        assert!(matches!(
            sqlx_resolution(Some(PathBuf::new())),
            SqlxResolution::EmptyValue
        ));
        assert!(matches!(
            sqlx_resolution(Some(PathBuf::from("relative/.sqlx"))),
            SqlxResolution::NonAbsolute(_)
        ));
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            sqlx_resolution(Some(dir.path().to_path_buf())),
            SqlxResolution::Track(_)
        ));
        assert!(matches!(
            sqlx_resolution(Some(dir.path().join("missing"))),
            SqlxResolution::Absent(_)
        ));
        // An existing NON-directory is Absent too (macros fall through).
        let file = dir.path().join("a-file");
        fs::write(&file, b"x").unwrap();
        assert!(matches!(
            sqlx_resolution(Some(file)),
            SqlxResolution::Absent(_)
        ));
    }
}
