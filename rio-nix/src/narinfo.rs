//! Narinfo text format parser and generator.
//!
//! The narinfo format is used by Nix binary cache HTTP servers. Each `.narinfo`
//! file contains metadata about a store path:
//!
//! ```text
//! StorePath: /nix/store/abc...-hello
//! URL: nar/abc123.nar.zst
//! Compression: zstd
//! NarHash: sha256:abc123...
//! NarSize: 12345
//! References: dep1-hash-name dep2-hash-name
//! Deriver: xyz...-hello.drv
//! Sig: cache.example.com-1:base64sig...
//! CA: fixed:sha256:abc123...
//! ```
//!
//! Field order is not significant. `References` uses space-separated
//! basenames (not full store paths). Multiple `Sig:` lines are allowed.

use thiserror::Error;

/// Errors from narinfo parsing.
#[derive(Debug, Error)]
pub enum NarInfoError {
    #[error("missing required field: {0}")]
    MissingField(&'static str),

    #[error("invalid NarSize value: {0}")]
    InvalidNarSize(String),

    #[error("invalid FileSize value: {0}")]
    InvalidFileSize(String),

    #[error("duplicate field: {0}")]
    DuplicateField(String),
}

/// Parsed narinfo metadata.
///
/// All fields use raw string values as they appear in the narinfo text.
/// Hash parsing and store path validation are deferred to the caller.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NarInfo {
    /// Full store path (e.g., `/nix/store/abc...-hello`).
    store_path: String,
    /// URL to the NAR file (e.g., `nar/abc123.nar.zst`).
    url: String,
    /// Compression method (e.g., `zstd`, `xz`, `none`).
    compression: String,
    /// NAR hash (e.g., `sha256:abc123...`).
    nar_hash: String,
    /// NAR size in bytes.
    nar_size: u64,
    /// Referenced store path basenames, space-separated in text form.
    references: Vec<String>,
    /// Deriver basename (e.g., `xyz...-hello.drv`).
    deriver: Option<String>,
    /// Cryptographic signatures.
    sigs: Vec<String>,
    /// Content address (e.g., `fixed:sha256:abc...`).
    ca: Option<String>,
    /// Compressed file hash (e.g., `sha256:def...`).
    file_hash: Option<String>,
    /// Compressed file size in bytes.
    file_size: Option<u64>,
}

impl NarInfo {
    /// Parse a narinfo text blob.
    pub fn parse(text: &str) -> Result<Self, NarInfoError> {
        let mut store_path = None;
        let mut url = None;
        let mut compression = None;
        let mut nar_hash = None;
        let mut nar_size = None;
        let mut references = Vec::new();
        let mut deriver = None;
        let mut sigs = Vec::new();
        let mut ca = None;
        let mut file_hash = None;
        let mut file_size = None;
        let mut refs_seen = false;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }

            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            let value = value.trim();

