//! The single index file: `digest → (pack, offset, len, kind)` plus
//! the root table (store-path entries with last-use timestamps).
//!
//! INVARIANT (ADR-024): the index is a cache of the packs, never the
//! truth. A corrupt or missing index is rebuilt by scanning packs —
//! never repaired in place. Anything that would be lost with the index
//! (roots included) is therefore also written into pack records.
//!
//! INVARIANT: every index rewrite is load + merge + rename UNDER the
//! exclusive `gc.lock` flock — re-read the on-disk index, union it
//! with this process's view, then rename into place. A plain
//! write-new + rename silently discards a concurrent writer's flushed
//! entries (demonstrated 1000/1000 lost), and lost root-table entries
//! mean the next GC mark misses those roots — data loss for fetched
//! content. [`write`] itself only serializes; the merge discipline
//! lives in the callers, which all hold the lock.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use crate::{Digest, Error, Kind, Result};

pub(crate) const INDEX_FILE: &str = "index.bin";
const INDEX_TMP: &str = "index.tmp";
const INDEX_MAGIC: &[u8; 8] = b"RIOPKIX1";
const INDEX_VERSION: u32 = 1;

/// Where one blob's payload lives. `offset`/`len` address the payload
/// bytes directly (header excluded), so a read is one pread + one
/// digest check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct BlobLoc {
    pub pack: String,
    pub offset: u64,
    pub len: u32,
    pub kind: Kind,
}

/// One root-table entry. `last_use` is UNIX seconds — the LRU clock
/// lives in batched index/pack records, not per-file utimensat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RootEntry {
    pub last_use: u64,
    pub digests: Vec<Digest>,
}

#[derive(Clone, Debug, Default)]
pub(crate) struct IndexView {
    pub blobs: HashMap<Digest, BlobLoc>,
    pub roots: HashMap<String, RootEntry>,
    /// Approximate dead bytes: incremented by Σ bytes referenced by
    /// each evicted root. Overestimates by the shared fraction —
    /// acceptable; worst harm is one spurious repack, and the mark
    /// inside GC remains ground truth (ADR-024).
    pub approx_dead: u64,
}

/// Load the on-disk index. `Ok(None)` covers both missing and corrupt:
/// the caller's answer to either is the same — rebuild from packs.
pub(crate) fn load(dir: &Path) -> Result<Option<IndexView>> {
    let data = match fs::read(dir.join(INDEX_FILE)) {
        Ok(d) => d,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    Ok(parse(&data))
}

/// Serialize and atomically replace the index. Callers MUST hold the
/// exclusive `gc.lock` flock (see module docs for why).
pub(crate) fn write(dir: &Path, view: &IndexView) -> Result<()> {
    let body = serialize(view)?;
    let mut out = Vec::with_capacity(8 + 4 + 32 + body.len());
    out.extend_from_slice(INDEX_MAGIC);
    out.extend_from_slice(&INDEX_VERSION.to_le_bytes());
    out.extend_from_slice(blake3::hash(&body).as_bytes());
    out.extend_from_slice(&body);

    let tmp = dir.join(INDEX_TMP);
    {
        // 0600 like the segments: the root table leaks store-path
        // names (project structure) even without blob access.
        let mut f = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&tmp)?;
        io::Write::write_all(&mut f, &out)?;
        f.sync_data()?;
    }
    fs::rename(&tmp, dir.join(INDEX_FILE))?;
    // fsync the directory so the rename itself survives a crash.
    fs::File::open(dir)?.sync_data()?;
    Ok(())
}

