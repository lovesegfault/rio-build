//! Nix worker protocol handshake.
//!
//! Implements the server side of the version negotiation sequence
//! for protocol version 1.37+.

use tokio::io::{AsyncRead, AsyncWrite};

use super::wire::{self, WireError};

/// Client-to-server magic. Represents "nixc" zero-extended to u64.
///
/// Nix serializes all integers as u64 LE, including magic constants.
/// The constant 0x6e697863 is written as 8 bytes: `63 78 69 6e 00 00 00 00`.
pub const WORKER_MAGIC_1: u64 = 0x6e697863;

/// Server-to-client magic. Represents "dxio" zero-extended to u64.
pub const WORKER_MAGIC_2: u64 = 0x6478696f;

/// Our protocol version: 1.38 encoded as `(1 << 8) | 38`.
/// We advertise 1.38 to support the feature exchange added in that version.
pub const PROTOCOL_VERSION: u64 = 0x126;

/// Minimum protocol version we accept from clients.
pub const MIN_CLIENT_VERSION: u64 = 0x125; // 1.37

/// Result of a successful handshake.
#[derive(Debug)]
pub struct HandshakeResult {
    /// The protocol version the client presented.
    pub client_version: u64,
}

/// Errors specific to the handshake.
#[derive(Debug, thiserror::Error)]
pub enum HandshakeError {
    #[error("wire format error: {0}")]
    Wire(#[from] WireError),

    #[error("invalid client magic: expected {WORKER_MAGIC_1:#018x}, got {0:#018x}")]
    InvalidMagic(u64),

    #[error(
        "client protocol version {client_major}.{client_minor} is too old; rio-build requires 1.37+"
    )]
    VersionTooOld {
        client_major: u64,
        client_minor: u64,
    },
}

/// Perform the server-side handshake over an async stream.
///
/// Steps:
/// 1. Read `WORKER_MAGIC_1` (u32) from client
/// 2. Write `WORKER_MAGIC_2` (u32) to client
/// 3. Write our protocol version (u64)
/// 4. Read client protocol version (u64)
/// 5. Read obsolete CPU affinity (u64, discard)
/// 6. Read `reserveSpace` (u64, discard)
/// 7. Write version string (padded string)
/// 8. Write trusted status: 1 (u64)
///
/// On error, the caller should send `STDERR_ERROR` and close the connection.
pub async fn server_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    version_string: &str,
) -> Result<HandshakeResult, HandshakeError> {
    // Step 1: Read client magic (u64 — Nix serializes all ints as u64)
    tracing::debug!("handshake: waiting to read WORKER_MAGIC_1");
    let client_magic = wire::read_u64(stream).await?;
    tracing::debug!(
        client_magic = format!("{client_magic:#018x}"),
        "handshake: read client magic"
    );
    if client_magic != WORKER_MAGIC_1 {
        return Err(HandshakeError::InvalidMagic(client_magic));
    }

    // Step 2: Write server magic + protocol version (u64 each)
    // Must flush after writing so the client receives the response before
    // sending its version. The Nix client reads MAGIC_2 + version in one go.
    tracing::debug!("handshake: writing WORKER_MAGIC_2 + version");
    wire::write_u64(stream, WORKER_MAGIC_2).await?;
    wire::write_u64(stream, PROTOCOL_VERSION).await?;
    use tokio::io::AsyncWriteExt;
    stream.flush().await.map_err(wire::WireError::Io)?;

    // Step 4: Read client protocol version
    let client_version = wire::read_u64(stream).await?;
    if client_version < MIN_CLIENT_VERSION {
        let client_major = client_version >> 8;
        let client_minor = client_version & 0xFF;
        return Err(HandshakeError::VersionTooOld {
            client_major,
            client_minor,
        });
    }

    // Step 5: Read obsolete CPU affinity (discard)
    let _cpu_affinity = wire::read_u64(stream).await?;

    // Step 6: Read reserveSpace (discard)
    let _reserve_space = wire::read_u64(stream).await?;

    // Step 7-8: Write version string + trusted status, then flush
    wire::write_string(stream, version_string).await?;
    wire::write_u64(stream, 1).await?; // trusted = 1
    stream.flush().await.map_err(wire::WireError::Io)?;

    // Step 9: Feature exchange (protocol >= 1.38)
    // The negotiated version is min(client, server).
    let negotiated_version = client_version.min(PROTOCOL_VERSION);
    if negotiated_version >= encode_version(1, 38) {
        // Read client features (string set)
        let client_features = wire::read_strings(stream).await?;
        tracing::debug!(
            features = ?client_features,
            "handshake: client features"
        );

        // Write our features (empty set for now)
        let our_features: Vec<String> = vec![];
        wire::write_strings(stream, &our_features).await?;
        stream.flush().await.map_err(wire::WireError::Io)?;
    }

    Ok(HandshakeResult {
        client_version: negotiated_version,
    })
}

/// Decode a protocol version number into (major, minor).
pub fn decode_version(v: u64) -> (u64, u64) {
    (v >> 8, v & 0xFF)
}

/// Encode (major, minor) into a protocol version number.
pub fn encode_version(major: u64, minor: u64) -> u64 {
    (major << 8) | minor
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[tokio::test]
    async fn test_successful_handshake() {
        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let server_handle = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_stream);
            let mut stream = tokio::io::join(reader, writer);
            server_handshake(&mut stream, "rio-build 0.1.0").await
        });

        // Simulate client side of handshake
        let (reader, mut writer) = tokio::io::split(client_stream);
        let mut reader = tokio::io::BufReader::new(reader);

        // Client sends: MAGIC_1 (u64) + version (u64)
        wire::write_u64(&mut writer, WORKER_MAGIC_1).await.unwrap();
        wire::write_u64(&mut writer, PROTOCOL_VERSION)
            .await
            .unwrap();
        writer.flush().await.unwrap();

        // Client reads: MAGIC_2 (u64) + server_version (u64)
        let magic2 = wire::read_u64(&mut reader).await.unwrap();
        assert_eq!(magic2, WORKER_MAGIC_2);
        let server_version = wire::read_u64(&mut reader).await.unwrap();
        assert_eq!(server_version, PROTOCOL_VERSION);

        // Client sends: affinity (u64) + reserveSpace (u64)
        wire::write_u64(&mut writer, 0).await.unwrap(); // CPU affinity
        wire::write_u64(&mut writer, 0).await.unwrap(); // reserveSpace
        writer.flush().await.unwrap();

        // Client reads: version_string + trusted status
        let version_string = wire::read_string(&mut reader).await.unwrap();
        assert_eq!(version_string, "rio-build 0.1.0");
        let trusted = wire::read_u64(&mut reader).await.unwrap();
        assert_eq!(trusted, 1);

        // Protocol >= 1.38: feature exchange
        // Client sends features (empty set)
        let client_features: Vec<String> = vec![];
        wire::write_strings(&mut writer, &client_features)
            .await
            .unwrap();
        writer.flush().await.unwrap();

        // Client reads server features
        let server_features = wire::read_strings(&mut reader).await.unwrap();
        assert!(server_features.is_empty());

        drop(writer);
        let result = server_handle.await.unwrap().unwrap();
        assert_eq!(result.client_version, PROTOCOL_VERSION);
    }

    #[test]
    fn test_version_encoding() {
        assert_eq!(encode_version(1, 37), 0x125);
        assert_eq!(decode_version(0x125), (1, 37));
        assert_eq!(decode_version(0x120), (1, 32));
    }
}
