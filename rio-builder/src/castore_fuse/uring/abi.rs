//! FUSE wire format for the fuse-over-io_uring transport.
//!
//! The classic `/dev/fuse` transport's parsing and marshalling live
//! inside the `fuser` crate and are `pub(crate)` there, so the ring
//! dispatcher carries its own copy of the (stable, uAPI-frozen) subset
//! the read-only castore-FUSE can ever see. Layouts mirror
//! `include/uapi/linux/fuse.h`; the size/offset unit tests below pin
//! them against the documented ABI values so a refactor cannot silently
//! shift a field.
//!
//! Endianness: the FUSE wire format is native-endian (the kernel and
//! daemon share one machine), hence `to_ne_bytes`/`from_ne_bytes`
//! throughout.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use fuser::{FileAttr, FileType};

// ─── opcodes (enum fuse_opcode) ─────────────────────────────────────────

pub(super) const FUSE_LOOKUP: u32 = 1;
pub(super) const FUSE_GETATTR: u32 = 3;
pub(super) const FUSE_SETATTR: u32 = 4;
pub(super) const FUSE_READLINK: u32 = 5;
pub(super) const FUSE_SYMLINK: u32 = 6;
pub(super) const FUSE_MKNOD: u32 = 8;
pub(super) const FUSE_MKDIR: u32 = 9;
pub(super) const FUSE_UNLINK: u32 = 10;
pub(super) const FUSE_RMDIR: u32 = 11;
pub(super) const FUSE_RENAME: u32 = 12;
pub(super) const FUSE_LINK: u32 = 13;
pub(super) const FUSE_OPEN: u32 = 14;
pub(super) const FUSE_READ: u32 = 15;
pub(super) const FUSE_WRITE: u32 = 16;
pub(super) const FUSE_STATFS: u32 = 17;
pub(super) const FUSE_RELEASE: u32 = 18;
pub(super) const FUSE_SETXATTR: u32 = 21;
pub(super) const FUSE_GETXATTR: u32 = 22;
pub(super) const FUSE_LISTXATTR: u32 = 23;
pub(super) const FUSE_REMOVEXATTR: u32 = 24;
pub(super) const FUSE_OPENDIR: u32 = 27;
pub(super) const FUSE_READDIR: u32 = 28;
pub(super) const FUSE_RELEASEDIR: u32 = 29;
pub(super) const FUSE_CREATE: u32 = 35;
pub(super) const FUSE_DESTROY: u32 = 38;
pub(super) const FUSE_IOCTL: u32 = 39;
pub(super) const FUSE_FALLOCATE: u32 = 43;
pub(super) const FUSE_READDIRPLUS: u32 = 44;
pub(super) const FUSE_RENAME2: u32 = 45;

// ─── open flags (fuse_open_out.open_flags) ──────────────────────────────

pub(super) const FOPEN_KEEP_CACHE: u32 = 1 << 1;
pub(super) const FOPEN_CACHE_DIR: u32 = 1 << 3;
pub(super) const FOPEN_PASSTHROUGH: u32 = 1 << 7;

// ─── fuse-over-io_uring framing (struct fuse_uring_req_header) ──────────

/// `FUSE_URING_IN_OUT_HEADER_SZ`: the `in_out` region holding
/// `fuse_in_header` (request) / `fuse_out_header` (reply).
pub(super) const URING_IN_OUT_HEADER_SZ: usize = 128;
/// `FUSE_URING_OP_IN_OUT_SZ`: the per-opcode fixed header region.
pub(super) const URING_OP_IN_OUT_SZ: usize = 128;
/// Byte offset of `struct fuse_uring_ent_in_out` inside the header
/// buffer.
pub(super) const URING_ENT_IN_OUT_OFF: usize = URING_IN_OUT_HEADER_SZ + URING_OP_IN_OUT_SZ;
/// `sizeof(struct fuse_uring_ent_in_out)`.
pub(super) const URING_ENT_IN_OUT_SZ: usize = 32;
/// `sizeof(struct fuse_uring_req_header)` — the minimum the headers
/// iovec must cover.
pub(super) const URING_REQ_HEADER_SZ: usize = URING_ENT_IN_OUT_OFF + URING_ENT_IN_OUT_SZ;

/// SQE sub-commands (`enum fuse_uring_cmd`).
pub(super) const FUSE_IO_URING_CMD_REGISTER: u32 = 1;
pub(super) const FUSE_IO_URING_CMD_COMMIT_AND_FETCH: u32 = 2;

