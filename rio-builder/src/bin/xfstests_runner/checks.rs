//! The check registry — the table of contents of every ported check.
//!
//! Every check is a Rust function asserting kernel-visible behavior of
//! the live castore-FUSE mount through direct syscalls — deterministic
//! errnos, no shell, no commands that can prompt. Check names carry
//! their xfstests origin (`generic_NNN_*`); the doc comment on each
//! function states what failure mode of this FUSE it guards. Shared
//! types and assertion helpers live in [`support`].
//!
//! Checks run in registration order and the order is load-bearing for
//! the read-path checks: `generic_075_091_read_integrity` must perform
//! the FIRST read of the big blob (the cold/streaming path) before the
//! concurrency and write-through checks touch the same file warm.
//! `--filter` exists for debugging a single check; a filtered run
//! cannot distinguish cold from warm reads.

pub mod dir_locks;
pub mod errno_battery;
pub mod io_paths;
pub mod meta;
pub mod read;
pub mod support;
pub mod walker;
pub mod write_attack;
pub mod xattr_statx;

pub use support::{
    Check, Ctx, Outcome, PrivDrop, RawDir, count_nodes, cpath, errno_of, expect_errno,
    first_divergence, open_raw, readable_plain_file, resolve_dep_root, wait_for,
};

/// All checks, in execution order (cold-read sequencing, then
/// privilege-dropping batteries, then the root-probe batteries, with
/// the state-restoring write-through and dirty-pipe page-cache probes
/// last — they need the big blob warm in the passthrough page cache).
pub fn registry() -> Vec<Check> {
    vec![
        Check {
            name: "mount_castore_fstype",
            origin: "harness (xfstests _require_scratch)",
            run: meta::mount_castore_fstype,
        },
        Check {
            name: "overlay_readdir_consumer",
            origin: "xfstests generic/257+453 (overlay-lowerdir leg)",
            run: meta::overlay_readdir_consumer,
        },
        Check {
            name: "generic_257_readdir_multibatch",
            origin: "xfstests generic/257",
            run: meta::generic_257_readdir_multibatch,
        },
        Check {
            name: "inode_identity_content_addressed",
            origin: "castore-specific (content-addressed identity)",
            run: meta::inode_identity_content_addressed,
        },
        Check {
            name: "generic_401_file_kinds",
            origin: "xfstests generic/401",
            run: meta::generic_401_file_kinds,
        },
        Check {
            name: "generic_002_nlink_walk",
            origin: "xfstests generic/002 (adapted: honest nlink per builder.fs.castore-nlink)",
            run: meta::generic_002_nlink_walk,
        },
        Check {
            name: "generic_005_symlink_errnos",
            origin: "xfstests generic/005",
            run: meta::generic_005_symlink_errnos,
        },
        Check {
            name: "generic_360_symlink_targets",
            origin: "xfstests generic/360",
            run: meta::generic_360_symlink_targets,
        },
        Check {
            name: "generic_453_byte_exact_names",
            origin: "xfstests generic/453",
            run: meta::generic_453_byte_exact_names,
        },
        // Identity/walker batch — the escaped-bug class (hardlinked-dir
        // semantics from digest-keyed directory inodes). Metadata-only:
        // none of these read file contents, so the cold-read sequencing
        // of the blob checks below is preserved.
        Check {
            name: "posix_dir_inode_uniqueness",
            origin: "castore-specific (POSIX: no hardlinked directories; GNU fts escape)",
            run: walker::posix_dir_inode_uniqueness,
        },
        Check {
            name: "hardlink_nlink_honesty",
            origin: "xfstests generic/002+100 intent (tar/du (dev,ino,nlink) honesty)",
            run: walker::hardlink_nlink_honesty,
        },
        Check {
            name: "dot_dotdot_identity",
            origin: "POSIX dir sanity ('.'/'..' identity; pairs with generic/028)",
            run: walker::dot_dotdot_identity,
        },
        Check {
            name: "generic_028_getcwd_stability",
            origin: "xfstests generic/028 (read-only adaptation: alias lookups as dentry churn)",
            run: walker::generic_028_getcwd_stability,
        },
        Check {
            name: "generic_011_dirstress",
            origin: "xfstests generic/011 (read-only adaptation)",
            run: walker::generic_011_dirstress,
        },
        Check {
            name: "fts_walk_concurrent_aliases",
            origin: "regression: GNU find fts ENOENT on aliased dir dentries (data-plane escape)",
            run: walker::fts_walk_concurrent_aliases,
        },
        Check {
            name: "generic_075_091_read_integrity",
            origin: "xfstests generic/075 + generic/091",
            run: read::generic_075_091_read_integrity,
        },
        Check {
            name: "generic_095_113_310_concurrency",
            origin: "xfstests generic/095 + generic/310 + generic/113 (sync open/close legs only; no AIO)",
            run: read::generic_095_113_310_concurrency,
        },
        // mmap/splice/copy_file_range run after the big blob is warm
        // (promoted to passthrough by the read-integrity check) so they
        // exercise the passthrough backing fd, the production read path.
        Check {
            name: "generic_074_127_mmap_reads",
            origin: "xfstests generic/074 + generic/127",
            run: io_paths::generic_074_127_mmap_reads,
        },
        Check {
            name: "generic_249_splice_read",
            origin: "xfstests generic/249",
            run: io_paths::generic_249_splice_read,
        },
        Check {
            name: "generic_430_553_copy_file_range",
            origin: "xfstests generic/430 + generic/553",
            run: io_paths::generic_430_553_copy_file_range,
        },
        Check {
            name: "generic_285_448_706_seek_hole_data",
            origin: "xfstests generic/285 + generic/448 + generic/706",
            run: io_paths::generic_285_448_706_seek_hole_data,
        },
        Check {
            name: "generic_263_odirect_read",
            origin: "xfstests generic/263 (read-only adaptation: O_DIRECT serves exact bytes or EINVAL)",
            run: io_paths::generic_263_odirect_read,
        },
        Check {
            name: "generic_013_fsstress_readonly",
            origin: "xfstests generic/013 + generic/241 intent (read-only op-mix soak, 4 threads)",
            run: walker::generic_013_fsstress_readonly,
        },
        Check {
            name: "generic_003_192_atime_stable",
            origin: "xfstests generic/003 + generic/192 (read-only adaptation: atime never moves)",
            run: walker::generic_003_192_atime_stable,
        },
        Check {
            name: "generic_467_open_by_handle",
            origin: "xfstests generic/467 (+426/477/756/777 refusal contract)",
            run: io_paths::generic_467_open_by_handle,
        },
        Check {
            name: "generic_020_062_097_xattr_read_legs",
            origin: "xfstests generic/020 + generic/062 + generic/097 (xattr read legs)",
            run: xattr_statx::generic_020_062_097_xattr_read_legs,
        },
        Check {
            name: "generic_423_statx_field_correctness",
            origin: "xfstests generic/423",
            run: xattr_statx::generic_423_statx_field_correctness,
        },
        Check {
            name: "generic_532_statx_attributes_mask_sanity",
            origin: "xfstests generic/532",
            run: xattr_statx::generic_532_statx_attributes_mask_sanity,
        },
        Check {
            name: "generic_471_rewinddir",
            origin: "xfstests generic/471",
            run: dir_locks::generic_471_rewinddir,
        },
        Check {
            name: "generic_676_seekdir",
            origin: "xfstests generic/676",
            run: dir_locks::generic_676_seekdir,
        },
        Check {
            name: "generic_088_second_uid_dac",
            origin: "xfstests generic/088",
            run: dir_locks::generic_088_second_uid_dac,
        },
        Check {
            name: "generic_131_read_locks",
            origin: "xfstests generic/131",
            run: dir_locks::generic_131_read_locks,
        },
        Check {
            name: "generic_478_571_ofd_locks_lease",
            origin: "xfstests generic/478 + generic/571 (OFD lock + lease read legs)",
            run: dir_locks::generic_478_571_ofd_locks_lease,
        },
        Check {
            name: "generic_637_small_getdents",
            origin: "xfstests generic/637 (small-getdents completeness leg)",
            run: dir_locks::generic_637_small_getdents,
        },
        Check {
            name: "generic_126_exec_access",
            origin: "xfstests generic/126",
            run: errno_battery::generic_126_exec_access,
        },
        Check {
            name: "generic_050_write_protection_unprivileged",
            origin: "xfstests generic/050 + generic/123 (adapted: unprivileged overwrite/append/delete/move all denied)",
            run: errno_battery::generic_050_write_protection_unprivileged,
        },
        Check {
            name: "generic_294_eexist_unprivileged",
            origin: "xfstests generic/294",
            run: errno_battery::generic_294_eexist_unprivileged,
        },
        Check {
            name: "generic_294_erofs_battery_root",
            origin: "xfstests generic/294 + generic/050 (root leg)",
            run: errno_battery::generic_294_erofs_battery_root,
        },
        Check {
            name: "generic_007_enoent_never_eio",
            origin: "xfstests generic/007",
            run: errno_battery::generic_007_enoent_never_eio,
        },
        Check {
            name: "statfs_zero_totals",
            origin: "castore-specific (statfs F-A pin; no upstream statfs-totals analogue)",
            run: errno_battery::statfs_zero_totals,
        },
        Check {
            name: "mount_readonly_honesty",
            origin: "harness ro-mount intent (statvfs ST_RDONLY + /proc/mounts ro + W_OK EROFS)",
            run: errno_battery::mount_readonly_honesty,
        },
        Check {
            name: "generic_006_name_limits",
            origin: "xfstests generic/006 intent (NAME_MAX/PATH_MAX errno contracts)",
            run: errno_battery::generic_006_name_limits,
        },
        Check {
            name: "open_flag_contracts",
            origin: "xfstests generic/004 + generic/763 intent (O_DIRECTORY/O_NOFOLLOW/O_PATH/O_TMPFILE/zero-write)",
            run: errno_battery::open_flag_contracts,
        },
        Check {
            name: "generic_680_dirty_pipe",
            origin: "xfstests generic/680 (CVE-2022-0847, Dirty Pipe)",
            run: write_attack::generic_680_dirty_pipe,
        },
        Check {
            name: "write_through_passthrough_root",
            origin: "xfstests generic/050 (root write leg)",
            run: errno_battery::write_through_passthrough_root,
        },
    ]
}
