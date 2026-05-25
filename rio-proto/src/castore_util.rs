//! Helpers on the `rio.castore` wire types that both producers and
//! consumers of `Directory` bodies must agree on.
//!
//! ADR-022 §6 makes the builder a *producer* of `Directory` bodies (the
//! fused output walk constructs them from the overlay upper) and the
//! store a *validator* of untrusted ones (`PutPathChunkedBegin.
//! directories`). The digest computation lives here — in the crate that
//! owns the type — so the two sides cannot drift: a builder that
//! digests its bodies differently from the store's recompute would have
//! every upload rejected at `validate_begin`'s reachability walk.
//!
//! The exact canonical-encoding bytes are pinned by
//! `rio_store::castore::tests::golden_directory_encoding` (kept there
//! because the legacy `nar_ls`-driven builder lives there too).

use prost::Message;

use rio_common::limits::{
    MAX_CASTORE_DIR_ENTRIES, MAX_CASTORE_NAME_BYTES, MAX_CASTORE_TARGET_BYTES,
};

use crate::castore::Directory;

/// `dir_digest = blake3(canonical_encode(Directory))`.
///
/// The canonical encoding is prost's default field-order encode of a
/// `Directory` whose three entry lists are each sorted byte-lex by
/// `name` (`r[store.castore.canonical-encoding]`). The caller is
/// responsible for the sort; this function does not re-sort (a
/// mis-sorted body hashes to a *different* digest, which is exactly
/// what makes [`validate_directory`] + a digest recompute sufficient
/// to reject it).
// r[impl store.castore.canonical-encoding]
pub fn directory_digest(d: &Directory) -> [u8; 32] {
    *blake3::hash(&d.encode_to_vec()).as_bytes()
}

/// Why a client-supplied [`Directory`] body was rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum DirectoryError {
    #[error("entry name {0:?} is not a valid single path component")]
    InvalidName(String),
    #[error("entries not sorted byte-lexicographically by name within a kind list at {0:?}")]
    Unsorted(String),
    #[error("duplicate entry name {0:?} across the directory's kind lists")]
    DuplicateName(String),
    #[error("child digest for entry {0:?} is {1} bytes, expected 32")]
    BadDigestLen(String, usize),
    #[error("entry name {0:?} is {1} bytes, exceeds {MAX_CASTORE_NAME_BYTES}")]
    NameTooLong(String, usize),
    #[error("symlink {0:?} target is {1} bytes, exceeds {MAX_CASTORE_TARGET_BYTES} (or empty)")]
    BadTarget(String, usize),
    #[error("{0} entries in one directory, exceeds {MAX_CASTORE_DIR_ENTRIES}")]
    TooManyEntries(usize),
}

/// Display of a possibly-non-UTF-8 entry name for error messages.
/// Always escaped: these strings end up in tonic `Status` messages and
/// `tracing` events, and a name is attacker-chosen bytes — newlines,
/// `\r`, and ANSI escapes must not pass through verbatim into a log
/// line or a terminal. Truncated so a near-limit name doesn't bloat
/// the error to hundreds of bytes.
fn show(name: &[u8]) -> String {
    let head = &name[..name.len().min(64)];
    let mut s = head.escape_ascii().to_string();
    if name.len() > head.len() {
        s.push('…');
    }
    s
}

