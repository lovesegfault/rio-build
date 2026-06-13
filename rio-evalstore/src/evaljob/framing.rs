//! Synchronous length-delimited proto framing — the blocking twin of
//! `rio-build-cli::framing` (same wire shape, same 64 MiB cap). The
//! eval parent and its fork workers are thread-free synchronous
//! processes, so tokio framing is not an option here; the two
//! implementations interoperate byte-for-byte (verified by the
//! coordinator-side tests plus the parent-loop tests in this crate).

use std::io::{self, Read, Write};

use prost::Message;

/// Hard cap on one frame, mirroring `rio-build-cli::framing`'s
/// `MAX_FRAME_LEN`. Anything larger is a corrupt length prefix.
pub const MAX_FRAME_LEN: u32 = 64 * 1024 * 1024;

/// Blocking Read/Write over a BORROWED raw fd (the coordinator channel
/// on fd 3, a worker's socketpair end handed across the FFI) — never
/// closes it.
pub struct FdIo(pub std::os::fd::RawFd);

impl Read for FdIo {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            // SAFETY: plain read(2) into a valid buffer.
            let n = unsafe { libc::read(self.0, buf.as_mut_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }
}

impl Write for FdIo {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        loop {
            // SAFETY: plain write(2) from a valid buffer.
            let n = unsafe { libc::write(self.0, buf.as_ptr().cast(), buf.len()) };
            if n >= 0 {
                return Ok(n as usize);
            }
            let e = io::Error::last_os_error();
            if e.kind() != io::ErrorKind::Interrupted {
                return Err(e);
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Write one length-prefixed frame and flush.
pub fn write_frame<W: Write, M: Message>(w: &mut W, msg: &M) -> io::Result<()> {
    let body = msg.encode_to_vec();
    let len = u32::try_from(body.len())
        .ok()
        .filter(|l| *l <= MAX_FRAME_LEN)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("frame too large: {} > {MAX_FRAME_LEN}", body.len()),
            )
        })?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// Write an already-encoded frame body (the parent's raw relay path:
/// worker frames pass through to the coordinator without re-encoding).
pub fn write_raw_frame<W: Write>(w: &mut W, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len())
        .ok()
        .filter(|l| *l <= MAX_FRAME_LEN)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "relay frame too large"))?;
    w.write_all(&len.to_be_bytes())?;
    w.write_all(body)?;
    w.flush()
}

/// Read one frame body. `Ok(None)` on clean EOF at a frame boundary;
/// `InvalidData` on an oversized prefix or truncation mid-frame.
// r[impl bc.ipc.frame-cap]
pub fn read_raw_frame<R: Read>(r: &mut R) -> io::Result<Option<Vec<u8>>> {
    let mut len_buf = [0u8; 4];
    // Clean EOF is ZERO bytes at a frame boundary (same rule as the
    // async twin): read the first byte separately so a torn prefix is
    // truncation, not a close.
    let mut first = [0u8; 1];
    loop {
        match r.read(&mut first) {
            Ok(0) => return Ok(None),
            Ok(_) => break,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    len_buf[0] = first[0];
    r.read_exact(&mut len_buf[1..]).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "stream truncated mid-length-prefix",
            )
        } else {
            e
        }
    })?;
    let len = u32::from_be_bytes(len_buf);
    if len > MAX_FRAME_LEN {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds cap {MAX_FRAME_LEN}"),
        ));
    }
    let mut body = vec![0u8; len as usize];
    r.read_exact(&mut body).map_err(|e| {
        if e.kind() == io::ErrorKind::UnexpectedEof {
            io::Error::new(io::ErrorKind::InvalidData, "stream truncated mid-frame")
        } else {
            e
        }
    })?;
    Ok(Some(body))
}

/// Read and decode one frame.
pub fn read_frame<R: Read, M: Message + Default>(r: &mut R) -> io::Result<Option<M>> {
    match read_raw_frame(r)? {
        None => Ok(None),
        Some(body) => M::decode(body.as_slice())
            .map(Some)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::evaljob::{CoordinatorFrame, WorkItem, coordinator_frame};

    fn work(attr: &str) -> CoordinatorFrame {
        CoordinatorFrame {
            msg: Some(coordinator_frame::Msg::Work(WorkItem { attr: attr.into() })),
        }
    }

    /// The sync framing interoperates with itself and honours the
    /// clean-EOF / truncation distinction of the async twin.
    // r[verify bc.ipc.frame-cap]
    #[test]
    fn roundtrip_eof_and_truncation() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &work("pkgs.hello")).unwrap();
        write_frame(&mut buf, &work("pkgs.world")).unwrap();

        let mut r = buf.as_slice();
        let f1: CoordinatorFrame = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(f1, work("pkgs.hello"));
        let f2: CoordinatorFrame = read_frame(&mut r).unwrap().unwrap();
        assert_eq!(f2, work("pkgs.world"));
        assert!(read_frame::<_, CoordinatorFrame>(&mut r).unwrap().is_none());

        // Oversized prefix rejected.
        let mut bad = u32::MAX.to_be_bytes().to_vec();
        let err = read_frame::<_, CoordinatorFrame>(&mut bad.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Torn prefix is truncation, not EOF.
        bad = vec![0u8, 0u8];
        let err = read_frame::<_, CoordinatorFrame>(&mut bad.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        // Truncated body is truncation.
        let mut torn = 100u32.to_be_bytes().to_vec();
        torn.extend_from_slice(b"abc");
        let err = read_frame::<_, CoordinatorFrame>(&mut torn.as_slice()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    /// Wire compatibility with the async coordinator side is by
    /// construction (same prefix, same proto bytes): a frame written
    /// here decodes from the exact bytes prost emits, prefix included.
    #[test]
    fn wire_shape_is_len_prefix_then_proto() {
        let msg = work("a");
        let mut buf = Vec::new();
        write_frame(&mut buf, &msg).unwrap();
        let body = msg.encode_to_vec();
        assert_eq!(&buf[..4], &(body.len() as u32).to_be_bytes());
        assert_eq!(&buf[4..], &body[..]);
    }
}
