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
//! sqlx-macros-core 0.9 itself checks *first* in its own chain — so the
//! hash and the macros can never disagree about which directory is in
//! play. Every supported build context sets it explicitly:
//!
//! - the dev shell exports `<worktree>/.sqlx` (nix/devshell.nix shellHook,
//!   gated on a rio checkout marker);
//! - crate2nix sandbox builds set it as a derivation env var pointing at
//!   the `.sqlx` fileset (nix/crate2nix.nix `sqlxOffline`);
//! - the pre-commit `sqlx-prepare-check` re-pins it from its own
//!   toplevel, so a shell entered in another worktree cannot make the
//!   hook validate against the wrong cache.
//!
//! Degradations are loud and pin sentinels (constants — they deliberately
//! make the tracker inert rather than wrong): unset → `untracked`
//! (bare cargo outside supported contexts — plain cargo's pre-existing
//! blind spot); set-but-empty → `untracked`; relative → `untracked`
//! (refused: the build script would resolve it against the package dir
//! while sqlx-macros resolve it inside rustc against the workspace root —
//! two different directories); set-but-missing → `absent` (the macros own
//! the real diagnostic).
//!
//! The `absent` arm keeps the BUILD SCRIPT from panicking when `.sqlx` is
//! deleted, but note the macros themselves still fail without a cache:
//! the hash flip to `absent` re-keys the consumers, forcing recompiles
//! whose `query!` has nothing to read — so after `rm -rf .sqlx`, even
//! `cargo xtask regen sqlx` cannot build (xtask prod-depends on
//! rio-scheduler). Recover with `git checkout -- .sqlx` (the cache is
//! committed) or run an already-built `target/debug/xtask` directly.
//!
//! Hashing covers exactly the macro-visible sets — top-level
//! `query-*.json` for the offline cache, top-level `<digits>_*.sql` for
//! migrations (`sqlx::migrate!`'s filename grammar) — so editor swap
//! files and stray artifacts never churn the key. Concurrency: a file
//! vanishing mid-read is skipped, a directory vanishing mid-hash degrades
//! to `absent`, and the hash is computed twice — if the listing churns
//! between passes (a cache rewrite racing this script), the build is
//! keyed with a unique value so it can never be replayed and the next
//! build re-hashes the settled state. Residual race, by construction
//! unobservable from a build script: the macros re-read the directory
//! later, inside rustc — a swap landing in *that* window mislabels one
//! compile. Exposure requires an exit-0 compile against a half-rewritten
//! cache; `regen sqlx` itself runs kache-disabled, so the racing writer
//! is typically a background checker (rust-analyzer) whose artifacts the
//! next settled build re-keys away locally.
//!
//! The hash is FNV-1a 64: dependency-free and deterministic across
//! platforms, runs, and toolchains. It only needs to *change* when content
//! changes; it is not a security boundary.

use std::path::{Path, PathBuf};

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

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
        SqlxResolution::Untracked => {
            println!(
                "cargo:warning=rio-buildhash: SQLX_OFFLINE_DIR unset — .sqlx/ changes will \
                 not re-key this crate (build inside the dev shell or a nix sandbox)"
            );
            "untracked".to_string()
        }
        SqlxResolution::EmptyValue => {
            println!(
                "cargo:warning=rio-buildhash: SQLX_OFFLINE_DIR is set but empty — the sqlx \
                 macros will fall through to their own discovery while this hash stays \
                 pinned; unset it or point it at the real .sqlx"
            );
            "untracked".to_string()
        }
        SqlxResolution::NonAbsolute(dir) => {
            println!(
                "cargo:warning=rio-buildhash: refusing relative SQLX_OFFLINE_DIR ({}) — the \
                 build script resolves it against the package dir but sqlx-macros resolve \
                 it inside rustc against the workspace root; set an absolute path",
                dir.display()
            );
            "untracked".to_string()
        }
        SqlxResolution::Absent(dir) => {
            // Watch the missing path. Cargo treats a nonexistent watched
            // path as always-stale, so this script re-runs (and re-warns)
            // on EVERY build until the directory exists — deliberate:
            // the state is broken-but-recoverable and should stay loud.
            println!("cargo:rerun-if-changed={}", dir.display());
            println!(
                "cargo:warning=rio-buildhash: SQLX_OFFLINE_DIR={} does not exist — \
                 offline query expansion will fail if this crate needs it \
                 (recover with: git checkout -- .sqlx)",
                dir.display()
            );
            "absent".to_string()
        }
        SqlxResolution::Track(dir) => {
            println!("cargo:rerun-if-changed={}", dir.display());
            settled_hash(&dir, is_sqlx_query_file)
        }
    };
    println!("cargo:rustc-env=RIO_SQLX_HASH={value}");
}

/// Track the crate's `migrations/` directory (top-level `<digits>_*.sql`,
/// the set `sqlx::migrate!` accepts) as the `RIO_MIGRATIONS_HASH` env-dep.
///
/// The directory is part of the crate (committed); its absence is a broken
/// checkout, so this one does panic.
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
    /// Variable unset — unsupported context, pin a sentinel.
    Untracked,
    /// Variable set but empty — sqlx falls through; refuse to guess.
    EmptyValue,
    /// Variable set but relative — the two readers would resolve it
    /// against different working directories; refuse.
    NonAbsolute(PathBuf),
    /// Variable set, absolute, but the directory does not exist.
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