/// Structural validation of a single `Directory` body (snix
/// `Directory::validate` equivalent).
///
/// Checks, per `castore.proto`'s documented invariants and the NAR
/// reader's own entry bounds (a `Directory` that would regenerate a
/// NAR the reader rejects must not be committable):
/// - every entry name is a single path component: non-empty, not `.`
///   or `..`, contains no `/` and no NUL, and is at most
///   [`MAX_CASTORE_NAME_BYTES`] bytes;
/// - every symlink target is non-empty, NUL-free, and at most
///   [`MAX_CASTORE_TARGET_BYTES`] bytes;
/// - the combined entry count is at most [`MAX_CASTORE_DIR_ENTRIES`];
/// - each of the three kind lists is sorted byte-lexicographically by
///   name (a prerequisite of the canonical encoding — an unsorted body
///   would digest differently from the same logical directory built by
///   `nar_ls`);
/// - names are unique across all three lists combined;
/// - `DirectoryEntry.digest` and `FileEntry.digest` are 32 bytes.
///
/// Does NOT check `DirectoryEntry.size` consistency (that needs the
/// referenced child bodies — the caller's reachability walk owns it;
/// nothing may trust the field before that walk has verified it)
/// and does NOT verify any digest (callers recompute reachable digests
/// via [`directory_digest`]).
pub fn validate_directory(d: &Directory) -> Result<(), DirectoryError> {
    fn valid_component(name: &[u8]) -> bool {
        !name.is_empty()
            && name != b"."
            && name != b".."
            && !name.contains(&b'/')
            && !name.contains(&0)
    }
    fn check_sorted<'a>(names: impl Iterator<Item = &'a [u8]>) -> Result<(), DirectoryError> {
        let mut prev: Option<&[u8]> = None;
        for n in names {
            if !valid_component(n) {
                return Err(DirectoryError::InvalidName(show(n)));
            }
            if n.len() > MAX_CASTORE_NAME_BYTES {
                return Err(DirectoryError::NameTooLong(show(n), n.len()));
            }
            if let Some(p) = prev
                && p >= n
            {
                return Err(DirectoryError::Unsorted(show(n)));
            }
            prev = Some(n);
        }
        Ok(())
    }

    let entry_count = d.directories.len() + d.files.len() + d.symlinks.len();
    if entry_count > MAX_CASTORE_DIR_ENTRIES {
        return Err(DirectoryError::TooManyEntries(entry_count));
    }

    check_sorted(d.directories.iter().map(|e| e.name.as_slice()))?;
    check_sorted(d.files.iter().map(|e| e.name.as_slice()))?;
    check_sorted(d.symlinks.iter().map(|e| e.name.as_slice()))?;

    for e in &d.directories {
        if e.digest.len() != 32 {
            return Err(DirectoryError::BadDigestLen(show(&e.name), e.digest.len()));
        }
    }
    for e in &d.files {
        if e.digest.len() != 32 {
            return Err(DirectoryError::BadDigestLen(show(&e.name), e.digest.len()));
        }
    }
    for e in &d.symlinks {
        // An empty or NUL-bearing target regenerates a NAR (or a FUSE
        // readlink) the consumer rejects; an oversized one is a memory
        // amplifier in the framing buffer.
        if e.target.is_empty() || e.target.len() > MAX_CASTORE_TARGET_BYTES || e.target.contains(&0)
        {
            return Err(DirectoryError::BadTarget(show(&e.name), e.target.len()));
        }
    }

    // Cross-list uniqueness. Each list is individually sorted and
    // duplicate-free (strict `<` above), so a three-way sorted merge
    // would also work — but the lists are typically tiny and a HashSet
    // reads more directly.
    let mut seen = std::collections::HashSet::with_capacity(
        d.directories.len() + d.files.len() + d.symlinks.len(),
    );
    for name in d
        .directories
        .iter()
        .map(|e| e.name.as_slice())
        .chain(d.files.iter().map(|e| e.name.as_slice()))
        .chain(d.symlinks.iter().map(|e| e.name.as_slice()))
    {
        if !seen.insert(name) {
            return Err(DirectoryError::DuplicateName(show(name)));
        }
    }
    Ok(())
}