// ─── little parse/encode helpers ─────────────────────────────────────────

fn get_u32(b: &[u8], off: usize) -> u32 {
    u32::from_ne_bytes(b[off..off + 4].try_into().unwrap())
}

fn get_u64(b: &[u8], off: usize) -> u64 {
    u64::from_ne_bytes(b[off..off + 8].try_into().unwrap())
}

fn put_u32(b: &mut [u8], off: usize, v: u32) {
    b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn put_i32(b: &mut [u8], off: usize, v: i32) {
    b[off..off + 4].copy_from_slice(&v.to_ne_bytes());
}

fn put_u64(b: &mut [u8], off: usize, v: u64) {
    b[off..off + 8].copy_from_slice(&v.to_ne_bytes());
}

// ─── struct fuse_in_header (request side of in_out) ─────────────────────

pub(super) const IN_HEADER_SZ: usize = 40;

/// Parsed `struct fuse_in_header` — the fields the dispatcher uses.
#[derive(Debug, Clone, Copy)]
pub(super) struct InHeader {
    pub opcode: u32,
    pub unique: u64,
    pub nodeid: u64,
}

impl InHeader {
    /// Parse from the start of the `in_out` region. `None` if the
    /// region is impossibly short (kernel contract violation).
    pub(super) fn parse(b: &[u8]) -> Option<Self> {
        if b.len() < IN_HEADER_SZ {
            return None;
        }
        Some(Self {
            opcode: get_u32(b, 4),
            unique: get_u64(b, 8),
            nodeid: get_u64(b, 16),
        })
    }
}

// ─── struct fuse_out_header (reply side of in_out) ──────────────────────

pub(super) const OUT_HEADER_SZ: usize = 16;

/// Write a `struct fuse_out_header` at the start of the `in_out`
/// region. `error` is 0 or a negated errno; `len` covers header +
/// payload like the classic transport.
pub(super) fn write_out_header(b: &mut [u8], error: i32, unique: u64, payload_len: usize) {
    put_u32(b, 0, (OUT_HEADER_SZ + payload_len) as u32);
    put_i32(b, 4, error);
    put_u64(b, 8, unique);
}

// ─── struct fuse_uring_ent_in_out ────────────────────────────────────────

/// The ring-entry trailer of the header buffer: request direction
/// carries the commit id and the request payload size; reply direction
/// echoes the commit id and carries the reply payload size.
#[derive(Debug, Clone, Copy)]
pub(super) struct EntInOut {
    pub commit_id: u64,
    pub payload_sz: u32,
}

impl EntInOut {
    pub(super) fn parse(header_buf: &[u8]) -> Option<Self> {
        let b = header_buf.get(URING_ENT_IN_OUT_OFF..URING_ENT_IN_OUT_OFF + URING_ENT_IN_OUT_SZ)?;
        Some(Self {
            commit_id: get_u64(b, 8),
            payload_sz: get_u32(b, 16),
        })
    }

    pub(super) fn write(header_buf: &mut [u8], commit_id: u64, payload_sz: u32) {
        let b = &mut header_buf[URING_ENT_IN_OUT_OFF..URING_ENT_IN_OUT_OFF + URING_ENT_IN_OUT_SZ];
        b.fill(0);
        put_u64(b, 8, commit_id);
        put_u32(b, 16, payload_sz);
    }
}

// ─── per-op request headers (op_in region) ──────────────────────────────

/// `struct fuse_open_in` — only `flags` matters to a read-only fs.
pub(super) fn parse_open_in_flags(op_in: &[u8]) -> i32 {
    get_u32(op_in, 0) as i32
}

/// `struct fuse_read_in { fh, offset, size, .. }` — shared by
/// READ/READDIR/READDIRPLUS.
#[derive(Debug, Clone, Copy)]
pub(super) struct ReadIn {
    pub fh: u64,
    pub offset: u64,
    pub size: u32,
}

impl ReadIn {
    pub(super) fn parse(op_in: &[u8]) -> Self {
        Self {
            fh: get_u64(op_in, 0),
            offset: get_u64(op_in, 8),
            size: get_u32(op_in, 16),
        }
    }
}

/// `struct fuse_release_in.fh`.
pub(super) fn parse_release_in_fh(op_in: &[u8]) -> u64 {
    get_u64(op_in, 0)
}

/// `struct fuse_getxattr_in.size` (also the listxattr request).
pub(super) fn parse_getxattr_in_size(op_in: &[u8]) -> u32 {
    get_u32(op_in, 0)
}

// ─── reply payload encoding ──────────────────────────────────────────────

/// `sizeof(struct fuse_attr)`: 6×u64 (ino..ctime) + 10×u32
/// (atimensec..flags) = 88. The reply sizes below must be byte-exact:
/// the kernel's `fuse_copy_out_args` rejects a reply whose payload
/// length is not exactly the out-arg size with EINVAL.
const ATTR_SZ: usize = 88;
pub(super) const ENTRY_OUT_SZ: usize = 40 + ATTR_SZ;
pub(super) const ATTR_OUT_SZ: usize = 16 + ATTR_SZ;
pub(super) const OPEN_OUT_SZ: usize = 16;
pub(super) const STATFS_OUT_SZ: usize = 80;
pub(super) const GETXATTR_OUT_SZ: usize = 8;
pub(super) const DIRENT_SZ: usize = 24;

/// `mode_from_kind_and_perm` parity with fuser: S_IFMT bits from the
/// kind, permission bits from `perm`.
fn mode_from_kind_and_perm(kind: FileType, perm: u16) -> u32 {
    use nix::libc::{S_IFBLK, S_IFCHR, S_IFDIR, S_IFIFO, S_IFLNK, S_IFREG, S_IFSOCK};
    (match kind {
        FileType::NamedPipe => S_IFIFO,
        FileType::CharDevice => S_IFCHR,
        FileType::BlockDevice => S_IFBLK,
        FileType::Directory => S_IFDIR,
        FileType::RegularFile => S_IFREG,
        FileType::Symlink => S_IFLNK,
        FileType::Socket => S_IFSOCK,
    }) | u32::from(perm)
}

fn time_parts(t: &SystemTime) -> (u64, u32) {
    // Castore attrs are epoch or epoch+1s; anything pre-epoch clamps
    // to 0 (fuser signals negative times via i64 casts, but the tree
    // never produces them).
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    (d.as_secs(), d.subsec_nanos())
}

/// Encode `struct fuse_attr` at `b[off..off+ATTR_SZ]`.
fn write_attr(b: &mut [u8], off: usize, attr: &FileAttr) {
    let (atime, atimensec) = time_parts(&attr.atime);
    let (mtime, mtimensec) = time_parts(&attr.mtime);
    let (ctime, ctimensec) = time_parts(&attr.ctime);
    put_u64(b, off, attr.ino.0);
    put_u64(b, off + 8, attr.size);
    put_u64(b, off + 16, attr.blocks);
    put_u64(b, off + 24, atime);
    put_u64(b, off + 32, mtime);
    put_u64(b, off + 40, ctime);
    put_u32(b, off + 48, atimensec);
    put_u32(b, off + 52, mtimensec);
    put_u32(b, off + 56, ctimensec);
    put_u32(b, off + 60, mode_from_kind_and_perm(attr.kind, attr.perm));
    put_u32(b, off + 64, attr.nlink);
    put_u32(b, off + 68, attr.uid);
    put_u32(b, off + 72, attr.gid);
    put_u32(b, off + 76, attr.rdev);
    put_u32(b, off + 80, attr.blksize);
    put_u32(b, off + 84, 0); // fuse_attr.flags (SUBMOUNT/DAX): unused
}

fn ttl_parts(ttl: &Duration) -> (u64, u32) {
    (ttl.as_secs(), ttl.subsec_nanos())
}

/// Encode `struct fuse_entry_out` at `b[off..]`; returns the byte
/// count written. `nodeid` comes from `attr.ino` like fuser's
/// `ReplyEntry::entry` — an `ino` of 0 is the cached negative entry.
pub(super) fn write_entry_out(
    b: &mut [u8],
    off: usize,
    ttl: &Duration,
    attr: &FileAttr,
    generation: u64,
) -> usize {
    let (secs, nsecs) = ttl_parts(ttl);
    put_u64(b, off, attr.ino.0);
    put_u64(b, off + 8, generation);
    put_u64(b, off + 16, secs); // entry_valid
    put_u64(b, off + 24, secs); // attr_valid
    put_u32(b, off + 32, nsecs);
    put_u32(b, off + 36, nsecs);
    write_attr(b, off + 40, attr);
    ENTRY_OUT_SZ
}

/// Encode `struct fuse_attr_out`; returns the byte count written.
pub(super) fn write_attr_out(b: &mut [u8], ttl: &Duration, attr: &FileAttr) -> usize {
    let (secs, nsecs) = ttl_parts(ttl);
    put_u64(b, 0, secs);
    put_u32(b, 8, nsecs);
    put_u32(b, 12, 0);
    write_attr(b, 16, attr);
    ATTR_OUT_SZ
}

/// Encode `struct fuse_open_out`; returns the byte count written.
/// `backing_id != 0` is only meaningful with `FOPEN_PASSTHROUGH` in
/// `open_flags`.
pub(super) fn write_open_out(b: &mut [u8], fh: u64, open_flags: u32, backing_id: i32) -> usize {
    put_u64(b, 0, fh);
    put_u32(b, 8, open_flags);
    put_i32(b, 12, backing_id);
    OPEN_OUT_SZ
}

/// Encode `struct fuse_statfs_out` with fuser's default-handler values
/// (everything 0 except bsize=512, namelen=255); returns the byte
/// count written.
pub(super) fn write_statfs_out(b: &mut [u8]) -> usize {
    b[..STATFS_OUT_SZ].fill(0);
    put_u32(b, 40, 512); // bsize
    put_u32(b, 44, 255); // namelen
    STATFS_OUT_SZ
}

/// Encode `struct fuse_getxattr_out` (the size-probe reply); returns
/// the byte count written.
pub(super) fn write_getxattr_out(b: &mut [u8], size: u32) -> usize {
    put_u32(b, 0, size);
    put_u32(b, 4, 0);
    GETXATTR_OUT_SZ
}

/// Incremental `fuse_dirent` / `fuse_direntplus` packer with the same
/// "stop when the kernel's requested size is exceeded" contract as
/// fuser's `ReplyDirectory::add` (an entry that would overflow is not
/// written and `push` returns `false`).
pub(super) struct DirentBuf<'a> {
    buf: &'a mut [u8],
    max: usize,
    len: usize,
}

