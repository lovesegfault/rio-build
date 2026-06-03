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
//! nothing they produce can ever be replayed from a wrapper cache. That
//! cost is deliberate — supported contexts always set the variable, and a
//! cache key must never claim to cover an input it cannot observe.
//!
//! The same per-query fallthrough cuts the other way in the Track arm:
//! resolving `SQLX_OFFLINE_DIR` to *a* directory does not make it the
//! *only* directory the macros can read. A replayable settled hash is
//! emitted only when the tracked dir is provably the only cache the
//! macros can read — when a fallthrough candidate (`<manifest>/.sqlx` or
//! the workspace root's `.sqlx`) exists with diverging macro-visible
//! content, the build is unkeyed exactly like the degraded arms (see
//! `divergent_fallthrough`).
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

use std::path::{Path, PathBuf};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

/// Never-existing path watched to force the build script to re-run on
/// every build (cargo treats a missing watched path as always-stale).
/// Relative to the build script's cwd (the package root). If this file is
/// ever actually created, always-rerun silently degrades to a plain mtime
/// watch — keep the name obscure and do not create it.
const ALWAYS_RERUN_SENTINEL: &str = ".rio-buildhash-always-rerun";

fn fnv1a(state: u64, bytes: &[u8]) -> u64 {
    bytes.iter().fold(state, |acc, b| {
        (acc ^ u64::from(*b)).wrapping_mul(FNV_PRIME)
    })
}

