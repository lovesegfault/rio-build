//! Archive identity: `archive_id`, the short id, and the canonical listing
//! digests over embedded derivations, embedded store paths, and narinfo
//! sidecars.
//!
//! `archive_id` is the lowercase hex SHA-256 of the bytes of
//! `manifest.json` exactly as stored in the archive; because the manifest
//! embeds per-member digests (`files`) and aggregate content digests
//! (`content_digests`), the id is a Merkle-style content address over the
//! whole archive. The identity is independent of the container: the
//! directory form and the image form of the same archive have the same id.

use sha2::Digest as _;

/// Digest of an empty canonical listing (the SHA-256 of the empty string).
pub const EMPTY_LISTING_DIGEST: &str =
    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// Lowercase hex SHA-256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(sha2::Sha256::digest(bytes))
}

/// `archive_id`: lowercase hex SHA-256 of the manifest.json bytes exactly
/// as stored in the archive.
pub fn archive_id_from_manifest_bytes(manifest_bytes: &[u8]) -> String {
    sha256_hex(manifest_bytes)
}

/// The short id: the first 16 hex characters of a full archive id (used in
/// S3 prefixes, campaign pins, and operator output). The input must be a
/// full 64-hex archive id, never an already-shortened one.
pub fn short_id(archive_id: &str) -> String {
    debug_assert!(
        archive_id.len() == 64 && archive_id.bytes().all(|b| b.is_ascii_hexdigit()),
        "short_id expects a full 64-hex archive id, got {archive_id:?}"
    );
    archive_id.chars().take(16).collect()
}

/// Canonical listing digest: one line per entry, `"<store path> <digest>"`,
/// sorted lexicographically by store path, joined with `"\n"`, with a
/// trailing newline; the digest of an empty listing is the SHA-256 of the
/// empty string. Entry order in the input does not matter. The line format
/// needs no escaping because entries are store paths and lowercase hex
/// digests, neither of which can contain a space or a newline.
pub fn listing_digest(entries: &[(String, String)]) -> String {
    if entries.is_empty() {
        return EMPTY_LISTING_DIGEST.to_string();
    }
    let mut sorted: Vec<&(String, String)> = entries.iter().collect();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    let mut listing = String::new();
    for (path, digest) in sorted {
        listing.push_str(path);
        listing.push(' ');
        listing.push_str(digest);
        listing.push('\n');
    }
    sha256_hex(listing.as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_listing_digest_is_sha256_of_the_empty_string() {
        assert_eq!(
            listing_digest(&[]),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(listing_digest(&[]), EMPTY_LISTING_DIGEST);
    }

    #[test]
    fn listing_digest_is_order_independent_and_content_sensitive() {
        let a = (
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a".to_string(),
            "1".repeat(64),
        );
        let b = (
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b".to_string(),
            "2".repeat(64),
        );
        let forward = listing_digest(&[a.clone(), b.clone()]);
        let reversed = listing_digest(&[b.clone(), a.clone()]);
        assert_eq!(forward, reversed);

        // Format pinning: the digest is the SHA-256 of the literal
        // "<path> <digest>\n" line per entry in sorted order, so the
        // canonical encoding cannot drift silently.
        let pinned = sha256_hex(format!("{} {}\n{} {}\n", a.0, a.1, b.0, b.1).as_bytes());
        assert_eq!(forward, pinned);

        // Changing a single character of one per-entry digest changes the
        // listing digest.
        let mut tampered = a.clone();
        tampered.1.replace_range(0..1, "f");
        assert_ne!(listing_digest(&[tampered, b.clone()]), forward);

        // Adding an entry changes the listing digest.
        let c = (
            "/nix/store/cccccccccccccccccccccccccccccccc-c".to_string(),
            "3".repeat(64),
        );
        assert_ne!(listing_digest(&[a, b, c]), forward);
    }

    #[test]
    fn archive_id_and_short_id() {
        let id = archive_id_from_manifest_bytes(b"{}");
        assert_eq!(id.len(), 64);
        assert!(
            id.chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()),
            "got: {id}"
        );

        let short = short_id(&id);
        assert_eq!(short.len(), 16);
        assert_eq!(short, id[..16]);
        assert!(id.starts_with(&short));

        assert_ne!(archive_id_from_manifest_bytes(b"{ }"), id);
    }

    #[test]
    fn sha256_hex_known_vector() {
        assert_eq!(sha256_hex(b""), EMPTY_LISTING_DIGEST);
    }
}
