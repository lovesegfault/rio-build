//! Castore-FUSE lazy `/nix/store` (ADR-022 §2).
//!
//! This module tree replaces the whole-store-path JIT FUSE in
//! [`crate::fuse`] at the P0560 cutover. Until then both coexist:
//! `fuse/` serves production, `castore_fuse/` accretes the new stack
//! bottom-up. So far that is the privileged-broker half ([`mountd`] +
//! [`mountd_proto`]); the filesystem itself (inode table, chunk fetch,
//! passthrough open, overlay assembly) is P0559.

pub mod mountd;
pub mod mountd_proto;
