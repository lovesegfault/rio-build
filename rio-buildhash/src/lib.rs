//! Build-script helper: surface out-of-band proc-macro inputs as tracked
//! rustc env-deps.
//!
//! Some proc macros read files that rustc's dep-info never sees on stable:
//! `sqlx::query!` reads `.sqlx/*.json` in offline mode, and
//! `sqlx::migrate!` embeds `migrations/*.sql` at expansion time. Cargo
//! papers over the gap with `cargo:rerun-if-changed`, which re-runs rustc —
//! but a content-keyed `RUSTC_WRAPPER` cache (kache; sccache-likes) then
//! sees byte-identical key inputs and restores the artifact compiled
//! against the *old* file contents: deterministic staleness that survives
//! `cargo clean`.
//!
//! The structural fix is to make the content reach rustc itself:
//!
//! 1. build.rs calls [`track_dir`] / [`track_dir_upwards`], which hashes
//!    the directory into `cargo:rustc-env=<NAME>=<hash>` (plus the usual
//!    `cargo:rerun-if-changed`);
//! 2. the consuming crate reads it with `const _: &str = env!("<NAME>");`.
//!
//! rustc records the `env!` read as a `# env-dep:` line in dep-info, which
//! is part of both cargo's fingerprint and any dep-info-derived wrapper
//! cache key. A change to the tracked directory therefore re-keys the
//! crate everywhere, by construction — and every dependent re-keys
//! transitively through the changed rlib hash.
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