            match key {
                "StorePath" => {
                    if store_path.is_some() {
                        return Err(NarInfoError::DuplicateField("StorePath".to_string()));
                    }
                    store_path = Some(value.to_string());
                }
                "URL" => {
                    if url.is_some() {
                        return Err(NarInfoError::DuplicateField("URL".to_string()));
                    }
                    url = Some(value.to_string());
                }
                "Compression" => {
                    if compression.is_some() {
                        return Err(NarInfoError::DuplicateField("Compression".to_string()));
                    }
                    compression = Some(value.to_string());
                }
                "NarHash" => {
                    if nar_hash.is_some() {
                        return Err(NarInfoError::DuplicateField("NarHash".to_string()));
                    }
                    nar_hash = Some(value.to_string());
                }
                "NarSize" => {
                    if nar_size.is_some() {
                        return Err(NarInfoError::DuplicateField("NarSize".to_string()));
                    }
                    nar_size = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| NarInfoError::InvalidNarSize(value.to_string()))?,
                    );
                }
                "References" => {
                    if refs_seen {
                        return Err(NarInfoError::DuplicateField("References".to_string()));
                    }
                    refs_seen = true;
                    if !value.is_empty() {
                        references = value.split_whitespace().map(String::from).collect();
                    }
                }
                "Deriver" => {
                    if deriver.is_some() {
                        return Err(NarInfoError::DuplicateField("Deriver".to_string()));
                    }
                    deriver = Some(value.to_string());
                }
                "Sig" => sigs.push(value.to_string()),
                "CA" => {
                    if ca.is_some() {
                        return Err(NarInfoError::DuplicateField("CA".to_string()));
                    }
                    ca = Some(value.to_string());
                }
                "FileHash" => {
                    if file_hash.is_some() {
                        return Err(NarInfoError::DuplicateField("FileHash".to_string()));
                    }
                    file_hash = Some(value.to_string());
                }
                "FileSize" => {
                    if file_size.is_some() {
                        return Err(NarInfoError::DuplicateField("FileSize".to_string()));
                    }
                    file_size = Some(
                        value
                            .parse::<u64>()
                            .map_err(|_| NarInfoError::InvalidFileSize(value.to_string()))?,
                    );
                }
                _ => {} // Ignore unknown fields for forward compatibility
            }
        }

        Ok(NarInfo {
            store_path: store_path.ok_or(NarInfoError::MissingField("StorePath"))?,
            url: url.ok_or(NarInfoError::MissingField("URL"))?,
            compression: compression.ok_or(NarInfoError::MissingField("Compression"))?,
            nar_hash: nar_hash.ok_or(NarInfoError::MissingField("NarHash"))?,
            nar_size: nar_size.ok_or(NarInfoError::MissingField("NarSize"))?,
            references,
            deriver,
            sigs,
            ca,
            file_hash,
            file_size,
        })
    }

    /// Serialize to narinfo text format.
    pub fn serialize(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();

        writeln!(out, "StorePath: {}", self.store_path).unwrap();
        writeln!(out, "URL: {}", self.url).unwrap();
        writeln!(out, "Compression: {}", self.compression).unwrap();
        if let Some(ref fh) = self.file_hash {
            writeln!(out, "FileHash: {fh}").unwrap();
        }
        if let Some(fs) = self.file_size {
            writeln!(out, "FileSize: {fs}").unwrap();
        }
        writeln!(out, "NarHash: {}", self.nar_hash).unwrap();
        writeln!(out, "NarSize: {}", self.nar_size).unwrap();

        if self.references.is_empty() {
            writeln!(out, "References:").unwrap();
        } else {
            writeln!(out, "References: {}", self.references.join(" ")).unwrap();
        }

        if let Some(ref d) = self.deriver {
            writeln!(out, "Deriver: {d}").unwrap();
        }

        for sig in &self.sigs {
            writeln!(out, "Sig: {sig}").unwrap();
        }

        if let Some(ref c) = self.ca {
            writeln!(out, "CA: {c}").unwrap();
        }

        out
    }

    /// The full store path.
    pub fn store_path(&self) -> &str {
        &self.store_path
    }

    /// URL to the NAR file.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// Compression method.
    pub fn compression(&self) -> &str {
        &self.compression
    }

    /// NAR hash string (e.g., `sha256:abc...`).
    pub fn nar_hash(&self) -> &str {
        &self.nar_hash
    }

    /// NAR size in bytes.
    pub fn nar_size(&self) -> u64 {
        self.nar_size
    }

    /// Referenced store path basenames.
    pub fn references(&self) -> &[String] {
        &self.references
    }

    /// Deriver basename.
    pub fn deriver(&self) -> Option<&str> {
        self.deriver.as_deref()
    }

    /// Cryptographic signatures.
    pub fn sigs(&self) -> &[String] {
        &self.sigs
    }

    /// Content address.
    pub fn ca(&self) -> Option<&str> {
        self.ca.as_deref()
    }

    /// Compressed file hash.
    pub fn file_hash(&self) -> Option<&str> {
        self.file_hash.as_deref()
    }

    /// Compressed file size in bytes.
    pub fn file_size(&self) -> Option<u64> {
        self.file_size
    }
}

/// Builder for constructing [`NarInfo`] values.
pub struct NarInfoBuilder {
    store_path: String,
    url: String,
    compression: String,
    nar_hash: String,
    nar_size: u64,
    references: Vec<String>,
    deriver: Option<String>,
    sigs: Vec<String>,
    ca: Option<String>,
    file_hash: Option<String>,
    file_size: Option<u64>,
}

