//! Nix worker protocol handshake.
//!
//! Implements the server side of the version negotiation sequence
//! for protocol version 1.37+.

use tokio::io::{AsyncRead, AsyncWrite};

use super::wire::{self, WireError};

/// Client-to-server magic (u32 LE). Represents "nixc" in ASCII.
pub const WORKER_MAGIC_1: u32 = 0x6e697863;

/// Server-to-client magic (u32 LE). Represents "dxio" in ASCII.
pub const WORKER_MAGIC_2: u32 = 0x6478696f;

/// Our protocol version: 1.37 encoded as `(1 << 8) | 37`.
pub const PROTOCOL_VERSION: u64 = 0x125;

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

    #[error("invalid client magic: expected {WORKER_MAGIC_1:#010x}, got {0:#010x}")]
    InvalidMagic(u32),

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
    // Step 1: Read client magic
    let client_magic = wire::read_u32(stream).await?;
    if client_magic != WORKER_MAGIC_1 {
        return Err(HandshakeError::InvalidMagic(client_magic));
    }

    // Step 2: Write server magic
    wire::write_u32(stream, WORKER_MAGIC_2).await?;

    // Step 3: Write our protocol version
    wire::write_u64(stream, PROTOCOL_VERSION).await?;

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

    // Step 7: Write version string
    wire::write_string(stream, version_string).await?;

    // Step 8: Write trusted status (1 = trusted)
    wire::write_u64(stream, 1).await?;

    Ok(HandshakeResult { client_version })
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

    /// Build a client-side handshake byte sequence.
    async fn build_client_handshake(version: u64) -> Vec<u8> {
        let mut buf = Vec::new();
        // Client sends: magic1 (u32), then later: version (u64), affinity (u64), reserve (u64)
        wire::write_u32(&mut buf, WORKER_MAGIC_1).await.unwrap();
        // The handshake function reads magic first, then writes magic2+version,
        // then reads client version. So we need to set up a duplex stream.
        // For this test helper, just build the request bytes.
        wire::write_u64(&mut buf, version).await.unwrap();
        wire::write_u64(&mut buf, 0).await.unwrap(); // CPU affinity
        wire::write_u64(&mut buf, 0).await.unwrap(); // reserveSpace
        buf
    }

    #[tokio::test]
    async fn test_successful_handshake() {
        // Create a duplex stream by using a pair of buffers
        let client_data = build_client_handshake(PROTOCOL_VERSION).await;

        // We need a bidirectional stream. Use tokio::io::duplex for this.
        let (client_stream, server_stream) = tokio::io::duplex(4096);

        let server_handle = tokio::spawn(async move {
            let (reader, writer) = tokio::io::split(server_stream);
            let mut stream = tokio::io::join(reader, writer);
            server_handshake(&mut stream, "rio-build 0.1.0").await
        });

        // Write client data
        let (reader, mut writer) = tokio::io::split(client_stream);
        writer.write_all(&client_data).await.unwrap();
        // Don't close writer yet — server needs to write back and we need to read

        // Read server response
        let mut reader = tokio::io::BufReader::new(reader);
        let magic2 = wire::read_u32(&mut reader).await.unwrap();
        assert_eq!(magic2, WORKER_MAGIC_2);

        let server_version = wire::read_u64(&mut reader).await.unwrap();
        assert_eq!(server_version, PROTOCOL_VERSION);

        let version_string = wire::read_string(&mut reader).await.unwrap();
        assert_eq!(version_string, "rio-build 0.1.0");

        let trusted = wire::read_u64(&mut reader).await.unwrap();
        assert_eq!(trusted, 1);

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
