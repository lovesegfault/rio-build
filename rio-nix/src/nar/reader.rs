//! NAR bytes → [`NarNode`] tree.

use std::io::{self, Read};

use super::sync_wire::{
    expect_str, read_bytes_bounded, read_name_bytes, read_string, read_target_bytes,
};
use super::{
    MAX_CONTENT_SIZE, MAX_DIRECTORY_ENTRIES, MAX_NAR_DEPTH, NAR_MAGIC, NarEntry, NarError, NarNode,
    Result, validate_entry_name,
};

/// Parse a NAR archive from a byte reader.
///
/// Returns the root [`NarNode`] representing the archived path.
pub fn parse(r: &mut impl Read) -> Result<NarNode> {
    let magic = read_string(r)?;
    if magic != NAR_MAGIC {
        return Err(NarError::InvalidMagic(magic));
    }

    parse_node(r, 0)
}

/// Parse a single NAR node.
///
/// Each sub-parser is responsible for consuming its own closing ")".
/// This is necessary because `parse_directory` reads tokens in a loop
/// and must consume ")" to detect end-of-directory.
fn parse_node(r: &mut impl Read, depth: usize) -> Result<NarNode> {
    if depth > MAX_NAR_DEPTH {
        return Err(NarError::NestingTooDeep(depth));
    }
    expect_str(r, "(")?;
    expect_str(r, "type")?;

    let node_type = read_string(r)?;
    match node_type.as_str() {
        "regular" => parse_regular(r),
        "directory" => parse_directory(r, depth),
        "symlink" => parse_symlink(r),
        _ => Err(NarError::UnknownNodeType(node_type)),
    }
}

fn parse_regular(r: &mut impl Read) -> Result<NarNode> {
    // Peek at next token: either "executable" or "contents"
    let token = read_string(r)?;
    let (executable, contents) = match token.as_str() {
        "executable" => {
            // The executable marker's value MUST be the empty string
            // (Nix C++ `parseDump` checks `s != ""`); accepting any
            // string would silently swallow stray tokens.
            expect_str(r, "")?;
            expect_str(r, "contents")?;
            let contents = read_bytes_bounded(r, MAX_CONTENT_SIZE)?;
            (true, contents)
        }
        "contents" => {
            let contents = read_bytes_bounded(r, MAX_CONTENT_SIZE)?;
            (false, contents)
        }
        _ => {
            return Err(NarError::UnexpectedToken {
                expected: "\"executable\" or \"contents\"".to_string(),
                got: token,
            });
        }
    };

    expect_str(r, ")")?;
    Ok(NarNode::Regular {
        executable,
        contents,
    })
}

fn parse_directory(r: &mut impl Read, depth: usize) -> Result<NarNode> {
    let mut entries = Vec::new();
    let mut prev_name: Option<String> = None;

    loop {
        if entries.len() >= MAX_DIRECTORY_ENTRIES {
            return Err(NarError::TooManyEntries(entries.len()));
        }

        // Peek: either "entry" or ")"
        let token = read_string(r)?;
        match token.as_str() {
            ")" => {
                // Closing ")" consumed — directory complete.
                return Ok(NarNode::Directory { entries });
            }
            "entry" => {
                expect_str(r, "(")?;
                expect_str(r, "name")?;

                let name_bytes = read_name_bytes(r)?;
                let name = String::from_utf8(name_bytes).map_err(|e| NarError::InvalidUtf8 {
                    context: "entry name",
                    offset: e.utf8_error().valid_up_to(),
                    source: e,
                })?;

                // Check runs before prev_name update so a rejected name
                // doesn't pollute sort-order state.
                validate_entry_name(&name)?;

                // Enforce sorted order
                if let Some(ref prev) = prev_name
                    && name <= *prev
                {
                    return Err(NarError::UnsortedEntries {
                        prev: prev.clone(),
                        cur: name,
                    });
                }
                prev_name = Some(name.clone());

                expect_str(r, "node")?;
                let node = parse_node(r, depth + 1)?;
                expect_str(r, ")")?; // close the entry's parens

                entries.push(NarEntry { name, node });
            }
            _ => {
                return Err(NarError::UnexpectedToken {
                    expected: "\"entry\" or \")\"".to_string(),
                    got: token,
                });
            }
        }
    }
}

fn parse_symlink(r: &mut impl Read) -> Result<NarNode> {
    expect_str(r, "target")?;
    let target_bytes = read_target_bytes(r)?;
    let target = String::from_utf8(target_bytes).map_err(|e| NarError::InvalidUtf8 {
        context: "symlink target",
        offset: e.utf8_error().valid_up_to(),
        source: e,
    })?;
    expect_str(r, ")")?;
    Ok(NarNode::Symlink { target })
}

/// Extract the content of a single regular file from a NAR.
///
/// Returns `Err(NarError::NotSingleFile)` if the root NAR node is not a
/// regular file (i.e., it's a directory or symlink).
/// This is the common case for `.drv` files uploaded via `wopAddToStoreNar`.
pub fn extract_single_file(nar_data: &[u8]) -> Result<Vec<u8>> {
    let node = parse(&mut io::Cursor::new(nar_data))?;
    match node {
        NarNode::Regular { contents, .. } => Ok(contents),
        _ => Err(NarError::NotSingleFile),
    }
}
