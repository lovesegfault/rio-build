//! Process-global test sandbox: temp-dir cwd + environment isolation.
//!
//! In-repo replacement for `figment::Jail` (the only piece of figment the
//! workspace still needed once the config loader moved to the `config`
//! crate). All env/cwd-mutating tests in one process serialize behind a
//! single global mutex; the environment and cwd are snapshotted on entry
//! and restored on drop (including on panic).
//!
//! The lock makes jailed tests mutually exclusive, but it cannot exclude
//! code that touches the environment without going through a `Jail`.
//! Consumer crates must therefore route ALL env mutation in their tests
//! through this type (or accept that such tests are only sound under
//! nextest's process-per-test model, and say so where they do it).
//! Under nextest the mutex is effectively redundant; under plain
//! `cargo test` (one process, many threads) it is the only thing
//! serializing jailed tests against each other.
//!
//! Nested `Jail::expect_with` calls deadlock on the non-reentrant global
//! lock — don't nest jails.
//!
//! Pre-existing environment variables remain visible inside the
//! jail — same as figment::Jail — only *changes* are rolled back.
//!
//! NOTE for rio-common: rio-common cannot dev-depend on this crate
//! (rio-test-support's `full` feature → rio-proto → rio-common would be a
//! dev-dependency cycle), so it carries a private trimmed copy at
//! `rio-common/src/test_jail.rs`. Keep behavioral changes in sync.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Global lock serializing all jailed tests within one process.
static JAIL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// A sandbox holding: the global jail lock, a temp dir used as cwd, and
/// snapshots of the environment + cwd taken at entry.
pub struct Jail {
    _guard: MutexGuard<'static, ()>,
    dir: tempfile::TempDir,
    saved_cwd: PathBuf,
    saved_env: HashMap<OsString, OsString>,
}

impl Jail {
    /// Run `f` inside a fresh jail; panic if it returns `Err`.
    ///
    /// Mirrors `figment::Jail::expect_with`, with `anyhow::Result` in place
    /// of the 208-byte `figment::Error` (so the old
    /// `#[allow(clippy::result_large_err)]` annotations can go away).
    pub fn expect_with<F>(f: F)
    where
        F: FnOnce(&mut Jail) -> anyhow::Result<()>,
    {
        let guard = match JAIL_LOCK.get_or_init(|| Mutex::new(())).lock() {
            Ok(g) => g,
            // A jailed test that panicked poisons the lock, but its Drop
            // already restored env+cwd, so continuing is sound.
            Err(poisoned) => poisoned.into_inner(),
        };
        let dir = tempfile::TempDir::new().expect("jail: create temp dir");
        let saved_cwd = std::env::current_dir().expect("jail: read current dir");
        let saved_env: HashMap<OsString, OsString> = std::env::vars_os().collect();
        std::env::set_current_dir(dir.path()).expect("jail: enter temp dir");
        let mut jail = Jail {
            _guard: guard,
            dir,
            saved_cwd,
            saved_env,
        };
        let result = f(&mut jail);
        drop(jail); // restore env + cwd BEFORE the panic below, so failures don't leak state
        result.expect("jail closure returned Err");
    }

    /// Set an environment variable for the duration of the jail.
    pub fn set_env<K: AsRef<str>, V: std::fmt::Display>(&mut self, key: K, value: V) {
        // SAFETY: JAIL_LOCK is held for the jail's whole lifetime, so no other
        // *jailed* test mutates the environment concurrently. Soundness further
        // requires that consumer crates route all test env mutation through
        // Jail (or run under nextest's process-per-test isolation) — see the
        // module docs.
        unsafe { std::env::set_var(key.as_ref(), value.to_string()) };
    }

    /// Create a file (and any parent directories) inside the jail
    /// directory, which is also the cwd while jailed.
    ///
    /// The path must be relative and must not contain `..` — files cannot
    /// be created outside the jail directory (figment::Jail parity).
    pub fn create_file<P: AsRef<Path>>(&self, path: P, contents: &str) -> anyhow::Result<PathBuf> {
        let rel = path.as_ref();
        anyhow::ensure!(
            rel.is_relative(),
            "jail: create_file path must be relative (got {})",
            rel.display()
        );
        anyhow::ensure!(
            !rel.components()
                .any(|c| matches!(c, std::path::Component::ParentDir)),
            "jail: create_file path must not contain '..' (got {})",
            rel.display()
        );
        let path = self.dir.path().join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut f = fs::File::create(&path)?;
        f.write_all(contents.as_bytes())?;
        f.sync_all()?;
        Ok(path)
    }

    /// The jail's temporary directory (== cwd while jailed).
    pub fn directory(&self) -> &Path {
        self.dir.path()
    }
}

