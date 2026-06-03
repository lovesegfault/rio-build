//! Regenerate `.sqlx/` offline query cache.
//!
//! Spins up an ephemeral postgres (reusing `rio-test-support::pg::PgServer`
//! — the same initdb+spawn bootstrap the integration tests use), runs
//! migrations, then `cargo sqlx prepare --workspace`.

use std::path::PathBuf;
use std::time::SystemTime;

use anyhow::Result;

use crate::sh::{self, cmd, repo_root, shell};
use crate::ui;

pub async fn run() -> Result<()> {
    let sh = shell()?;

    // rio-test-support bootstraps a process-global postgres (initdb +
    // spawn, PR_SET_PDEATHSIG cleanup). Unix-socket-only; sqlx-cli
    // handles the `?host=/path` URL form.
    let pg = rio_test_support::pg::PgServer::get();
    let url = pg.admin_url();

    // Devshell sets SQLX_OFFLINE=true globally so cargo build works
    // without PG. Unset so prepare actually hits the DB.
    let _env = sh.push_env("DATABASE_URL", url);
    let _env2 = sh.push_env("SQLX_OFFLINE", "false");
    // Isolate prepare's inner builds into a sub-target so the main
    // target/ stays warm: `cargo sqlx prepare` does per-package feature
    // resolution (different unit graphs than workspace builds) and bumps
    // source mtimes, both of which would churn the main fingerprints.
    // (sqlx-cli 0.9 only *forwards* a pre-existing RUSTFLAGS — verified
    // against its prepare.rs — so the historical "sqlx sets RUSTFLAGS"
    // poisoning rationale no longer applies; the isolation stays for the
    // reasons above.)
    let isolated = sh.current_dir().join("target/sqlx-prepare");
    let _env3 = sh.push_env("CARGO_TARGET_DIR", &isolated);
    // Disable the kache wrapper for every inner build of this flow.
    // Prepare's compiles are typed against the LIVE database
    // (DATABASE_URL set, SQLX_OFFLINE=false), but neither variable
    // reaches a cache key — sqlx-macros read them via plain
    // std::env::var, invisible to dep-info on stable — and RIO_SQLX_HASH
    // still hashes the PRE-regen .sqlx (the CLI rewrites it only after
    // the inner builds finish). Caching such compiles would store
    // online-typed artifacts under offline-looking keys: today they land
    // in a parallel keyspace only because CARGO_TARGET_DIR alters the
    // remap-sentinel set, which is an accident, not a guarantee — and
    // either way each regen would flood the shared store with a full
    // parallel workspace build that nothing replays. Disabled mode is
    // pure passthrough (no lookup, no store, no pre-pass); prepare's
    // metadata emission is unaffected because the REAL rustc run always
    // expands the macros.
    let _env4 = sh.push_env("KACHE_DISABLED", "1");
    // Earlier kache-ENABLED runs hardlink-restored artifacts into this
    // scratch target as read-only store inodes (mode 0444, nlink 2).
    // With the wrapper disabled, kache's pre-clean never runs and plain
    // rustc EACCESes overwriting them ("output file … is not
    // writeable"). Unlink them up front — never chmod: the inode is
    // SHARED with the kache store, so making it writable would let
    // rustc truncate the store blob in place and poison every future
    // restore of that entry. Unlinking only drops this target's link;
    // cargo rebuilds the missing outputs.
    //
    // `! -perm -u+w` (mode bits), NOT `! -writable`: -writable is an
    // access(2) test, vacuous under uid 0 — root would skip the unlink
    // and rustc's truncating opens would then silently rewrite the
    // shared store inode (the exact poisoning forbidden above) instead
    // of EACCESing.
    // `-links +1` narrows the unlink to the shared-inode signature:
    // kache restores are hardlinks by construction (nlink >= 2, the
    // second link being the store blob), while legitimate read-only
    // OUT_DIR artifacts (build scripts fs::copy'ing from read-only
    // sources, e.g. nix-vendored bundled bindings) are nlink-1 and must
    // survive — cargo never re-runs a build script because OUT_DIR
    // content vanished, so deleting them bricks the scratch target.
    //
    // Division of labor with kacheWrapped (nix/devshell.nix): the
    // RUSTC_WRAPPER now sweeps each compile's own --out-dir on every
    // disabled/bypassed invocation, INCLUDING nlink-1 reflink restores
    // (kache tries reflink before hardlink, v0.4.0 link.rs:52, and only
    // Copy-strategy artifacts get chmodded 0755 — so on a reflink fs a
    // restore is read-only at nlink 1, invisible to the `-links +1`
    // signature here, which this scope must keep for the OUT_DIR files
    // above). This tree-wide pass stays as defense in depth for
    // invocations that never see the wrapper (bare cargo without
    // RUSTC_WRAPPER on the scratch target).
    if isolated.exists() {
        sh::run(cmd!(
            sh,
            "find {isolated} -type f -links +1 ! -perm -u+w -delete"
        ))
        .await?;
    }

    // `cargo sqlx prepare` bumps src/{lib,main}.rs mtimes on every
    // workspace crate to force proc-macro re-expansion (stable can't
    // track env vars). Snapshot mtimes now, restore after, so the
    // main target/ fingerprint stays valid.
    let snapshot = snapshot_mtimes()?;
    let _restore = scopeguard::guard(snapshot, |s| {
        for (p, t) in s {
            let _ = filetime::set_file_mtime(&p, filetime::FileTime::from(t));
        }
    });

    ui::step("cargo sqlx migrate run", || {
        sh::run(cmd!(
            sh,
            "cargo sqlx migrate run --source rio-migrations/migrations"
        ))
    })
    .await?;

    // --check first: exits 0 if cache is current. Non-zero → regenerate.
    //
    // `cargo sqlx prepare --workspace` internally does `cargo rustc -p
    // <crate>` per member — per-package feature resolution, unavoidable.
    // The --check fast-path avoids the rebuild in the common case.
    //
    // `-- --all-targets`: forwarded to the inner `cargo rustc` so
    // `#[cfg(test)]` queries are cached too. The cross-service
    // `LivePin` contract anchor (rio-store gc tests) lives under
    // cfg(test) — without this, regen succeeds but `cargo test`
    // fails on "no cached data for this query".
    let current = ui::step("cargo sqlx prepare --check", || async {
        Ok(sh::run(cmd!(
            sh,
            "cargo sqlx prepare --workspace --check -- --all-targets"
        ))
        .await
        .is_ok())
    })
    .await?;
    if current {
        tracing::debug!("sqlx cache already current");
        return Ok(());
    }

    ui::step("cargo sqlx prepare --workspace", || {
        sh::run(cmd!(sh, "cargo sqlx prepare --workspace -- --all-targets"))
    })
    .await?;

    let count = std::fs::read_dir(sh.current_dir().join(".sqlx"))
        .map(|d| d.count())
        .unwrap_or(0);
    tracing::debug!("{count} queries cached");
    Ok(())
}

/// Snapshot mtimes of all files sqlx-cli touches for its "minimal
/// recompile setup": build.rs, src/lib.rs, src/main.rs, src/bin/*.rs.
fn snapshot_mtimes() -> Result<Vec<(PathBuf, SystemTime)>> {
    let mut out = Vec::new();
    let mut record = |p: PathBuf| {
        if let Ok(m) = std::fs::metadata(&p)
            && let Ok(t) = m.modified()
        {
            out.push((p, t));
        }
    };
    for entry in std::fs::read_dir(repo_root())? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let root = entry.path();
        record(root.join("build.rs"));
        let src = root.join("src");
        record(src.join("lib.rs"));
        record(src.join("main.rs"));
        if let Ok(bins) = std::fs::read_dir(src.join("bin")) {
            for b in bins.flatten() {
                if b.path().extension().is_some_and(|e| e == "rs") {
                    record(b.path());
                }
            }
        }
    }
    Ok(out)
}
