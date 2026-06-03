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
            settled_hash(&dir, is_sqlx_query_file)
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
    FileVanished,
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
        _ => {
            println!(
                "cargo:warning=rio-buildhash: {} changed while hashing — keying this \
                 build uniquely (uncacheable); the next build re-hashes the settled state",
                dir.display()
            );
            emit_always_rerun();
            unique_value("churn")
        }
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return DirHash::FileVanished,
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
        DirHash, SqlxResolution, hash_matching_files, is_migration_file, is_sqlx_query_file,
        settled_hash, sqlx_resolution, unique_value,
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
        // hash of the smaller set — it is churn, keyed uniquely.
        let dir = setup();
        std::os::unix::fs::symlink("nonexistent", dir.path().join("query-dd.json")).unwrap();
        assert_eq!(
            hash_matching_files(dir.path(), is_sqlx_query_file),
            DirHash::FileVanished
        );
        let churned = settled_hash(dir.path(), is_sqlx_query_file);
        assert!(churned.starts_with("churn-"), "{churned}");
        // And it is unique per call, never replayable.
        assert_ne!(churned, settled_hash(dir.path(), is_sqlx_query_file));
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