impl NarInfoBuilder {
    /// Create a builder with the required fields.
    pub fn new(
        store_path: impl Into<String>,
        url: impl Into<String>,
        compression: impl Into<String>,
        nar_hash: impl Into<String>,
        nar_size: u64,
    ) -> Self {
        NarInfoBuilder {
            store_path: store_path.into(),
            url: url.into(),
            compression: compression.into(),
            nar_hash: nar_hash.into(),
            nar_size,
            references: Vec::new(),
            deriver: None,
            sigs: Vec::new(),
            ca: None,
            file_hash: None,
            file_size: None,
        }
    }

    /// Set the references (store path basenames).
    pub fn references(mut self, refs: Vec<String>) -> Self {
        self.references = refs;
        self
    }

    /// Set the deriver basename.
    pub fn deriver(mut self, deriver: impl Into<String>) -> Self {
        self.deriver = Some(deriver.into());
        self
    }

    /// Add a signature.
    pub fn sig(mut self, sig: impl Into<String>) -> Self {
        self.sigs.push(sig.into());
        self
    }

    /// Set the content address.
    pub fn ca(mut self, ca: impl Into<String>) -> Self {
        self.ca = Some(ca.into());
        self
    }

    /// Set the compressed file hash.
    pub fn file_hash(mut self, file_hash: impl Into<String>) -> Self {
        self.file_hash = Some(file_hash.into());
        self
    }

    /// Set the compressed file size.
    pub fn file_size(mut self, file_size: u64) -> Self {
        self.file_size = Some(file_size);
        self
    }

    /// Build the [`NarInfo`], validating that required fields are non-empty.
    pub fn build(self) -> Result<NarInfo, NarInfoError> {
        if self.store_path.is_empty() {
            return Err(NarInfoError::MissingField("StorePath"));
        }
        if self.url.is_empty() {
            return Err(NarInfoError::MissingField("URL"));
        }
        if self.compression.is_empty() {
            return Err(NarInfoError::MissingField("Compression"));
        }
        if self.nar_hash.is_empty() {
            return Err(NarInfoError::MissingField("NarHash"));
        }
        Ok(NarInfo {
            store_path: self.store_path,
            url: self.url,
            compression: self.compression,
            nar_hash: self.nar_hash,
            nar_size: self.nar_size,
            references: self.references,
            deriver: self.deriver,
            sigs: self.sigs,
            ca: self.ca,
            file_hash: self.file_hash,
            file_size: self.file_size,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_NARINFO: &str = "\
StorePath: /nix/store/mi08jhbcjib1i1kgvbd0fxn2yrnzdv4a-hello-2.12.2
URL: nar/0abc123.nar.zst
Compression: zstd
NarHash: sha256:8ff54500fb829867e5aee6ec054f5ccd5266670b0bf6f491b5276269dc9b358c
NarSize: 274640
References: mi08jhbcjib1i1kgvbd0fxn2yrnzdv4a-hello-2.12.2 wb6rhpznjfczwlwx23zmdrrw74bayxw4-glibc-2.42-47
Deriver: c7xq1hry7mx5rxr1vqwf7dhphpd7hl29-hello-2.12.2.drv
Sig: cache.nixos.org-1:0N1BDA7KG/Bu1LM0gJmmq5Ns0Ad+3kaE8VrWF0T9qnCGm/s+fuxRHXeXERTc8vl/uUmOyXqdssbkieous4RNCg==
";

    #[test]
    fn parse_full_narinfo() {
        let info = NarInfo::parse(SAMPLE_NARINFO).unwrap();

        assert_eq!(
            info.store_path(),
            "/nix/store/mi08jhbcjib1i1kgvbd0fxn2yrnzdv4a-hello-2.12.2"
        );
        assert_eq!(info.url(), "nar/0abc123.nar.zst");
        assert_eq!(info.compression(), "zstd");
        assert_eq!(
            info.nar_hash(),
            "sha256:8ff54500fb829867e5aee6ec054f5ccd5266670b0bf6f491b5276269dc9b358c"
        );
        assert_eq!(info.nar_size(), 274640);
        assert_eq!(info.references().len(), 2);
        assert_eq!(
            info.references()[0],
            "mi08jhbcjib1i1kgvbd0fxn2yrnzdv4a-hello-2.12.2"
        );
        assert_eq!(
            info.references()[1],
            "wb6rhpznjfczwlwx23zmdrrw74bayxw4-glibc-2.42-47"
        );
        assert_eq!(
            info.deriver(),
            Some("c7xq1hry7mx5rxr1vqwf7dhphpd7hl29-hello-2.12.2.drv")
        );
        assert_eq!(info.sigs().len(), 1);
        assert!(info.sigs()[0].starts_with("cache.nixos.org-1:"));
        assert!(info.ca().is_none());
    }

    #[test]
    fn roundtrip() {
        let info = NarInfo::parse(SAMPLE_NARINFO).unwrap();
        let serialized = info.serialize();
        let reparsed = NarInfo::parse(&serialized).unwrap();
        assert_eq!(info, reparsed);
    }

    #[test]
    fn parse_minimal() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
";
        let info = NarInfo::parse(text).unwrap();
        assert_eq!(info.store_path(), "/nix/store/abc-test");
        assert!(info.references().is_empty());
        assert!(info.deriver().is_none());
        assert!(info.sigs().is_empty());
        assert!(info.ca().is_none());
    }

    #[test]
    fn parse_with_ca() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
CA: fixed:sha256:abcdef
";
        let info = NarInfo::parse(text).unwrap();
        assert_eq!(info.ca(), Some("fixed:sha256:abcdef"));
    }

    #[test]
    fn parse_multiple_sigs() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
Sig: key1:sig1
Sig: key2:sig2
";
        let info = NarInfo::parse(text).unwrap();
        assert_eq!(info.sigs().len(), 2);
        assert_eq!(info.sigs()[0], "key1:sig1");
        assert_eq!(info.sigs()[1], "key2:sig2");
    }

