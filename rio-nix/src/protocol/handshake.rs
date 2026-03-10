//! Nix worker protocol handshake.
//!
//! Implements the server side of the version negotiation sequence
//! for protocol version 1.37+.

use tokio::io::{AsyncRead, AsyncWrite};

use super::wire::{self, WireError};

// r[impl gw.handshake.magic]
// r[impl gw.wire.all-ints-u64]
/// Client-to-server magic. Represents "nixc" zero-extended to u64.
///
/// Nix serializes all integers as u64 LE, including magic constants.
/// The constant 0x6e697863 is written as 8 bytes: `63 78 69 6e 00 00 00 00`.
pub const WORKER_MAGIC_1: u64 = 0x6e697863;

/// Server-to-client magic. Represents "dxio" zero-extended to u64.
pub const WORKER_MAGIC_2: u64 = 0x6478696f;

// r[impl gw.compat.version-range]
/// Our protocol version: 1.38 encoded as `(1 << 8) | 38`.
/// We advertise 1.38 to support the feature exchange added in that version.
pub const PROTOCOL_VERSION: u64 = 0x126;

/// Minimum protocol version we accept from clients.
pub const MIN_CLIENT_VERSION: u64 = 0x125; // 1.37

/// Result of a successful handshake.
#[must_use]
#[non_exhaustive]
#[derive(Debug, PartialEq, Eq)]
pub struct HandshakeResult {
    /// The negotiated protocol version: `min(client_version, server_version)`.
    negotiated_version: u64,
}

impl HandshakeResult {
    /// Create a new handshake result (crate-internal).
    pub(crate) fn new(negotiated_version: u64) -> Self {
        HandshakeResult { negotiated_version }
    }

