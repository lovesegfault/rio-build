//! File content serving and prefetch for the FUSE store.
//!
//! Handles `read`, `readlink`, and `readdir` operations. Serves content
//! from the local SSD cache, fetching from `StoreService.GetPath` on
//! cache miss.

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

use fuser::Errno;

/// Read a range of bytes from a file on disk.
pub fn read_file_range(path: &Path, offset: u64, size: usize) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let meta = file.metadata()?;
    let file_size = meta.len();

    if offset >= file_size {
        return Ok(Vec::new());
    }

    let read_size = size.min((file_size - offset) as usize);
    let mut buf = vec![0u8; read_size];

    file.seek(SeekFrom::Start(offset))?;
    let n = file.read(&mut buf)?;
    buf.truncate(n);

    Ok(buf)
}

/// Convert an `io::Error` to a FUSE `Errno`.
pub fn io_error_to_errno(e: &io::Error) -> Errno {
    match e.kind() {
        io::ErrorKind::NotFound => Errno::ENOENT,
        io::ErrorKind::PermissionDenied => Errno::EACCES,
        _ => Errno::EIO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_read_file_range_basic() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("data.bin");
        fs::write(&file_path, b"hello world").unwrap();

        let data = read_file_range(&file_path, 0, 5).unwrap();
        assert_eq!(&data, b"hello");

        let data = read_file_range(&file_path, 6, 5).unwrap();
        assert_eq!(&data, b"world");

        let data = read_file_range(&file_path, 100, 5).unwrap();
        assert!(data.is_empty());

        let data = read_file_range(&file_path, 0, 100).unwrap();
        assert_eq!(&data, b"hello world");
    }

    #[test]
    fn test_io_error_to_errno() {
        // Errno does not implement PartialEq, so compare via Debug format
        let not_found = io::Error::new(io::ErrorKind::NotFound, "not found");
        assert_eq!(
            format!("{:?}", io_error_to_errno(&not_found)),
            format!("{:?}", Errno::ENOENT)
        );

        let perm = io::Error::new(io::ErrorKind::PermissionDenied, "denied");
        assert_eq!(
            format!("{:?}", io_error_to_errno(&perm)),
            format!("{:?}", Errno::EACCES)
        );

        let other = io::Error::other("something");
        assert_eq!(
            format!("{:?}", io_error_to_errno(&other)),
            format!("{:?}", Errno::EIO)
        );
    }
}