impl<'a> DirentBuf<'a> {
    /// `max` is the kernel's `fuse_read_in.size`, clamped by the
    /// caller to the payload buffer.
    pub(super) fn new(buf: &'a mut [u8], max: usize) -> Self {
        let max = max.min(buf.len());
        Self { buf, max, len: 0 }
    }

    pub(super) fn len(&self) -> usize {
        self.len
    }

    fn push_raw(&mut self, header: &[u8], ino: u64, off: u64, typ: u32, name: &[u8]) -> bool {
        let dirent_total = DIRENT_SZ + name.len();
        let padded = dirent_total.next_multiple_of(8);
        let total = header.len() + padded;
        if self.len + total > self.max {
            return false;
        }
        let b = &mut self.buf[self.len..self.len + total];
        b[header.len()..].fill(0); // zero the name padding
        b[..header.len()].copy_from_slice(header);
        let d = header.len();
        put_u64(b, d, ino);
        put_u64(b, d + 8, off);
        put_u32(b, d + 16, name.len() as u32);
        put_u32(b, d + 20, typ);
        b[d + DIRENT_SZ..d + DIRENT_SZ + name.len()].copy_from_slice(name);
        self.len += total;
        true
    }

    /// Append a `fuse_dirent`. Returns `false` (without writing) when
    /// the entry does not fit.
    pub(super) fn push(&mut self, ino: u64, off: u64, kind: FileType, name: &[u8]) -> bool {
        let typ = mode_from_kind_and_perm(kind, 0) >> 12;
        self.push_raw(&[], ino, off, typ, name)
    }