impl Drop for Jail {
    fn drop(&mut self) {
        // Leave the temp dir before it gets deleted.
        if let Err(e) = std::env::set_current_dir(&self.saved_cwd) {
            eprintln!(
                "jail: failed to restore cwd to {}: {e}",
                self.saved_cwd.display()
            );
        }
        // Remove vars that were added, restore vars that were changed.
        let current: Vec<OsString> = std::env::vars_os().map(|(k, _)| k).collect();
        for k in current {
            if !self.saved_env.contains_key(&k) {
                // SAFETY: see set_env.
                unsafe { std::env::remove_var(&k) };
            }
        }
        for (k, v) in &self.saved_env {
            // SAFETY: see set_env.
            unsafe { std::env::set_var(k, v) };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Jail;

    #[test]
    fn env_set_inside_is_restored_after() {
        const KEY: &str = "RIO_JAIL_SELFTEST_RESTORE";
        assert!(std::env::var(KEY).is_err(), "var must not pre-exist");
        Jail::expect_with(|jail| {
            jail.set_env(KEY, "inside");
            assert_eq!(std::env::var(KEY).unwrap(), "inside");
            Ok(())
        });
        assert!(
            std::env::var(KEY).is_err(),
            "set_env must be undone when the jail drops"
        );
    }

    #[test]
    fn cwd_is_a_temp_dir_inside_and_restored_after() {
        let before = std::env::current_dir().unwrap();
        Jail::expect_with(|jail| {
            let inside = std::env::current_dir().unwrap();
            assert_ne!(inside, before, "jail must chdir away from the original cwd");
            assert_eq!(
                inside.canonicalize().unwrap(),
                jail.directory().canonicalize().unwrap(),
                "cwd inside the jail is the jail directory"
            );
            Ok(())
        });
        assert_eq!(std::env::current_dir().unwrap(), before, "cwd restored");
    }

    #[test]
    fn create_file_lands_in_jail_cwd() {
        Jail::expect_with(|jail| {
            jail.create_file("sub/dir/test.toml", "answer = 42")?;
            let read = std::fs::read_to_string("sub/dir/test.toml")?;
            assert_eq!(read, "answer = 42");
            Ok(())
        });
    }

    #[test]
    fn panic_inside_jail_still_restores_env_and_cwd() {
        const KEY: &str = "RIO_JAIL_SELFTEST_PANIC";
        let before = std::env::current_dir().unwrap();
        let result = std::panic::catch_unwind(|| {
            Jail::expect_with(|jail| {
                jail.set_env(KEY, "leaky");
                panic!("boom");
            });
        });
        assert!(result.is_err(), "the panic must propagate");
        assert!(std::env::var(KEY).is_err(), "env restored on panic");
        assert_eq!(
            std::env::current_dir().unwrap(),
            before,
            "cwd restored on panic"
        );
    }

    #[test]
    fn err_from_closure_panics() {
        let result = std::panic::catch_unwind(|| {
            Jail::expect_with(|_jail| anyhow::bail!("deliberate failure"));
        });
        assert!(
            result.is_err(),
            "Err from the closure must panic (figment::Jail parity)"
        );
    }

    #[test]
    fn create_file_rejects_paths_escaping_the_jail() {
        Jail::expect_with(|jail| {
            let abs = jail.create_file("/tmp/rio-jail-selftest-absolute.toml", "nope");
            assert!(abs.is_err(), "absolute paths must be rejected");
            assert!(
                !std::path::Path::new("/tmp/rio-jail-selftest-absolute.toml").exists(),
                "absolute-path file must not be created"
            );

            let up = jail.create_file("../escape.toml", "nope");
            assert!(up.is_err(), "'..' components must be rejected");
            assert!(
                !jail
                    .directory()
                    .parent()
                    .unwrap()
                    .join("escape.toml")
                    .exists(),
                "'..' file must not be created"
            );
            Ok(())
        });
    }

    #[test]
    fn pre_existing_env_var_is_restored_to_original_value() {
        const KEY: &str = "RIO_JAIL_SELFTEST_OVERWRITE";
        // SAFETY: this test module runs under nextest's process-per-test
        // isolation, so mutating the environment outside a jail here cannot
        // race another thread.
        unsafe { std::env::set_var(KEY, "original") };
        Jail::expect_with(|jail| {
            jail.set_env(KEY, "overwritten");
            assert_eq!(std::env::var(KEY).unwrap(), "overwritten");
            Ok(())
        });
        assert_eq!(
            std::env::var(KEY).unwrap(),
            "original",
            "pre-existing var must be restored to its original value"
        );
        // SAFETY: see above — process-per-test isolation.
        unsafe { std::env::remove_var(KEY) };
    }
}
