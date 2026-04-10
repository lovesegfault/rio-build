//! `ValidPathInfo` wire encoding (Nix worker protocol ≥ 1.16).
//!
//! Shared by `wopQueryPathInfo` (server → client, this struct alone) and
//! `wopAddMultipleToStore` / `wopAddToStoreNar` (client → server, prefixed
//! with the store path it describes). The 8-field body is identical in both
//! directions; only the surrounding framing differs.
//!
//! Field order matches Nix C++ `WorkerProto::Serialise<ValidPathInfo>`
//! (`worker-protocol.cc`):
//! `deriver, narHash, references, registrationTime, narSize, ultimate,
//! sigs, ca`.

use tokio::io::{AsyncRead, AsyncWrite};

use super::wire::{
    Result, read_bool, read_nar_hash, read_string, read_strings, read_u64, write_bool,
    write_nar_hash, write_string, write_strings, write_u64,
};

/// Wire representation of Nix's `ValidPathInfo` (without the leading
/// store-path field, which the caller frames separately).
///
/// `deriver` and `content_address` use the wire convention "empty string =
/// absent"; the struct keeps them as `Option` so callers don't have to
/// remember the sentinel.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ValidPathInfo {
    /// Store path of the `.drv` that produced this path.
    pub deriver: Option<String>,
    /// SHA-256 of the path's NAR serialization (raw 32 bytes).
    pub nar_hash: Vec<u8>,
    /// Direct reference closure (full store paths).
    pub references: Vec<String>,
    /// Unix epoch seconds.
    pub registration_time: u64,
    /// NAR byte length.
    pub nar_size: u64,
    /// "Ultimately trusted" — built locally on a trusted machine.
    pub ultimate: bool,
    /// `<keyname>:<base64>` ed25519 signatures over the narinfo fingerprint.
    pub signatures: Vec<String>,
    /// `fixed:r:sha256:...` / `text:sha256:...` content-address descriptor.
    pub content_address: Option<String>,
}

/// Write the 8-field `ValidPathInfo` body.
pub async fn write_valid_path_info<W: AsyncWrite + Unpin>(
    w: &mut W,
    info: &ValidPathInfo,
) -> Result<()> {
    write_string(w, info.deriver.as_deref().unwrap_or("")).await?;
    write_nar_hash(w, &info.nar_hash).await?;
    write_strings(w, &info.references).await?;
    write_u64(w, info.registration_time).await?;
    write_u64(w, info.nar_size).await?;
    write_bool(w, info.ultimate).await?;
    write_strings(w, &info.signatures).await?;
    write_string(w, info.content_address.as_deref().unwrap_or("")).await?;
    Ok(())
}

/// Read the 8-field `ValidPathInfo` body.
pub async fn read_valid_path_info<R: AsyncRead + Unpin>(r: &mut R) -> Result<ValidPathInfo> {
    let deriver = none_if_empty(read_string(r).await?);
    let nar_hash = read_nar_hash(r).await?;
    let references = read_strings(r).await?;
    let registration_time = read_u64(r).await?;
    let nar_size = read_u64(r).await?;
    let ultimate = read_bool(r).await?;
    let signatures = read_strings(r).await?;
    let content_address = none_if_empty(read_string(r).await?);
    Ok(ValidPathInfo {
        deriver,
        nar_hash,
        references,
        registration_time,
        nar_size,
        ultimate,
        signatures,
        content_address,
    })
}

fn none_if_empty(s: String) -> Option<String> {
    if s.is_empty() { None } else { Some(s) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample() -> ValidPathInfo {
        ValidPathInfo {
            deriver: Some("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello.drv".into()),
            nar_hash: vec![0xab; 32],
            references: vec![
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-glibc".into(),
                "/nix/store/cccccccccccccccccccccccccccccccc-hello".into(),
            ],
            registration_time: 1_700_000_000,
            nar_size: 12345,
            ultimate: true,
            signatures: vec!["cache.example.org-1:AAAA".into()],
            content_address: None,
        }
    }

    #[tokio::test]
    async fn roundtrip() -> anyhow::Result<()> {
        let info = sample();
        let mut buf = Vec::new();
        write_valid_path_info(&mut buf, &info).await?;
        let parsed = read_valid_path_info(&mut Cursor::new(&buf)).await?;
        assert_eq!(parsed, info);
        Ok(())
    }

    #[tokio::test]
    async fn empty_maps_to_none() -> anyhow::Result<()> {
        let info = ValidPathInfo {
            nar_hash: vec![0u8; 32],
            ..Default::default()
        };
        let mut buf = Vec::new();
        write_valid_path_info(&mut buf, &info).await?;
        let parsed = read_valid_path_info(&mut Cursor::new(&buf)).await?;
        assert_eq!(parsed.deriver, None);
        assert_eq!(parsed.content_address, None);
        assert_eq!(parsed, info);
        Ok(())
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        fn arb_info() -> impl Strategy<Value = ValidPathInfo> {
            (
                proptest::option::of("[a-z/.-]{1,80}"),
                proptest::collection::vec(any::<u8>(), 32..=32),
                proptest::collection::vec("[a-z/.-]{1,80}", 0..4),
                any::<u64>(),
                any::<u64>(),
                any::<bool>(),
                proptest::collection::vec("[A-Za-z0-9:+/=.-]{1,80}", 0..3),
                proptest::option::of("[a-z0-9:.-]{1,60}"),
            )
                .prop_map(|(d, h, r, t, n, u, s, c)| ValidPathInfo {
                    deriver: d,
                    nar_hash: h,
                    references: r,
                    registration_time: t,
                    nar_size: n,
                    ultimate: u,
                    signatures: s,
                    content_address: c,
                })
        }

        proptest! {
            #![proptest_config(ProptestConfig::with_cases(512))]
            #[test]
            fn roundtrip_prop(info in arb_info()) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .build()
                    .unwrap();
                rt.block_on(async {
                    let mut buf = Vec::new();
                    write_valid_path_info(&mut buf, &info).await.unwrap();
                    let parsed = read_valid_path_info(&mut Cursor::new(&buf)).await.unwrap();
                    prop_assert_eq!(parsed, info);
                    Ok(())
                })?;
            }
        }
    }
}