fn parse(data: &[u8]) -> Option<IndexView> {
    let header = data.get(..44)?;
    if &header[0..8] != INDEX_MAGIC
        || u32::from_le_bytes(header[8..12].try_into().ok()?) != INDEX_VERSION
    {
        return None;
    }
    let body = &data[44..];
    if blake3::hash(body).as_bytes() != &header[12..44] {
        return None;
    }

    let mut cur = Cur { d: body };
    let npacks = cur.u32()? as usize;
    let mut packs = Vec::with_capacity(npacks);
    for _ in 0..npacks {
        let len = cur.u16()? as usize;
        packs.push(std::str::from_utf8(cur.take(len)?).ok()?.to_string());
    }
    let nblobs = cur.u64()?;
    let mut blobs = HashMap::with_capacity(usize::try_from(nblobs).ok()?);
    for _ in 0..nblobs {
        let digest = cur.digest()?;
        let kind = Kind(cur.u8()?);
        let pack_idx = cur.u32()? as usize;
        let offset = cur.u64()?;
        let len = cur.u32()?;
        blobs.insert(
            digest,
            BlobLoc {
                pack: packs.get(pack_idx)?.clone(),
                offset,
                len,
                kind,
            },
        );
    }
    let nroots = cur.u32()? as usize;
    let mut roots = HashMap::with_capacity(nroots);
    for _ in 0..nroots {
        let nlen = cur.u16()? as usize;
        let name = std::str::from_utf8(cur.take(nlen)?).ok()?.to_string();
        let last_use = cur.u64()?;
        let count = cur.u32()? as usize;
        let mut digests = Vec::with_capacity(count);
        for _ in 0..count {
            digests.push(cur.digest()?);
        }
        roots.insert(name, RootEntry { last_use, digests });
    }
    let approx_dead = cur.u64()?;
    if !cur.d.is_empty() {
        return None;
    }
    Some(IndexView {
        blobs,
        roots,
        approx_dead,
    })
}

fn serialize(view: &IndexView) -> Result<Vec<u8>> {
    // Sorted iteration: byte-stable output for identical views.
    let mut pack_names: Vec<&str> = view.blobs.values().map(|l| l.pack.as_str()).collect();
    pack_names.sort_unstable();
    pack_names.dedup();
    let pack_idx: HashMap<&str, u32> = pack_names
        .iter()
        .enumerate()
        .map(|(i, n)| (*n, i as u32))
        .collect();

    let mut body = Vec::new();
    body.extend_from_slice(
        &(u32::try_from(pack_names.len()).expect("pack count fits u32")).to_le_bytes(),
    );
    for name in &pack_names {
        let len = u16::try_from(name.len())
            .map_err(|_| Error::Corrupt(format!("pack name too long: {name}")))?;
        body.extend_from_slice(&len.to_le_bytes());
        body.extend_from_slice(name.as_bytes());
    }

    let mut blobs: Vec<(&Digest, &BlobLoc)> = view.blobs.iter().collect();
    blobs.sort_unstable_by_key(|(d, _)| *d);
    body.extend_from_slice(&(blobs.len() as u64).to_le_bytes());
    for (digest, loc) in blobs {
        body.extend_from_slice(&digest.0);
        body.push(loc.kind.0);
        body.extend_from_slice(&pack_idx[loc.pack.as_str()].to_le_bytes());
        body.extend_from_slice(&loc.offset.to_le_bytes());
        body.extend_from_slice(&loc.len.to_le_bytes());
    }

    let mut roots: Vec<(&String, &RootEntry)> = view.roots.iter().collect();
    roots.sort_unstable_by_key(|(n, _)| n.as_str());
    body.extend_from_slice(
        &(u32::try_from(roots.len()).expect("root count fits u32")).to_le_bytes(),
    );
    for (name, entry) in roots {
        encode_root_into(&mut body, name, entry)?;
    }

    body.extend_from_slice(&view.approx_dead.to_le_bytes());
    Ok(body)
}

/// Root payload shared between the index body and ROOT pack records —
/// one encoding, so packs alone can rebuild the root table.
pub(crate) fn encode_root_payload(name: &str, entry: &RootEntry) -> Result<Vec<u8>> {
    let mut buf = Vec::with_capacity(2 + name.len() + 8 + 4 + entry.digests.len() * 32);
    encode_root_into(&mut buf, name, entry)?;
    Ok(buf)
}