/// Track the sqlx offline query cache as the `RIO_SQLX_HASH` env-dep.
///
/// Resolution is the `SQLX_OFFLINE_DIR` contract described in the module
/// docs — no discovery. The consuming crate MUST read the variable
/// (`const _: &str = env!("RIO_SQLX_HASH");`) or no env-dep is recorded
/// and the tracking silently does nothing.
pub fn track_sqlx() {
    println!("cargo:rerun-if-env-changed=SQLX_OFFLINE_DIR");
    let value = match sqlx_resolution(std::env::var_os("SQLX_OFFLINE_DIR").map(PathBuf::from)) {
        SqlxResolution::Untracked => unkeyed(
            "SQLX_OFFLINE_DIR unset — the sqlx macros may still find a cache via their own \
             discovery, so this build is keyed uniquely (uncacheable); build inside the dev \
             shell or a nix sandbox for keyed builds",
        ),
        SqlxResolution::EmptyValue => unkeyed(
            "SQLX_OFFLINE_DIR is set but empty — the sqlx macros fall through to their own \
             discovery; keying this build uniquely (uncacheable); unset it or point it at \
             the real .sqlx",
        ),
        SqlxResolution::NonAbsolute(dir) => unkeyed(&format!(
            "refusing relative SQLX_OFFLINE_DIR ({}) — the build script resolves it against \
             the package dir but sqlx-macros resolve it inside rustc against the workspace \
             root; keying this build uniquely (uncacheable); set an absolute path",
            dir.display()
        )),
        SqlxResolution::Absent(dir) => unkeyed(&format!(
            "SQLX_OFFLINE_DIR={} is not a directory — the sqlx macros fall through to their \
             own discovery and may still compile, so this build is keyed uniquely \
             (uncacheable); fix the variable (a dangling export from a removed worktree?) \
             or restore the cache (git checkout -- .sqlx)",
            dir.display()
        )),
        SqlxResolution::Track(dir) => {
            println!("cargo:rerun-if-changed={}", dir.display());
            // Package root, the same assumption track_migrations relies
            // on for its cwd fallback: cargo and buildRustCrate both set
            // CARGO_MANIFEST_DIR and run the script from the package dir.
            let manifest_dir = std::env::var_os("CARGO_MANIFEST_DIR")
                .map(PathBuf::from)
                .or_else(|| std::env::current_dir().ok())
                .unwrap_or_default();
            match divergent_fallthrough(&dir, &manifest_dir) {
                Some(twin) => unkeyed(&format!(
                    "SQLX_OFFLINE_DIR={} diverges from the fallthrough cache at {} — \
                     sqlx-macros resolve each query-<hash>.json independently \
                     (find-first-existing), so a json missing under the tracked dir \
                     silently loads from the fallthrough copy this hash never saw; \
                     keying this build uniquely (uncacheable); re-point \
                     SQLX_OFFLINE_DIR at the cache you mean, or remove the divergent \
                     copy",
                    dir.display(),
                    twin.display()
                )),
                None => settled_hash(&dir, is_sqlx_query_file),
            }
        }
    };
    println!("cargo:rustc-env=RIO_SQLX_HASH={value}");
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
    println!("cargo:rerun-if-changed={}", dir.display());
    println!(
        "cargo:rustc-env=RIO_MIGRATIONS_HASH={}",
        settled_hash(dir, is_migration_file)
    );
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

/// A fallthrough `.sqlx` candidate that exists, is not the tracked dir,
/// and whose macro-visible content differs from it — `Some(path)` means
/// the Track arm must NOT emit a replayable key.
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
/// Per candidate: canonicalize NotFound => skip (an absent dir serves no
/// file — sqlx's `exists()` runs on the joined FILE path);
/// canonicalize-equal to the tracked dir => skip (it IS the tracked dir,
/// the devshell shape; canonical comparison is robust to symlinks);
/// non-directory => skip (nothing joins under it); any other probe
/// error => divergent (never claim replayability on a partial
/// observation). A surviving foreign candidate is watched with
/// `rerun-if-changed` (a later edit to it must re-run this script —
/// cargo re-runs only on watched-path/env changes, never on emitted env
/// VALUES) and compared by macro-visible content hash: both passes
/// `Hash` and equal => benign (identical bytes cannot leak anything the
/// tracked hash does not cover); anything else => divergent. Whole-set
/// equality deliberately over-fires when the divergence is confined to
/// files the tracked dir also contains (per-query masking means those
/// bytes cannot leak) — precision there would only buy cacheability
/// inside an already-misconfigured state, and the over-fire is loud and
/// self-heals once the twin is removed.
///
/// Returns `None` when `tracked` itself fails to canonicalize (vanished
/// after the `is_dir` check) — `settled_hash`'s double-pass then reports
/// the vanish with the churn wording.
///
/// Accepted residual: a twin CREATED after a settled compile is unseen
/// until a watched input changes — watching a not-yet-existing path IS
/// the always-stale primitive (`ALWAYS_RERUN_SENTINEL`) and would defeat
/// caching entirely. Same residual class as the in-rustc re-read window
/// in the module docs; the motivating stale-sibling case has the twin
/// existing at script time and is caught.
fn divergent_fallthrough(tracked: &Path, manifest_dir: &Path) -> Option<PathBuf> {
    let tracked_canon = match tracked.canonicalize() {
        Ok(canon) => canon,
        Err(_) => return None,
    };
    let candidates = [
        manifest_dir.join(".sqlx"),
        manifest_dir.join("..").join(".sqlx"),
    ];
    for candidate in candidates {
        let canon = match candidate.canonicalize() {
            Ok(canon) => canon,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return Some(candidate),
        };
        if canon == tracked_canon || !canon.is_dir() {
            continue;
        }
        println!("cargo:rerun-if-changed={}", canon.display());
        let tracked_hash = hash_matching_files(tracked, is_sqlx_query_file);
        let candidate_hash = hash_matching_files(&canon, is_sqlx_query_file);
        match (tracked_hash, candidate_hash) {
            (DirHash::Hash(a), DirHash::Hash(b)) if a == b => {}
            _ => return Some(canon),
        }
    }
    None
}

/// Degraded-state emission: warn, force a re-run on every build, and key
/// this build with a value nothing can ever replay. See the module docs
/// for why degraded states must be unkeyed rather than pinned to a
/// constant sentinel.
fn unkeyed(why: &str) -> String {
    println!("cargo:warning=rio-buildhash: {why}");
    emit_always_rerun();
    unique_value("unkeyed")
}

/// Watch a never-existing path: cargo re-runs the script on EVERY build
/// (verified primitive — `stale: missing <path>` in cargo's fingerprint
/// log). Emitting a different `rustc-env` value alone does NOT re-run a
/// script, so this is the only way a degraded/churned state can heal on
/// the next build.
fn emit_always_rerun() {
    println!("cargo:rerun-if-changed={ALWAYS_RERUN_SENTINEL}");
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

/// Hash the matching set twice; any disagreement, vanish, or partial read
/// is churn: warn, force a re-run next build, and key this build uniquely
/// (it can never be replayed; the next build re-hashes the settled state
/// because of the always-stale watch — NOT because the env value changed,
/// which cargo ignores).
fn settled_hash(dir: &Path, pred: fn(&str) -> bool) -> String {
    let first = hash_matching_files(dir, pred);
    let second = hash_matching_files(dir, pred);
    match (first, second) {
        (DirHash::Hash(a), DirHash::Hash(b)) if a == b => a,
        (first, second) => {
            println!(
                "cargo:warning=rio-buildhash: {}",
                churn_warning(dir, &first, &second)
            );
            emit_always_rerun();
            unique_value("churn")
        }
    }
}

/// Diagnostic for a double-pass disagreement. BOTH passes failing on the
/// SAME listed-but-unreadable path is not transient churn: only
/// `ErrorKind::NotFound` reaches `FileVanished` (other read errors panic
/// in `hash_matching_files`, so "permission denied" is never this arm),
/// and a NotFound that holds across two passes is a persistent dangling
/// entry — typically a broken symlink. The transient wording's "the next
/// build re-hashes the settled state" would be a misdiagnosis there:
/// nothing settles until the entry is removed. Every other combination
/// keeps the transient-churn wording.
fn churn_warning(dir: &Path, first: &DirHash, second: &DirHash) -> String {
    match (first, second) {
        (DirHash::FileVanished(a), DirHash::FileVanished(b)) if a == b => format!(
            "{} is listed but absent on read in both hashing passes — a persistent \
             dangling entry (broken symlink?), not transient churn; keying this build \
             uniquely (uncacheable) on every build until the entry is removed",
            a.display()
        ),
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
/// corresponding variant (the caller treats it as churn — a hash over a
/// partially-read set would alias the legitimate hash of a smaller set and
/// replay a stale artifact with zero diagnostics). Per-entry iterator
/// errors and non-NotFound read errors PANIC: they are environmental
/// failures, not races, and must stay loud.
fn hash_matching_files(dir: &Path, pred: fn(&str) -> bool) -> DirHash {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DirHash::DirVanished,
        Err(e) => panic!("rio-buildhash: read_dir({}) failed: {e}", dir.display()),
    };
    let mut names: Vec<String> = entries
        .filter_map(|entry| {
            let entry = entry.unwrap_or_else(|e| {
                panic!(
                    "rio-buildhash: dir entry under {} failed: {e}",
                    dir.display()
                )
            });
            let name = entry.file_name().into_string().ok()?;
            (pred(&name) && !entry.path().is_dir()).then_some(name)
        })
        .collect();
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
                return DirHash::FileVanished(path);
            }
            Err(e) => panic!("rio-buildhash: read({}) failed: {e}", path.display()),
        };
        state = fnv1a(state, name.as_bytes());
        // Separator that cannot appear in UTF-8 name bytes, so
        // ("ab", "c") never collides with ("a", "bc").
        state = fnv1a(state, &[0xFF]);
        state = fnv1a(state, &(contents.len() as u64).to_le_bytes());
        state = fnv1a(state, &contents);
    }
    DirHash::Hash(format!("{state:016x}"))
}

