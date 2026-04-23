//! Concatenate workspace Rust source for the `every_table_is_queried`
//! schema-liveness guard (`tests/migrations.rs`).
//!
//! That test asserts every table named in `migrations/*.sql` appears as
//! a literal token in ≥1 workspace `.rs` file, so a migration adding
//! dead schema fails CI BEFORE `migration_checksums_frozen` freezes it
//! (after which dropping the column needs a NEW migration).
//!
//! The corpus is workspace `.rs` source — NOT `.sqlx/query-*.json`,
//! which only covers the `query!`/`query_as!` macro family. Non-macro
//! `sqlx::query()` / `QueryBuilder` callsites (≈200 in this repo,
//! including `sla_ema_state` at `cost.rs:357`) leave no `.sqlx/` entry.
//!
//! Scope: crates that hold PG queries (rio-store, rio-scheduler, xtask).
//! `src/migrations.rs` is excluded — its per-migration doc-consts
//! mention table names as commentary, which would mask exactly the dead
//! tables the guard exists to catch.
//!
//! crate2nix: `nix/crate2nix.nix` symlinks `rio-scheduler/src` and
//! `xtask/src` next to the unpacked `rio-store/` so the `../<crate>/src`
//! walks here resolve under per-crate builds (same pattern as the
//! `migrations/` symlink for `sqlx::migrate!`).

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

fn main() {
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let out_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap());

    // (label, root). rio-store/src is local; the other two are sibling
    // crates resolved via `../`. Under crate2nix the siblings are
    // symlinks into nix-store filesets — see `pgQuerySrcFileset`.
    let roots = [
        ("rio-store", manifest.join("src")),
        ("rio-scheduler", manifest.join("../rio-scheduler/src")),
        ("xtask", manifest.join("../xtask/src")),
    ];

    let out_path = out_dir.join("all_rs.txt");
    let mut out = fs::File::create(&out_path).unwrap();
    let mut total = 0usize;

    for (label, root) in &roots {
        // Coarse rerun trigger — per-file `rerun-if-changed` on every
        // `.rs` would be hundreds of stat calls per build; the dir-level
        // trigger fires on add/remove/mtime which is sufficient here.
        println!("cargo:rerun-if-changed={}", root.display());
        assert!(
            root.is_dir(),
            "schema-liveness corpus root {} not found — under crate2nix \
             this means nix/crate2nix.nix's rio-store override didn't \
             symlink {label}/src into $NIX_BUILD_TOP",
            root.display()
        );
        walk_rs(root, &mut |p| {
            // Exclude the migration doc-const file: it names every table
            // as prose, which would defeat the liveness check.
            if p.file_name().is_some_and(|n| n == "migrations.rs") {
                return;
            }
            writeln!(out, "// ──── {} ────", p.display()).unwrap();
            out.write_all(&fs::read(p).unwrap()).unwrap();
            total += 1;
        });
    }

    // Non-empty sanity: a silently-empty corpus would make the test
    // vacuously fail every table — better to fail at build time with a
    // clear message.
    assert!(
        total > 50,
        "schema-liveness corpus suspiciously small ({total} files) — \
         expected ≥50 across rio-store + rio-scheduler + xtask"
    );
}

/// Recursive `.rs` walk via std (no `walkdir` build-dep). Follows
/// symlinks (the crate2nix sibling roots ARE symlinks).
fn walk_rs(dir: &Path, f: &mut impl FnMut(&Path)) {
    for entry in fs::read_dir(dir).unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        // `metadata()` (not `file_type()`) so symlinked dirs recurse.
        let md = fs::metadata(&path).unwrap();
        if md.is_dir() {
            walk_rs(&path, f);
        } else if path.extension().is_some_and(|e| e == "rs") {
            f(&path);
        }
    }
}
