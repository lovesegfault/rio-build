//! Nix wire protocol client-side test helpers.
//!
//! These send the client half of the handshake/setOptions exchange against
//! a `DuplexStream`. All wire I/O propagates via `?`.

use rio_nix::protocol::client::{StderrMessage, read_stderr_message};
use rio_nix::protocol::handshake::{PROTOCOL_VERSION, WORKER_MAGIC_1, WORKER_MAGIC_2};
use rio_nix::protocol::stderr::{STDERR_LAST, StderrError};
use rio_nix::protocol::wire;
use tokio::io::DuplexStream;

/// Perform the client side of the Nix worker protocol handshake.
///
/// Sends magic1 + version, reads magic2 + server version, exchanges features,
/// sends obsolete-affinity/reserve-space placeholders, reads version-string +
/// trusted flag + STDERR_LAST.
pub async fn do_handshake(s: &mut DuplexStream) -> anyhow::Result<()> {
    crate::wire_send!(s; u64: WORKER_MAGIC_1, u64: PROTOCOL_VERSION);

    let magic2 = wire::read_u64(s).await?;
    assert_eq!(
        magic2, WORKER_MAGIC_2,
        "server must reply with WORKER_MAGIC_2"
    );
    let _server_version = wire::read_u64(s).await?;

    crate::wire_send!(s; strings: wire::NO_STRINGS);
    let _server_features = wire::read_strings(s).await?;

    crate::wire_send!(s; u64: 0, u64: 0); // obsolete CPU affinity, reserveSpace

    let _version = wire::read_string(s).await?;
    let _trusted = wire::read_u64(s).await?;

    let last = wire::read_u64(s).await?;
    assert_eq!(last, STDERR_LAST, "handshake should end with STDERR_LAST");
    Ok(())
}

/// Send wopSetOptions (19) with all-zero values and empty overrides.
pub async fn send_set_options(s: &mut DuplexStream) -> anyhow::Result<()> {
    // 14 u64 zeros: opcode(19) + 12 option fields + overrides count(0)
    crate::wire_send!(s;
        u64: 19, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0,
        u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0,
    );

    let msg = wire::read_u64(s).await?;
    assert_eq!(msg, STDERR_LAST, "setOptions should end with STDERR_LAST");
    Ok(())
}

/// `wopQueryPathInfo` (opcode 26) response fields, wire-order.
/// `r[gw.opcode.query-path-info]` at docs/src/components/gateway.md.
///
/// Returned AFTER `valid: bool` — caller reads `valid` first, then
/// calls [`read_path_info`] if `valid == true`. Tests usually want one
/// field (refs, nar_hash, ca) and discard the rest; this reads them all
/// so callsites don't repeat the 8-field discard sequence.
#[derive(Debug)]
pub struct PathInfoWire {
    pub deriver: String,
    pub nar_hash: String,
    pub references: Vec<String>,
    pub registration_time: u64,
    pub nar_size: u64,
    pub ultimate: bool,
    pub sigs: Vec<String>,
    pub ca: String,
}

/// Read the `wopQueryPathInfo` response body (everything after `valid: bool`).
/// Caller MUST read `valid` first — this reads deriver onwards.
pub async fn read_path_info(s: &mut DuplexStream) -> anyhow::Result<PathInfoWire> {
    Ok(PathInfoWire {
        deriver: wire::read_string(s).await?,
        nar_hash: wire::read_string(s).await?,
        references: wire::read_strings(s).await?,
        registration_time: wire::read_u64(s).await?,
        nar_size: wire::read_u64(s).await?,
        ultimate: wire::read_bool(s).await?,
        sigs: wire::read_strings(s).await?,
        ca: wire::read_string(s).await?,
    })
}

/// Drain STDERR messages until STDERR_LAST. Returns all non-Last messages.
/// Panics if STDERR_ERROR is received (use `drain_stderr_expecting_error` for
/// error-path tests).
pub async fn drain_stderr_until_last(s: &mut DuplexStream) -> anyhow::Result<Vec<StderrMessage>> {
    let mut msgs = Vec::new();
    loop {
        match read_stderr_message(s).await? {
            StderrMessage::Last => return Ok(msgs),
            StderrMessage::Error(e) => {
                panic!("unexpected STDERR_ERROR: {}", e.message);
            }
            other => msgs.push(other),
        }
    }
}