// r[verify store.castore.canonical-encoding]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::castore::{DirectoryEntry, FileEntry, SymlinkEntry};

    fn file(name: &[u8]) -> FileEntry {
        FileEntry {
            name: name.to_vec(),
            digest: vec![1u8; 32],
            size: 1,
            executable: false,
        }
    }

    #[test]
    fn digest_is_blake3_of_prost_encode() {
        let d = Directory {
            directories: vec![],
            files: vec![file(b"a")],
            symlinks: vec![],
        };
        assert_eq!(
            directory_digest(&d),
            *blake3::hash(&d.encode_to_vec()).as_bytes()
        );
        // Different body → different digest (sanity, not crypto).
        let d2 = Directory {
            files: vec![file(b"b")],
            ..d.clone()
        };
        assert_ne!(directory_digest(&d), directory_digest(&d2));
    }

    #[test]
    fn validate_accepts_a_well_formed_directory() {
        let d = Directory {
            directories: vec![DirectoryEntry {
                name: b"sub".to_vec(),
                digest: vec![2u8; 32],
                size: 0,
            }],
            files: vec![file(b"a"), file(b"b")],
            symlinks: vec![SymlinkEntry {
                name: b"link".to_vec(),
                target: b"a".to_vec(),
            }],
        };
        assert_eq!(validate_directory(&d), Ok(()));
        // Empty directory is valid (≈90% of nixpkgs dirs are empty).
        assert_eq!(validate_directory(&Directory::default()), Ok(()));
    }

    #[test]
    fn validate_rejects_malformed_names() {
        for bad in [&b""[..], b".", b"..", b"a/b", b"a\0b"] {
            let d = Directory {
                files: vec![file(bad)],
                ..Default::default()
            };
            assert!(
                matches!(validate_directory(&d), Err(DirectoryError::InvalidName(_))),
                "name {bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn validate_rejects_unsorted_and_duplicate_names() {
        let unsorted = Directory {
            files: vec![file(b"b"), file(b"a")],
            ..Default::default()
        };
        assert!(matches!(
            validate_directory(&unsorted),
            Err(DirectoryError::Unsorted(_))
        ));

        // Same-list duplicates surface as Unsorted (strict <); the
        // cross-list case needs the HashSet.
        let dup_same_list = Directory {
            files: vec![file(b"a"), file(b"a")],
            ..Default::default()
        };
        assert!(matches!(
            validate_directory(&dup_same_list),
            Err(DirectoryError::Unsorted(_))
        ));

        let dup_cross_list = Directory {
            files: vec![file(b"x")],
            symlinks: vec![SymlinkEntry {
                name: b"x".to_vec(),
                target: b"y".to_vec(),
            }],
            ..Default::default()
        };
        assert!(matches!(
            validate_directory(&dup_cross_list),
            Err(DirectoryError::DuplicateName(_))
        ));
    }

    #[test]
    fn validate_rejects_oversized_names_targets_and_entry_counts() {
        // Name one byte over the cap. Without this bound a 100 MB name
        // passes validation, inflates the regenerated NAR framing, and
        // produces a body the NAR reader itself rejects (NameTooLong) —
        // a committed path that can never be re-served.
        let long_name = Directory {
            files: vec![file(&vec![b'a'; MAX_CASTORE_NAME_BYTES + 1])],
            ..Default::default()
        };
        assert!(matches!(
            validate_directory(&long_name),
            Err(DirectoryError::NameTooLong(_, _))
        ));
        // At the cap is fine.
        let at_cap = Directory {
            files: vec![file(&vec![b'a'; MAX_CASTORE_NAME_BYTES])],
            ..Default::default()
        };
        assert_eq!(validate_directory(&at_cap), Ok(()));

        // Symlink targets: empty (uncreatable via symlink(2)),
        // oversized, and NUL-bearing are all rejected.
        for bad_target in [
            vec![],
            vec![b't'; MAX_CASTORE_TARGET_BYTES + 1],
            vec![b'a', 0],
        ] {
            let d = Directory {
                symlinks: vec![SymlinkEntry {
                    name: b"l".to_vec(),
                    target: bad_target.clone(),
                }],
                ..Default::default()
            };
            assert!(
                matches!(validate_directory(&d), Err(DirectoryError::BadTarget(_, _))),
                "target of {} bytes must be rejected",
                bad_target.len()
            );
        }
    }

    #[test]
    fn error_display_escapes_hostile_name_bytes() {
        // Entry names are attacker-chosen bytes that flow into tonic
        // Status messages and log lines. A newline or an ANSI escape
        // passed through verbatim is log injection. The duplicate
        // forces an error variant that carries the name.
        let hostile = b"evil\n\x1b[31mINJECTED";
        let d = Directory {
            files: vec![file(hostile), file(hostile)],
            ..Default::default()
        };
        let err = validate_directory(&d).unwrap_err();
        let msg = err.to_string();
        assert!(
            !msg.contains('\n') && !msg.contains('\x1b'),
            "error display must escape control bytes, got: {msg:?}"
        );
    }

    #[test]
    fn validate_rejects_short_digests() {
        let d = Directory {
            files: vec![FileEntry {
                name: b"a".to_vec(),
                digest: vec![1u8; 31],
                size: 1,
                executable: false,
            }],
            ..Default::default()
        };
        assert!(matches!(
            validate_directory(&d),
            Err(DirectoryError::BadDigestLen(_, 31))
        ));
    }
}
