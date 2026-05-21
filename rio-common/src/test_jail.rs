//! Private test-only env/cwd sandbox for rio-common's own tests.
//!
//! This is a trimmed copy of `rio_test_support::Jail` (no `create_file`).
//! It exists because rio-common cannot dev-depend on rio-test-support:
//! rio-test-support's `full` feature pulls rio-proto, which depends on
//! rio-common — a dev-dependency cycle. Keep behavioral changes in sync
//! with rio-test-support/src/jail.rs (including its locking caveats: the
//! lock only excludes other jailed tests; all env mutation in this
//! crate's tests must go through this type, or be sound only under
//! nextest's process-per-test isolation).

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard, OnceLock};

/// Global lock serializing all jailed tests within one process.
static JAIL_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// A sandbox holding: the global jail lock, a temp dir used as cwd, and
/// snapshots of the environment + cwd taken at entry.
pub(crate) struct Jail {
    _guard: MutexGuard<'static, ()>,
    _dir: tempfile::TempDir,
    saved_cwd: PathBuf,
    saved_env: HashMap<OsString, OsString>,
}

impl Jail {
    /// Run `f` jailed (temp cwd, env snapshot, global lock); panic on `Err`.
    pub(crate) fn expect_with<F>(f: F)
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
            _dir: dir,
            saved_cwd,
            saved_env,
        };
        let result = f(&mut jail);
        drop(jail); // restore env + cwd BEFORE the panic below, so failures don't leak state
        result.expect("jail closure returned Err");
    }

    /// Set an environment variable for the duration of the jail.
    pub(crate) fn set_env<K: AsRef<str>, V: std::fmt::Display>(&mut self, key: K, value: V) {
        // SAFETY: JAIL_LOCK is held for the jail's whole lifetime, so no
        // other *jailed* test mutates the environment concurrently.
        // Soundness further requires that all env mutation in rio-common's
        // tests goes through this type (or is only run under nextest's
        // process-per-test isolation) — see the module docs.
        unsafe { std::env::set_var(key.as_ref(), value.to_string()) };
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