/// Drain STDERR messages expecting STDERR_ERROR. Returns the error.
/// Panics if STDERR_LAST is received first.
pub async fn drain_stderr_expecting_error(s: &mut DuplexStream) -> anyhow::Result<StderrError> {
    loop {
        match read_stderr_message(s).await? {
            StderrMessage::Error(e) => return Ok(e),
            StderrMessage::Last => panic!("expected STDERR_ERROR but got STDERR_LAST"),
            _ => {} // skip other messages
        }
    }
}

/// Build a `Vec<u8>` by sequentially writing Nix wire primitives.
/// Each `kind: value` pair expands to `wire::write_<kind>(&mut buf, value).await?;`.
/// Callers must be in a function returning `Result` — the `?` inside the
/// block-expression early-returns from the enclosing function.
/// Must be called from async context.
///
/// # Kinds
/// - `u64: n` — `write_u64`
/// - `string: s` — `write_string`
/// - `strings: slice` — `write_strings`
/// - `bool: b` — `write_bool`
/// - `bytes: slice` — `write_bytes`
/// - `framed: slice` — `write_framed_stream` (8 KiB chunks, 0-terminated)
/// - `raw: slice` — `extend_from_slice` (only valid for `Vec<u8>` sinks)
///
/// # Example
/// ```ignore
/// let buf = wire_bytes![u64: 39, string: "/nix/store/...", bool: true];
/// ```
#[macro_export]
macro_rules! wire_bytes {
    ($( $kind:ident : $val:expr ),* $(,)?) => {{
        #[allow(unused_mut)]
        let mut __buf: Vec<u8> = Vec::new();
        $( $crate::wire_bytes!(@write (&mut __buf), $kind, $val); )*
        __buf
    }};
    (@write $buf:expr, u64, $v:expr) => {
        ::rio_nix::protocol::wire::write_u64($buf, $v).await?
    };
    (@write $buf:expr, string, $v:expr) => {
        ::rio_nix::protocol::wire::write_string($buf, $v).await?
    };
    (@write $buf:expr, strings, $v:expr) => {
        ::rio_nix::protocol::wire::write_strings($buf, $v).await?
    };
    (@write $buf:expr, bool, $v:expr) => {
        ::rio_nix::protocol::wire::write_bool($buf, $v).await?
    };
    (@write $buf:expr, bytes, $v:expr) => {
        ::rio_nix::protocol::wire::write_bytes($buf, $v).await?
    };
    (@write $buf:expr, framed, $v:expr) => {
        ::rio_nix::protocol::wire::write_framed_stream($buf, $v, 8192).await?
    };
    (@write $buf:expr, raw, $v:expr) => {
        { let __b: &mut Vec<u8> = $buf; __b.extend_from_slice($v); }
    };
}

/// Write wire primitives directly to a stream (e.g., `&mut h.stream`),
/// then flush. The `raw:` kind is NOT supported — use `wire_bytes!` for
/// building intermediate `Vec<u8>` buffers with raw payloads.
///
/// # Example
/// ```ignore
/// wire_send!(&mut h.stream; u64: 39, string: path, bool: true);
/// ```
#[macro_export]
macro_rules! wire_send {
    ($stream:expr; $( $kind:ident : $val:expr ),* $(,)?) => {{
        // Reborrow to support `&mut T` bindings (`s: &mut Stream`) without
        // consuming them. For `&mut h.stream`-style call sites this is a no-op.
        let __s: &mut _ = &mut *$stream;
        $( $crate::wire_bytes!(@write __s, $kind, $val); )*
        ::tokio::io::AsyncWriteExt::flush(__s).await?;
    }};
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn wire_bytes_macro_roundtrip() -> anyhow::Result<()> {
        use rio_nix::protocol::wire;
        let buf = crate::wire_bytes![
            u64: 42,
            string: "hello",
            bool: true,
            strings: &["a", "b"],
            bytes: b"raw",
            raw: b"extra",
        ];
        let mut cur = std::io::Cursor::new(buf);
        assert_eq!(wire::read_u64(&mut cur).await?, 42);
        assert_eq!(wire::read_string(&mut cur).await?, "hello");
        assert!(wire::read_bool(&mut cur).await?);
        assert_eq!(wire::read_strings(&mut cur).await?, vec!["a", "b"]);
        assert_eq!(wire::read_bytes(&mut cur).await?, b"raw");
        // raw: bytes are appended verbatim (no length prefix)
        let mut rest = Vec::new();
        tokio::io::AsyncReadExt::read_to_end(&mut cur, &mut rest).await?;
        assert_eq!(rest, b"extra");
        Ok(())
    }
}