    #[test]
    fn parse_with_file_hash_and_size() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
FileHash: sha256:deadbeef
FileSize: 5000
NarHash: sha256:0000
NarSize: 10000
References:
";
        let info = NarInfo::parse(text).unwrap();
        assert_eq!(info.file_hash(), Some("sha256:deadbeef"));
        assert_eq!(info.file_size(), Some(5000));
    }

    #[test]
    fn missing_store_path() {
        let text = "\
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::MissingField("StorePath"))
        ));
    }

    #[test]
    fn missing_nar_hash() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarSize: 100
References:
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::MissingField("NarHash"))
        ));
    }

    #[test]
    fn invalid_nar_size() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: not_a_number
References:
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::InvalidNarSize(_))
        ));
    }

    #[test]
    fn duplicate_field() {
        let text = "\
StorePath: /nix/store/abc-test
StorePath: /nix/store/def-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::DuplicateField(_))
        ));
    }

    #[test]
    fn duplicate_deriver() {
        let text = "\
StorePath: /nix/store/abc-test
URL: nar/abc.nar.zst
Compression: zstd
NarHash: sha256:0000
NarSize: 100
References:
Deriver: first.drv
Deriver: second.drv
";
        assert!(matches!(
            NarInfo::parse(text),
            Err(NarInfoError::DuplicateField(_))
        ));
    }

    #[test]
    fn builder_constructs_valid_narinfo() {
        let info = NarInfoBuilder::new(
            "/nix/store/abc-test",
            "nar/abc.nar.zst",
            "zstd",
            "sha256:0000",
            100,
        )
        .references(vec!["abc-dep".to_string()])
        .deriver("xyz-test.drv")
        .sig("key:sig")
        .ca("fixed:sha256:beef")
        .build()
        .unwrap();

        assert_eq!(info.store_path(), "/nix/store/abc-test");
        assert_eq!(info.references(), &["abc-dep"]);
        assert_eq!(info.deriver(), Some("xyz-test.drv"));
        assert_eq!(info.sigs(), &["key:sig"]);
        assert_eq!(info.ca(), Some("fixed:sha256:beef"));

        // Verify it roundtrips
        let serialized = info.serialize();
        let reparsed = NarInfo::parse(&serialized).unwrap();
        assert_eq!(info, reparsed);
    }

    #[test]
    fn serialize_omits_empty_optional_fields() {
        let info = NarInfoBuilder::new(
            "/nix/store/abc-test",
            "nar/abc.nar.zst",
            "none",
            "sha256:0000",
            100,
        )
        .build()
        .unwrap();

        let text = info.serialize();
        assert!(!text.contains("Deriver:"));
        assert!(!text.contains("Sig:"));
        assert!(!text.contains("CA:"));
        assert!(!text.contains("FileHash:"));
        assert!(!text.contains("FileSize:"));
    }
}