/// Recursively collect all files under `dir`, as (relative-path, absolute)
/// pairs sorted by relative path — deterministic regardless of filesystem
/// iteration order.
fn collect_files(dir: &Path) -> Vec<(String, PathBuf)> {
    fn walk(root: &Path, current: &Path, out: &mut Vec<(String, PathBuf)>) {
        let entries = std::fs::read_dir(current).unwrap_or_else(|e| {
            panic!("rio-buildhash: read_dir({}) failed: {e}", current.display())
        });
        for entry in entries {
            let entry = entry.unwrap_or_else(|e| {
                panic!(
                    "rio-buildhash: dir entry under {} failed: {e}",
                    current.display()
                )
            });
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else {
                let rel = path
                    .strip_prefix(root)
                    .expect("walk stays under root")
                    .components()
                    .map(|c| {
                        c.as_os_str().to_str().unwrap_or_else(|| {
                            panic!(
                                "rio-buildhash: non-UTF-8 path component under {}",
                                root.display()
                            )
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("/");
                out.push((rel, path));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

/// Content hash of a directory tree: relative paths + file contents,
/// independent of mtimes, ownership, and traversal order.
pub fn dir_hash(dir: &Path) -> String {
    let mut state = FNV_OFFSET;
    for (rel, path) in collect_files(dir) {
        state = fnv1a(state, rel.as_bytes());
        // Separator that cannot appear in UTF-8 path bytes, so
        // ("ab", "c") never collides with ("a", "bc").
        state = fnv1a(state, &[0xFF]);
        let contents = std::fs::read(&path)
            .unwrap_or_else(|e| panic!("rio-buildhash: read({}) failed: {e}", path.display()));
        state = fnv1a(state, &(contents.len() as u64).to_le_bytes());
        state = fnv1a(state, &contents);
    }
    format!("{state:016x}")
}

/// Track `dir` (relative to the crate root, like all build.rs paths) as a
/// rustc env-dep named `env_name`.
///
/// Emits `cargo:rerun-if-changed` (so the build script re-runs on changes)
/// and `cargo:rustc-env` with the content hash (so rustc — and anything
/// keyed on rustc's dep-info — sees the change). The consuming crate MUST
/// read the variable, e.g. `const _: &str = env!("RIO_SQLX_HASH");`, or no
/// env-dep is recorded and the tracking silently does nothing.
pub fn track_dir(env_name: &str, dir: &Path) {
    assert!(
        dir.is_dir(),
        "rio-buildhash: {} is not a directory (cwd: {})",
        dir.display(),
        std::env::current_dir().unwrap_or_default().display(),
    );
    println!("cargo:rerun-if-changed={}", dir.display());
    println!("cargo:rustc-env={env_name}={}", dir_hash(dir));
}

/// Like [`track_dir`], but finds `dir_name` by walking up from
/// `CARGO_MANIFEST_DIR` (plain cargo worktree layouts).
pub fn track_dir_upwards(env_name: &str, dir_name: &str) {
    let manifest_dir = manifest_dir();
    let found = manifest_dir
        .ancestors()
        .map(|a| a.join(dir_name))
        .find(|c| c.is_dir())
        .unwrap_or_else(|| {
            panic!(
                "rio-buildhash: no {dir_name}/ found above {} — \
                 offline macro inputs missing from the source tree?",
                manifest_dir.display()
            )
        });
    track_dir(env_name, &found);
}

/// Like [`track_dir_upwards`], but mirrors sqlx-macros-core 0.9's own
/// discovery chain so it works in every layout this repo builds in:
///
/// 1. `$<override_var>` as a real environment variable;
/// 2. a `<override_var>=<path>` line in `$CARGO_MANIFEST_DIR/.env` —
///    crate2nix's buildRustCrate sandbox provides `.sqlx` this way
///    (see nix/crate2nix.nix `sqlxOffline`: per-crate sources have no
///    workspace root above them, so a `.env` file points at a separate
///    fileset store path);
/// 3. walking up from `CARGO_MANIFEST_DIR` (plain cargo worktrees).
pub fn track_dir_upwards_or_env(env_name: &str, override_var: &str, dir_name: &str) {
    println!("cargo:rerun-if-env-changed={override_var}");
    if let Some(dir) = std::env::var_os(override_var).map(PathBuf::from)
        && dir.is_dir()
    {
        track_dir(env_name, &dir);
        return;
    }
    let dotenv = manifest_dir().join(".env");
    if let Ok(contents) = std::fs::read_to_string(&dotenv)
        && let Some(dir) = dotenv_value(&contents, override_var)
        && dir.is_dir()
    {
        track_dir(env_name, &dir);
        return;
    }
    track_dir_upwards(env_name, dir_name);
}

/// First `VAR=value` line in dotenv-style `contents`, as a path.
fn dotenv_value(contents: &str, var: &str) -> Option<PathBuf> {
    let prefix = format!("{var}=");
    contents
        .lines()
        .filter_map(|l| l.strip_prefix(&prefix))
        .map(str::trim)
        .find(|v| !v.is_empty())
        .map(PathBuf::from)
}

fn manifest_dir() -> PathBuf {
    PathBuf::from(
        std::env::var_os("CARGO_MANIFEST_DIR")
            .expect("rio-buildhash: CARGO_MANIFEST_DIR unset (not run from a build script?)"),
    )
}

#[cfg(test)]
mod tests {
    use super::dir_hash;
    use std::fs;

    fn setup() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::write(dir.path().join("a.json"), b"{\"q\": 1}").unwrap();
        fs::create_dir(dir.path().join("sub")).unwrap();
        fs::write(dir.path().join("sub/b.sql"), b"SELECT 1;").unwrap();
        dir
    }

    #[test]
    fn stable_across_calls() {
        let dir = setup();
        assert_eq!(dir_hash(dir.path()), dir_hash(dir.path()));
    }

    #[test]
    fn mtime_does_not_matter() {
        let dir = setup();
        let before = dir_hash(dir.path());
        // Rewrite identical contents (bumps mtime — the thing sqlx-cli
        // manipulates and content keys must ignore).
        fs::write(dir.path().join("a.json"), b"{\"q\": 1}").unwrap();
        assert_eq!(before, dir_hash(dir.path()));
    }

    #[test]
    fn content_change_changes_hash() {
        let dir = setup();
        let before = dir_hash(dir.path());
        fs::write(dir.path().join("a.json"), b"{\"q\": 2}").unwrap();
        assert_ne!(before, dir_hash(dir.path()));
    }

    #[test]
    fn file_add_and_remove_change_hash() {
        let dir = setup();
        let before = dir_hash(dir.path());
        fs::write(dir.path().join("c.json"), b"{}").unwrap();
        let added = dir_hash(dir.path());
        assert_ne!(before, added);
        fs::remove_file(dir.path().join("c.json")).unwrap();
        assert_eq!(before, dir_hash(dir.path()));
    }

    #[test]
    fn rename_changes_hash() {
        let dir = setup();
        let before = dir_hash(dir.path());
        fs::rename(dir.path().join("a.json"), dir.path().join("z.json")).unwrap();
        assert_ne!(before, dir_hash(dir.path()));
    }

    #[test]
    fn dotenv_value_parses_first_match() {
        let contents = "OTHER=x\nSQLX_OFFLINE_DIR=/nix/store/abc/.sqlx\nSQLX_OFFLINE_DIR=/dup\n";
        assert_eq!(
            super::dotenv_value(contents, "SQLX_OFFLINE_DIR"),
            Some(std::path::PathBuf::from("/nix/store/abc/.sqlx"))
        );
        assert_eq!(super::dotenv_value(contents, "MISSING"), None);
        assert_eq!(
            super::dotenv_value("SQLX_OFFLINE_DIR=\n", "SQLX_OFFLINE_DIR"),
            None
        );
    }

    #[test]
    fn path_content_boundary_no_collision() {
        // ("ab" containing "c") vs ("a" containing "bc") must differ.
        let d1 = tempfile::tempdir().unwrap();
        fs::write(d1.path().join("ab"), b"c").unwrap();
        let d2 = tempfile::tempdir().unwrap();
        fs::write(d2.path().join("a"), b"bc").unwrap();
        assert_ne!(dir_hash(d1.path()), dir_hash(d2.path()));
    }
}