#[cfg(test)]
mod tests {
    use super::{
        DirHash, SqlxResolution, churn_warning, divergent_fallthrough, hash_matching_files,
        is_migration_file, is_sqlx_query_file, settled_hash, sqlx_resolution, unique_value,
    };
    use std::fs;
    use std::path::PathBuf;

    fn sqlx_hash(dir: &std::path::Path) -> String {
        match hash_matching_files(dir, is_sqlx_query_file) {
            DirHash::Hash(h) => h,
            other => panic!("expected settled hash, got {other:?}"),
        }
    }

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("query-aa.json"), b"{\"q\": 1}").unwrap();
        fs::write(dir.path().join("query-bb.json"), b"{\"q\": 2}").unwrap();
        dir
    }

    #[test]
    fn stable_across_calls() {
        let dir = setup();
        assert_eq!(sqlx_hash(dir.path()), sqlx_hash(dir.path()));
        // And the settled (double-pass) wrapper agrees on a quiet dir.
        assert_eq!(
            settled_hash(dir.path(), is_sqlx_query_file),
            sqlx_hash(dir.path())
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
        let churned = settled_hash(dir.path(), is_sqlx_query_file);
        assert!(churned.starts_with("churn-"), "{churned}");
        // And it is unique per call, never replayable.
        assert_ne!(churned, settled_hash(dir.path(), is_sqlx_query_file));
    }

    #[test]
    fn churn_warning_same_path_double_vanish_is_persistent() {
        let dir = PathBuf::from("/cache/.sqlx");
        let gone = dir.join("query-dd.json");
        let msg = churn_warning(
            &dir,
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
            let msg = churn_warning(&dir, &first, &second);
            assert!(msg.contains("changed while hashing"), "{msg}");
            assert!(msg.contains("/cache/.sqlx"), "{msg}");
            assert!(!msg.contains("persistent"), "{msg}");
        }
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
        let churned = settled_hash(&gone, is_sqlx_query_file);
        assert!(churned.starts_with("churn-"), "{churned}");
    }

    #[test]
    fn unique_values_never_repeat() {
        let a = unique_value("unkeyed");
        let b = unique_value("unkeyed");
        assert_ne!(a, b);
        assert!(a.starts_with("unkeyed-"));
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
    fn write_queries(dir: &std::path::Path, tag: &str) {
        fs::create_dir_all(dir).unwrap();
        fs::write(dir.join("query-aa.json"), format!("{{\"q\": \"{tag}-1\"}}")).unwrap();
        fs::write(dir.join("query-bb.json"), format!("{{\"q\": \"{tag}-2\"}}")).unwrap();
    }

    #[test]
    fn divergence_absent_candidates_benign() {
        // The crate2nix sandbox shape: neither <manifest>/.sqlx nor
        // <manifest>/../.sqlx exists — nothing to fall through to.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        assert_eq!(divergent_fallthrough(&tracked, &manifest), None);
    }

    #[test]
    fn divergence_manifest_dotsqlx_identical_benign() {
        // A byte-identical manifest/.sqlx serves nothing the tracked
        // hash does not already cover.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        write_queries(&manifest.join(".sqlx"), "v1");
        assert_eq!(divergent_fallthrough(&tracked, &manifest), None);
    }

    #[test]
    fn divergence_workspace_self_benign() {
        // The devshell no-op in unit form: tracked IS the workspace
        // root's .sqlx, reached via the manifest/../.sqlx probe.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join(".sqlx");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        assert_eq!(divergent_fallthrough(&tracked, &manifest), None);
    }

    #[test]
    fn divergence_symlinked_tracked_benign() {
        // Same-dir detection must survive symlinks: manifest/.sqlx is a
        // symlink TO the tracked dir, not a second cache.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("real-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        std::os::unix::fs::symlink(&tracked, manifest.join(".sqlx")).unwrap();
        assert_eq!(divergent_fallthrough(&tracked, &manifest), None);
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
        assert_eq!(
            divergent_fallthrough(&tracked, &manifest),
            Some(twin.canonicalize().unwrap())
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
        assert_eq!(
            divergent_fallthrough(&tracked, &manifest),
            Some(workspace_sqlx.canonicalize().unwrap())
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
        assert_eq!(divergent_fallthrough(&tracked, &manifest), None);
    }

    #[test]
    fn divergence_non_dir_candidate_benign() {
        // A regular FILE named .sqlx serves no query json (sqlx joins
        // filenames under it and exists() fails) — skip, don't hash it.
        let root = tempfile::tempdir().unwrap();
        let tracked = root.path().join("tracked-cache");
        write_queries(&tracked, "v1");
        let manifest = root.path().join("crate");
        fs::create_dir(&manifest).unwrap();
        fs::write(manifest.join(".sqlx"), b"not a directory").unwrap();
        assert_eq!(divergent_fallthrough(&tracked, &manifest), None);
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