fn is_sqlx_query_file(name: &str) -> bool {
    name.starts_with("query-") && name.ends_with(".json")
}

/// `sqlx::migrate!` filename grammar: `<version>_<description>.sql`
/// (including the `.up.sql`/`.down.sql` reversible forms, which still end
/// in `.sql`). Anything else — `scratch.sql~`, editor swap files, README —
/// is silently skipped by the macro and must not churn the hash.
fn is_migration_file(name: &str) -> bool {
    let Some(rest) = name.strip_suffix(".sql") else {
        return false;
    };
    let Some((version, _description)) = rest.split_once('_') else {
        return false;
    };
    !version.is_empty() && version.bytes().all(|b| b.is_ascii_digit())
}

/// Hash the matching set twice; if the listing or contents churn between
/// passes (a cache rewrite racing this build script), key this build with
/// a unique value: it can never be replayed from a wrapper cache, and the
/// changed env value re-runs the script on the next build, which hashes
/// the settled state.
fn settled_hash(dir: &Path, pred: fn(&str) -> bool) -> String {
    let first = hash_matching_files(dir, pred);
    let second = hash_matching_files(dir, pred);
    match (first, second) {
        (Some(a), Some(b)) if a == b => a,
        (None, None) => "absent".to_string(),
        _ => {
            println!(
                "cargo:warning=rio-buildhash: {} changed while hashing — keying this \
                 build uniquely; the next build re-hashes the settled state",
                dir.display()
            );
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            format!("churn-{nanos}")
        }
    }
}

/// Content hash of the top-level files in `dir` whose names match `pred`:
/// names + contents, independent of mtimes, ownership, listing order, and
/// non-matching files. `None` when the directory itself has vanished
/// (degrade, don't panic — the same race tolerance as the per-file skip).
/// Per-entry iterator errors PANIC: silently dropping one would emit a
/// plausible-looking truncated hash that could alias a legitimately
/// smaller set and replay a stale artifact with zero diagnostics.
fn hash_matching_files(dir: &Path, pred: fn(&str) -> bool) -> Option<String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return None,
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
            // TOCTOU: deleted (or a dangling symlink) between listing and
            // read — e.g. `cargo sqlx prepare` swapping the cache while a
            // parallel build script runs. Skip; the settled-hash double
            // pass and rerun-if-changed re-key once the churn settles.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => panic!("rio-buildhash: read({}) failed: {e}", path.display()),
        };
        state = fnv1a(state, name.as_bytes());
        // Separator that cannot appear in UTF-8 name bytes, so
        // ("ab", "c") never collides with ("a", "bc").
        state = fnv1a(state, &[0xFF]);
        state = fnv1a(state, &(contents.len() as u64).to_le_bytes());
        state = fnv1a(state, &contents);
    }
    Some(format!("{state:016x}"))
}

#[cfg(test)]
mod tests {
    use super::{
        SqlxResolution, hash_matching_files, is_migration_file, is_sqlx_query_file, settled_hash,
        sqlx_resolution,
    };
    use std::fs;
    use std::path::PathBuf;

    fn sqlx_hash(dir: &std::path::Path) -> String {
        hash_matching_files(dir, is_sqlx_query_file).expect("dir exists")
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
        // Rewrite identical contents (bumps mtime — the thing sqlx-cli
        // manipulates and content keys must ignore).
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
        // None of these are read by sqlx-macros: editor swap/backup
        // files, docs, nested dirs (the offline cache is flat).
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
        // Skipped by sqlx::migrate! — must not churn the hash.
        assert!(!is_migration_file("scratch.sql"));
        assert!(!is_migration_file("001_init.sql~"));
        assert!(!is_migration_file("_no_version.sql"));
        assert!(!is_migration_file("a1_bad.sql"));
        assert!(!is_migration_file("README.md"));
    }

    #[test]
    fn vanished_file_is_skipped_not_fatal() {
        // Deterministic stand-in for the TOCTOU race: a dangling symlink
        // passes the listing but fails the read with NotFound.
        let dir = setup();
        std::os::unix::fs::symlink("nonexistent", dir.path().join("query-dd.json")).unwrap();
        let with_dangling = sqlx_hash(dir.path());
        fs::remove_file(dir.path().join("query-dd.json")).unwrap();
        assert_eq!(with_dangling, sqlx_hash(dir.path()));
    }

    #[test]
    fn vanished_dir_degrades_to_none() {
        let dir = tempfile::tempdir().unwrap();
        let gone = dir.path().join("gone");
        fs::create_dir(&gone).unwrap();
        fs::remove_dir(&gone).unwrap();
        assert_eq!(hash_matching_files(&gone, is_sqlx_query_file), None);
        assert_eq!(settled_hash(&gone, is_sqlx_query_file), "absent");
    }

    #[test]
    fn name_content_boundary_no_collision() {
        let d1 = tempfile::tempdir().unwrap();
        fs::write(d1.path().join("query-ab.json"), b"c").unwrap();
        let d2 = tempfile::tempdir().unwrap();
        // Same concatenated bytes, different (name, content) split.
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
    }
}