    /// Append a `fuse_direntplus` (entry_out + dirent). Returns
    /// `false` (without writing) when the entry does not fit.
    pub(super) fn push_plus(
        &mut self,
        off: u64,
        name: &[u8],
        ttl: &Duration,
        attr: &FileAttr,
        generation: u64,
    ) -> bool {
        let mut entry = [0u8; ENTRY_OUT_SZ];
        write_entry_out(&mut entry, 0, ttl, attr, generation);
        let typ = mode_from_kind_and_perm(attr.kind, 0) >> 12;
        self.push_raw(&entry, attr.ino.0, off, typ, name)
    }
}

// ─── struct fuse_uring_cmd_req (the 80-byte SQE command area) ───────────

/// Encode `struct fuse_uring_cmd_req { flags: u64, commit_id: u64,
/// qid: u16, padding: [u8; 6] }` into an SQE128 command area.
pub(super) fn encode_cmd_req(commit_id: u64, qid: u16) -> [u8; 80] {
    let mut cmd = [0u8; 80];
    cmd[8..16].copy_from_slice(&commit_id.to_ne_bytes());
    cmd[16..18].copy_from_slice(&qid.to_ne_bytes());
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuser::INodeNo;

    fn get_u16(b: &[u8], off: usize) -> u16 {
        u16::from_ne_bytes(b[off..off + 2].try_into().unwrap())
    }

    fn file_attr() -> FileAttr {
        FileAttr {
            ino: INodeNo(7),
            size: 42,
            blocks: 1,
            atime: UNIX_EPOCH + Duration::from_secs(1),
            mtime: UNIX_EPOCH + Duration::from_secs(1),
            ctime: UNIX_EPOCH + Duration::from_secs(1),
            crtime: UNIX_EPOCH,
            kind: FileType::RegularFile,
            perm: 0o555,
            nlink: 1,
            uid: 0,
            gid: 0,
            rdev: 0,
            blksize: 4096,
            flags: 0,
        }
    }

    /// The framing constants must match `include/uapi/linux/fuse.h`:
    /// a wrong header size or trailer offset corrupts every ring
    /// request silently (the kernel just reads the wrong bytes).
    #[test]
    fn uring_header_layout_matches_kernel_abi() {
        assert_eq!(URING_IN_OUT_HEADER_SZ, 128);
        assert_eq!(URING_OP_IN_OUT_SZ, 128);
        assert_eq!(URING_ENT_IN_OUT_OFF, 256);
        assert_eq!(
            URING_REQ_HEADER_SZ, 288,
            "sizeof(struct fuse_uring_req_header)"
        );
        assert_eq!(IN_HEADER_SZ, 40, "sizeof(struct fuse_in_header)");
        assert_eq!(OUT_HEADER_SZ, 16, "sizeof(struct fuse_out_header)");
    }

    /// `fuse_uring_ent_in_out` round-trip: the reply writer must place
    /// `commit_id`/`payload_sz` where the request parser (and the
    /// kernel) read them.
    #[test]
    fn ent_in_out_roundtrip() {
        let mut hdr = [0u8; URING_REQ_HEADER_SZ];
        EntInOut::write(&mut hdr, 0xdead_beef_cafe, 4096);
        let parsed = EntInOut::parse(&hdr).unwrap();
        assert_eq!(parsed.commit_id, 0xdead_beef_cafe);
        assert_eq!(parsed.payload_sz, 4096);
        // A header buffer shorter than the trailer is a malformed
        // entry, not a panic.
        assert!(EntInOut::parse(&hdr[..200]).is_none());
    }

    /// `fuse_attr`/`fuse_entry_out`/`fuse_attr_out` field placement,
    /// pinned against the uAPI struct layout. A drifted offset would
    /// make `stat()` return garbage for every file in the mount.
    #[test]
    fn attr_encoding_matches_uapi_layout() {
        let attr = file_attr();
        let ttl = Duration::from_secs(5) + Duration::from_nanos(9);

        let mut b = vec![0u8; ENTRY_OUT_SZ];
        assert_eq!(write_entry_out(&mut b, 0, &ttl, &attr, 3), ENTRY_OUT_SZ);
        assert_eq!(get_u64(&b, 0), 7, "nodeid = attr.ino");
        assert_eq!(get_u64(&b, 8), 3, "generation");
        assert_eq!(get_u64(&b, 16), 5, "entry_valid secs");
        assert_eq!(get_u32(&b, 32), 9, "entry_valid nsecs");
        // fuse_attr at offset 40.
        assert_eq!(get_u64(&b, 40), 7, "attr.ino");
        assert_eq!(get_u64(&b, 48), 42, "attr.size");
        assert_eq!(get_u64(&b, 64), 1, "attr.atime secs");
        assert_eq!(
            get_u32(&b, 100),
            nix::libc::S_IFREG | 0o555,
            "attr.mode = S_IFMT | perm"
        );
        assert_eq!(get_u32(&b, 120), 4096, "attr.blksize");

        let mut b = vec![0u8; ATTR_OUT_SZ];
        assert_eq!(write_attr_out(&mut b, &ttl, &attr), ATTR_OUT_SZ);
        assert_eq!(get_u64(&b, 0), 5, "attr_valid secs");
        assert_eq!(get_u32(&b, 8), 9, "attr_valid nsecs");
        assert_eq!(get_u64(&b, 16), 7, "attr.ino at offset 16");
    }

    /// The infinite-TTL encoding (`Duration::MAX`) must not wrap or
    /// panic — it is what every castore reply carries.
    #[test]
    fn infinite_ttl_encodes_saturated() {
        let mut b = vec![0u8; ATTR_OUT_SZ];
        write_attr_out(&mut b, &Duration::MAX, &file_attr());
        assert_eq!(get_u64(&b, 0), u64::MAX);
    }

    /// `fuse_open_out` carries the passthrough backing id exactly as
    /// fuser's `opened_passthrough` does — this is the warm-read path
    /// over the ring.
    #[test]
    fn open_out_passthrough_layout() {
        let mut b = vec![0u8; OPEN_OUT_SZ];
        write_open_out(&mut b, 11, FOPEN_PASSTHROUGH, 5);
        assert_eq!(get_u64(&b, 0), 11, "fh");
        assert_eq!(get_u32(&b, 8), 1 << 7, "open_flags = FOPEN_PASSTHROUGH");
        assert_eq!(get_u32(&b, 12), 5, "backing_id");
    }

    /// Dirent packing: 8-byte alignment, the kernel's resume offsets,
    /// and the "would overflow → not written" contract that fuser's
    /// `ReplyDirectory::add` has. READDIR resume breaks if any of
    /// these drift.
    #[test]
    fn dirent_packing_aligns_and_respects_size() {
        let mut buf = vec![0u8; 4096];
        let mut d = DirentBuf::new(&mut buf, 80);
        assert!(d.push(1, 1, FileType::Directory, b"."));
        // 24 + 1 → padded to 32.
        assert_eq!(d.len(), 32);
        assert!(d.push(2, 2, FileType::RegularFile, b"abcdefgh"));
        // 24 + 8 = 32, already aligned.
        assert_eq!(d.len(), 64);
        // 80 - 64 = 16 < 24: must refuse without writing.
        assert!(!d.push(3, 3, FileType::Symlink, b"x"));
        assert_eq!(d.len(), 64);

        // Field placement of the second entry.
        assert_eq!(get_u64(&buf, 32), 2, "ino");
        assert_eq!(get_u64(&buf, 40), 2, "off");
        assert_eq!(get_u32(&buf, 48), 8, "namelen");
        assert_eq!(
            get_u32(&buf, 52),
            nix::libc::DT_REG as u32,
            "typ = mode >> 12"
        );
        assert_eq!(&buf[56..64], b"abcdefgh");
    }

    /// direntplus = entry_out (128) + dirent (24+name, padded); the
    /// attr inside must be the same encoding as a LOOKUP reply so the
    /// dcache priming is byte-identical.
    #[test]
    fn direntplus_embeds_entry_out() {
        let mut buf = vec![0u8; 4096];
        let attr = file_attr();
        let mut d = DirentBuf::new(&mut buf, 4096);
        assert!(d.push_plus(1, b"f", &Duration::MAX, &attr, 0));
        assert_eq!(d.len(), ENTRY_OUT_SZ + 32);
        assert_eq!(get_u64(&buf, 0), 7, "entry_out.nodeid");
        assert_eq!(get_u64(&buf, ENTRY_OUT_SZ), 7, "dirent.ino");
        assert_eq!(get_u32(&buf, ENTRY_OUT_SZ + 16), 1, "namelen");
    }

    /// `fuse_uring_cmd_req` field placement in the 80-byte command
    /// area: commit_id at 8, qid at 16 (flags at 0 stays zero).
    #[test]
    fn cmd_req_layout() {
        let cmd = encode_cmd_req(0x1122_3344, 9);
        assert_eq!(get_u64(&cmd, 0), 0, "flags");
        assert_eq!(get_u64(&cmd, 8), 0x1122_3344, "commit_id");
        assert_eq!(get_u16(&cmd, 16), 9, "qid");
    }

    /// Request-side parsers read the documented offsets.
    #[test]
    fn request_parsers_read_uapi_offsets() {
        let mut b = vec![0u8; IN_HEADER_SZ];
        put_u32(&mut b, 0, 64); // len
        put_u32(&mut b, 4, FUSE_LOOKUP);
        put_u64(&mut b, 8, 77); // unique
        put_u64(&mut b, 16, 5); // nodeid
        let h = InHeader::parse(&b).unwrap();
        assert_eq!((h.opcode, h.unique, h.nodeid), (FUSE_LOOKUP, 77, 5));
        assert!(InHeader::parse(&b[..32]).is_none());

        let mut op = vec![0u8; 40];
        put_u64(&mut op, 0, 4); // fh
        put_u64(&mut op, 8, 8192); // offset
        put_u32(&mut op, 16, 4096); // size
        let r = ReadIn::parse(&op);
        assert_eq!((r.fh, r.offset, r.size), (4, 8192, 4096));
    }
}