fn encode_root_into(buf: &mut Vec<u8>, name: &str, entry: &RootEntry) -> Result<()> {
    let nlen = u16::try_from(name.len())
        .map_err(|_| Error::Corrupt(format!("root name too long: {name}")))?;
    buf.extend_from_slice(&nlen.to_le_bytes());
    buf.extend_from_slice(name.as_bytes());
    buf.extend_from_slice(&entry.last_use.to_le_bytes());
    buf.extend_from_slice(
        &(u32::try_from(entry.digests.len()).expect("root digest count fits u32")).to_le_bytes(),
    );
    for d in &entry.digests {
        buf.extend_from_slice(&d.0);
    }
    Ok(())
}

pub(crate) fn parse_root_payload(data: &[u8]) -> Option<(String, RootEntry)> {
    let mut cur = Cur { d: data };
    let nlen = cur.u16()? as usize;
    let name = std::str::from_utf8(cur.take(nlen)?).ok()?.to_string();
    let last_use = cur.u64()?;
    let count = cur.u32()? as usize;
    let mut digests = Vec::with_capacity(count);
    for _ in 0..count {
        digests.push(cur.digest()?);
    }
    if !cur.d.is_empty() {
        return None;
    }
    Some((name, RootEntry { last_use, digests }))
}

struct Cur<'a> {
    d: &'a [u8],
}

impl<'a> Cur<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.d.len() < n {
            return None;
        }
        let (head, tail) = self.d.split_at(n);
        self.d = tail;
        Some(head)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        Some(u16::from_le_bytes(self.take(2)?.try_into().ok()?))
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn digest(&mut self) -> Option<Digest> {
        let mut out = [0u8; 32];
        out.copy_from_slice(self.take(32)?);
        Some(Digest(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_roundtrips() {
        let dir = tempfile::tempdir().unwrap();
        let mut view = IndexView::default();
        let d1 = Digest::of(b"one");
        let d2 = Digest::of(b"two");
        view.blobs.insert(
            d1,
            BlobLoc {
                pack: "seg-a.pack".into(),
                offset: 42,
                len: 3,
                kind: Kind(0),
            },
        );
        view.blobs.insert(
            d2,
            BlobLoc {
                pack: "seg-b.pack".into(),
                offset: 7,
                len: 3,
                kind: Kind(2),
            },
        );
        view.roots.insert(
            "/rio/store/x".into(),
            RootEntry {
                last_use: 12345,
                digests: vec![d1, d2],
            },
        );
        view.approx_dead = 99;
        write(dir.path(), &view).unwrap();

        let loaded = load(dir.path()).unwrap().expect("index parses");
        assert_eq!(loaded.blobs, view.blobs);
        assert_eq!(loaded.roots, view.roots);
        assert_eq!(loaded.approx_dead, 99);
    }

    #[test]
    fn corrupt_index_loads_as_none() {
        let dir = tempfile::tempdir().unwrap();
        let view = IndexView::default();
        write(dir.path(), &view).unwrap();
        let path = dir.path().join(INDEX_FILE);
        let mut data = fs::read(&path).unwrap();
        let mid = data.len() / 2;
        data[mid] ^= 0xFF;
        fs::write(&path, &data).unwrap();
        assert!(load(dir.path()).unwrap().is_none());
    }

    #[test]
    fn root_payload_roundtrips() {
        let entry = RootEntry {
            last_use: 77,
            digests: vec![Digest::of(b"a"), Digest::of(b"b")],
        };
        let payload = encode_root_payload("/rio/store/abc-foo", &entry).unwrap();
        let (name, parsed) = parse_root_payload(&payload).unwrap();
        assert_eq!(name, "/rio/store/abc-foo");
        assert_eq!(parsed, entry);
    }
}