    /// The negotiated protocol version: `min(client_version, server_version)`.
    pub fn negotiated_version(&self) -> u64 {
        self.negotiated_version
    }
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
/// All values are u64 LE on the wire (including magic bytes).
///
/// Phase 1 — Magic + version exchange:
/// 1. Read `WORKER_MAGIC_1` (u64) from client
/// 2. Write `WORKER_MAGIC_2` (u64) + protocol version (u64) to client; flush
/// 3. Read client protocol version (u64)
///
/// Phase 2 — Feature exchange (protocol >= 1.38):
/// 4. Read client features (string collection)
/// 5. Write server features (string collection); flush
///
/// Phase 3 — Post-handshake:
/// 6. Read obsolete CPU affinity (u64, if non-zero read mask u64)
/// 7. Read `reserveSpace` (u64, discard)
/// 8. Write version string (padded string) + trusted status (u64); flush
///
/// Phase 4 — Initial STDERR_LAST:
/// 9. Write STDERR_LAST (u64); flush
///
/// On error, the caller should send `STDERR_ERROR` and close the connection.
#[cfg(test)]
pub async fn server_handshake<S: AsyncRead + AsyncWrite + Unpin>(
    stream: &mut S,
    version_string: &str,
) -> Result<HandshakeResult, HandshakeError> {
    let (mut reader, mut writer) = tokio::io::split(stream);
    server_handshake_split(&mut reader, &mut writer, version_string).await
}

/// Server-side handshake with separate reader and writer streams.
///
/// Same as `server_handshake` but takes split read/write halves. This is
/// needed when the reader and writer are independent streams (e.g., two
/// ends of separate pipes bridging SSH channel I/O).
// r[impl gw.handshake.phases]
// r[impl gw.handshake.magic]
// r[impl gw.handshake.version-negotiation]
// r[impl gw.handshake.features]
// r[impl gw.handshake.initial-stderr-last]
// r[impl gw.handshake.flush-points]
#[tracing::instrument(name = "handshake", skip_all)]
pub async fn server_handshake_split<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    version_string: &str,
) -> Result<HandshakeResult, HandshakeError> {
    use tokio::io::AsyncWriteExt;

    // Phase 1: Magic + version exchange (BasicClientConnection::handshake)
    let client_magic = wire::read_u64(reader).await?;
    if client_magic != WORKER_MAGIC_1 {
        return Err(HandshakeError::InvalidMagic(client_magic));
    }

    wire::write_u64(writer, WORKER_MAGIC_2).await?;
    wire::write_u64(writer, PROTOCOL_VERSION).await?;
    writer.flush().await.map_err(wire::WireError::Io)?;

    let client_version = wire::read_u64(reader).await?;
    if client_version < MIN_CLIENT_VERSION {
        let client_major = client_version >> 8;
        let client_minor = client_version & 0xFF;
        return Err(HandshakeError::VersionTooOld {
            client_major,
            client_minor,
        });
    }

    let negotiated_version = client_version.min(PROTOCOL_VERSION);

    // Phase 2: Feature exchange (protocol >= 1.38)
    if negotiated_version >= encode_version(1, 38) {
        let _client_features = wire::read_strings(reader).await?;
        wire::write_strings(writer, wire::NO_STRINGS).await?;
        writer.flush().await.map_err(wire::WireError::Io)?;
    }

    // Phase 3: Post-handshake (BasicServerConnection::postHandshake)
    // Client sends obsolete CPU affinity
    let cpu_affinity = wire::read_u64(reader).await?;
    if cpu_affinity != 0 {
        // If non-zero, there's a second u64 (the actual affinity mask) — read and discard
        let _affinity_mask = wire::read_u64(reader).await?;
    }
    // Client sends obsolete reserveSpace
    let _reserve_space = wire::read_u64(reader).await?;

    // Server sends ClientHandshakeInfo: version string + trusted status
    wire::write_string(writer, version_string).await?;
    wire::write_u64(writer, 1).await?; // trusted = 1
    writer.flush().await.map_err(wire::WireError::Io)?;

    // Phase 4: Client calls processStderrReturn — send STDERR_LAST
    wire::write_u64(writer, super::stderr::STDERR_LAST).await?;
    writer.flush().await.map_err(wire::WireError::Io)?;

    Ok(HandshakeResult::new(negotiated_version))
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
    async fn test_successful_handshake() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let server_handle = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_stream);
            let mut stream = tokio::io::join(reader, writer);
            server_handshake(&mut stream, "rio-build 0.1.0").await
        });

        // Simulate client side of handshake
        let (reader, mut writer) = tokio::io::split(client_stream);
        let mut reader = tokio::io::BufReader::new(reader);

        // Step 1: Client sends MAGIC_1 + version
        wire::write_u64(&mut writer, WORKER_MAGIC_1).await?;
        wire::write_u64(&mut writer, PROTOCOL_VERSION).await?;
        writer.flush().await?;

        // Step 2: Client reads MAGIC_2 + server_version
        let magic2 = wire::read_u64(&mut reader).await?;
        assert_eq!(magic2, WORKER_MAGIC_2);
        let server_version = wire::read_u64(&mut reader).await?;
        assert_eq!(server_version, PROTOCOL_VERSION);

        // Step 3 (>= 1.38): Client sends features
        wire::write_strings(&mut writer, wire::NO_STRINGS).await?;
        writer.flush().await?;

        // Step 4: Client reads server features
        let server_features = wire::read_strings(&mut reader).await?;
        assert!(server_features.is_empty());

        // Post-handshake: client sends affinity + reserveSpace
        wire::write_u64(&mut writer, 0).await?; // CPU affinity = 0
        wire::write_u64(&mut writer, 0).await?; // reserveSpace = false
        writer.flush().await?;

        // Client reads version string + trusted + STDERR_LAST
        let version_str = wire::read_string(&mut reader).await?;
        assert!(version_str.contains("rio-build"));
        let trusted = wire::read_u64(&mut reader).await?;
        assert_eq!(trusted, 1);
        let stderr_last = wire::read_u64(&mut reader).await?;
        assert_eq!(stderr_last, crate::protocol::stderr::STDERR_LAST);

        drop(writer);
        let result = server_handle.await??;
        assert_eq!(result.negotiated_version(), PROTOCOL_VERSION);
        Ok(())
    }

    #[test]
    fn test_version_encoding() {
        assert_eq!(encode_version(1, 37), 0x125);
        assert_eq!(decode_version(0x125), (1, 37));
        assert_eq!(decode_version(0x120), (1, 32));
    }

    #[tokio::test]
    async fn test_invalid_magic_rejected() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let server_handle = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_stream);
            let mut stream = tokio::io::join(reader, writer);
            server_handshake(&mut stream, "rio-build 0.1.0").await
        });

        let (_, mut writer) = tokio::io::split(client_stream);
        // Send wrong magic
        wire::write_u64(&mut writer, 0xDEADBEEF).await?;
        writer.flush().await?;

        let result = server_handle.await?;
        assert!(matches!(
            result,
            Err(HandshakeError::InvalidMagic(0xDEADBEEF))
        ));
        Ok(())
    }

    #[tokio::test]
    async fn test_version_too_old_rejected() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let server_handle = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_stream);
            let mut stream = tokio::io::join(reader, writer);
            server_handshake(&mut stream, "rio-build 0.1.0").await
        });

        let (reader, mut writer) = tokio::io::split(client_stream);
        let mut reader = tokio::io::BufReader::new(reader);

        // Send correct magic
        wire::write_u64(&mut writer, WORKER_MAGIC_1).await?;
        // Send old version (1.32)
        wire::write_u64(&mut writer, encode_version(1, 32)).await?;
        writer.flush().await?;

        // Read server's response (MAGIC_2 + version)
        let _magic2 = wire::read_u64(&mut reader).await?;
        let _server_version = wire::read_u64(&mut reader).await?;

        drop(writer);
        let result = server_handle.await?;
        assert!(matches!(
            result,
            Err(HandshakeError::VersionTooOld {
                client_major: 1,
                client_minor: 32,
            })
        ));
        Ok(())
    }

    #[tokio::test]
    async fn test_handshake_with_nonzero_cpu_affinity() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let server_handle = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_stream);
            let mut stream = tokio::io::join(reader, writer);
            server_handshake(&mut stream, "rio-build 0.1.0").await
        });

        let (reader, mut writer) = tokio::io::split(client_stream);
        let mut reader = tokio::io::BufReader::new(reader);

        // Magic + version
        wire::write_u64(&mut writer, WORKER_MAGIC_1).await?;
        wire::write_u64(&mut writer, PROTOCOL_VERSION).await?;
        writer.flush().await?;

        // Read server magic + version
        let _magic2 = wire::read_u64(&mut reader).await?;
        let _server_version = wire::read_u64(&mut reader).await?;

        // Feature exchange
        wire::write_strings(&mut writer, wire::NO_STRINGS).await?;
        writer.flush().await?;
        let _server_features = wire::read_strings(&mut reader).await?;

        // Post-handshake: send NON-ZERO CPU affinity + mask
        wire::write_u64(&mut writer, 1).await?; // cpu_affinity = 1 (non-zero)
        wire::write_u64(&mut writer, 0xFF).await?; // affinity mask
        wire::write_u64(&mut writer, 0).await?; // reserveSpace
        writer.flush().await?;

        // Read version string + trusted + STDERR_LAST
        let _version_str = wire::read_string(&mut reader).await?;
        let _trusted = wire::read_u64(&mut reader).await?;
        let _stderr_last = wire::read_u64(&mut reader).await?;

        drop(writer);
        let result = server_handle.await??;
        assert_eq!(result.negotiated_version(), PROTOCOL_VERSION);
        Ok(())
    }

    #[tokio::test]
    async fn test_handshake_v1_37_skips_feature_exchange() -> anyhow::Result<()> {
        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let server_handle = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_stream);
            let mut stream = tokio::io::join(reader, writer);
            server_handshake(&mut stream, "rio-build 0.1.0").await
        });

        let (reader, mut writer) = tokio::io::split(client_stream);
        let mut reader = tokio::io::BufReader::new(reader);

        // Send correct magic + version 1.37
        wire::write_u64(&mut writer, WORKER_MAGIC_1).await?;
        wire::write_u64(&mut writer, encode_version(1, 37)).await?;
        writer.flush().await?;

        // Read server magic + version
        let _magic2 = wire::read_u64(&mut reader).await?;
        let _server_version = wire::read_u64(&mut reader).await?;

        // NO feature exchange for v1.37 — jump straight to post-handshake
        // Send affinity + reserveSpace
        wire::write_u64(&mut writer, 0).await?; // CPU affinity = 0
        wire::write_u64(&mut writer, 0).await?; // reserveSpace = 0
        writer.flush().await?;

        // Read version string + trusted + STDERR_LAST
        let _version_str = wire::read_string(&mut reader).await?;
        let _trusted = wire::read_u64(&mut reader).await?;
        let last = wire::read_u64(&mut reader).await?;
        assert_eq!(last, crate::protocol::stderr::STDERR_LAST);

        drop(writer);
        let result = server_handle.await??;
        assert_eq!(result.negotiated_version(), encode_version(1, 37));
        Ok(())
    }
}
