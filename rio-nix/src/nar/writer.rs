//! [`NarNode`] tree → NAR bytes.

use std::io::Write;

use super::sync_wire::{write_bytes, write_str};
use super::{MAX_NAR_DEPTH, NAR_MAGIC, NarError, NarNode, Result};

/// Serialize a [`NarNode`] tree to NAR format.
///
/// Enforces the same [`MAX_NAR_DEPTH`] limit as the parser and the
/// filesystem walkers: serialization recurses per directory level, so a
/// deeper tree (only constructible by hand — every in-tree producer is
/// itself depth-capped) must fail with
/// [`NestingTooDeep`](NarError::NestingTooDeep), not overflow the stack.
pub fn serialize(w: &mut impl Write, node: &NarNode) -> Result<()> {
    write_str(w, NAR_MAGIC)?;
    serialize_node(w, node, 0)
}

pub(super) fn serialize_node(w: &mut impl Write, node: &NarNode, depth: usize) -> Result<()> {
    if depth > MAX_NAR_DEPTH {
        return Err(NarError::NestingTooDeep(depth));
    }
    write_str(w, "(")?;
    write_str(w, "type")?;

    match node {
        NarNode::Regular {
            executable,
            contents,
        } => {
            write_str(w, "regular")?;
            if *executable {
                write_str(w, "executable")?;
                write_str(w, "")?;
            }
            write_str(w, "contents")?;
            write_bytes(w, contents)?;
            write_str(w, ")")?;
        }
        NarNode::Directory { entries } => {
            write_str(w, "directory")?;
            for entry in entries {
                write_str(w, "entry")?;
                write_str(w, "(")?;
                write_str(w, "name")?;
                write_str(w, &entry.name)?;
                write_str(w, "node")?;
                serialize_node(w, &entry.node, depth + 1)?;
                write_str(w, ")")?;
            }
            write_str(w, ")")?;
        }
        NarNode::Symlink { target } => {
            write_str(w, "symlink")?;
            write_str(w, "target")?;
            write_str(w, target)?;
            write_str(w, ")")?;
        }
    }

    Ok(())
}
