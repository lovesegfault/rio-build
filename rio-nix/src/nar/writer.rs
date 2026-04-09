//! [`NarNode`] tree → NAR bytes.

use std::io::Write;

use super::sync_wire::{write_bytes, write_str};
use super::{NAR_MAGIC, NarNode, Result};

/// Serialize a [`NarNode`] tree to NAR format.
pub fn serialize(w: &mut impl Write, node: &NarNode) -> Result<()> {
    write_str(w, NAR_MAGIC)?;
    serialize_node(w, node)
}

pub(super) fn serialize_node(w: &mut impl Write, node: &NarNode) -> Result<()> {
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
                serialize_node(w, &entry.node)?;
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
