//! Inode-to-path bidirectional map with kernel lookup refcounting.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use fuser::INodeNo;

/// Bidirectional inode-to-path map with kernel lookup refcounting, protected
/// by a single lock.
///
/// FUSE protocol: each successful lookup/create reply increments the kernel's
/// refcount for that inode by 1. The kernel sends `forget(ino, n)` when it
/// drops n references. When the count hits zero, the filesystem may free
/// resources associated with the inode.
pub(super) struct InodeMap {
    inode_to_path: HashMap<u64, PathBuf>,
    path_to_inode: HashMap<PathBuf, u64>,
    /// Kernel lookup refcount per inode. Incremented on each reply.entry(),
    /// decremented (by n) on forget(ino, n). When it hits zero, the inode
    /// entry is removed from all maps (except ROOT, which is never forgotten).
    nlookup: HashMap<u64, u64>,
    next_inode: u64,
}

impl InodeMap {
    pub(super) fn new(root_path: PathBuf) -> Self {
        let mut inode_to_path = HashMap::new();
        let mut path_to_inode = HashMap::new();
        inode_to_path.insert(INodeNo::ROOT.0, root_path.clone());
        path_to_inode.insert(root_path, INodeNo::ROOT.0);
        Self {
            inode_to_path,
            path_to_inode,
            nlookup: HashMap::new(),
            next_inode: 2,
        }
    }

    pub(super) fn get_or_create(&mut self, path: PathBuf) -> u64 {
        if let Some(&ino) = self.path_to_inode.get(&path) {
            return ino;
        }
        let ino = self.next_inode;
        self.next_inode += 1;
        self.inode_to_path.insert(ino, path.clone());
        self.path_to_inode.insert(path, ino);
        ino
    }

    /// Read-only lookup: returns the inode if path is already tracked.
    pub(super) fn get_existing(&self, path: &Path) -> Option<u64> {
        self.path_to_inode.get(path).copied()
    }

    /// Increment the kernel lookup refcount for an inode. Call this exactly
    /// once per successful reply.entry().
    pub(super) fn increment_lookup(&mut self, ino: u64) {
        *self.nlookup.entry(ino).or_insert(0) += 1;
    }

    /// Decrement the kernel lookup refcount by `n`. If it reaches zero (and
    /// this is not ROOT), remove the inode from all maps. Returns true iff
    /// the inode was removed.
    pub(super) fn forget(&mut self, ino: u64, n: u64) -> bool {
        if ino == INodeNo::ROOT.0 {
            return false;
        }
        let Some(count) = self.nlookup.get_mut(&ino) else {
            // Kernel sent forget for an inode we don't track (e.g., created
            // via readdir without a subsequent lookup). Nothing to do.
            return false;
        };
        *count = count.saturating_sub(n);
        if *count == 0 {
            self.nlookup.remove(&ino);
            if let Some(path) = self.inode_to_path.remove(&ino) {
                self.path_to_inode.remove(&path);
            }
            true
        } else {
            false
        }
    }

    pub(super) fn real_path(&self, ino: u64) -> Option<PathBuf> {
        self.inode_to_path.get(&ino).cloned()
    }

    pub(super) fn parent_inode(&self, dir_path: &Path) -> u64 {
        dir_path
            .parent()
            .and_then(|p| self.path_to_inode.get(p).copied())
            .unwrap_or(INodeNo::ROOT.0)
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.inode_to_path.len()
    }
}

/// High bit mask for ephemeral inodes. Persistent inodes start at 2 and
/// grow sequentially; setting bit 63 guarantees no collision (would need
/// 2^63 sequential allocations to overlap).
pub(super) const EPHEMERAL_INODE_BIT: u64 = 1u64 << 63;

/// Compute a deterministic ephemeral inode from a path.
///
/// Used for readdir entries that have not been lookup()'d. FUSE does not
/// require readdir inode numbers to match lookup inodes — they are
/// informational (ls -i). Applications doing hardlink detection use
/// lookup/getattr inodes. We don't implement readdirplus, so there's no
/// attribute caching from readdir.
///
/// If a readdir'd path is later lookup()'d, it gets a real persistent inode
/// via get_or_create_inode_for_lookup — the ephemeral one is simply forgotten.
pub(super) fn ephemeral_inode(path: &Path) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    path.hash(&mut hasher);
    hasher.finish() | EPHEMERAL_INODE_BIT
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_inode_map_new() {
        let root = PathBuf::from("/nix/store");
        let map = InodeMap::new(root.clone());

        assert_eq!(map.real_path(INodeNo::ROOT.0), Some(root));
        assert!(map.real_path(2).is_none());
    }

    #[test]
    fn test_inode_map_allocation() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);

        let p1 = PathBuf::from("/nix/store/abc-hello");
        let p2 = PathBuf::from("/nix/store/def-world");

        let ino1 = map.get_or_create(p1.clone());
        let ino2 = map.get_or_create(p2);

        assert_ne!(ino1, ino2);
        assert_eq!(map.get_or_create(p1), ino1);
    }

    #[test]
    fn test_inode_map_parent() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);

        let child = PathBuf::from("/nix/store/abc-hello");
        let _ino = map.get_or_create(child.clone());

        assert_eq!(map.parent_inode(&child), INodeNo::ROOT.0);
    }

    #[test]
    fn test_inode_map_forget_removes_when_zero() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);
        let p = PathBuf::from("/nix/store/abc-hello");
        let ino = map.get_or_create(p);
        map.increment_lookup(ino);
        map.increment_lookup(ino);
        // nlookup = 2; forget(1) -> nlookup = 1, not removed
        assert!(!map.forget(ino, 1));
        assert!(map.real_path(ino).is_some());
        // forget(1) -> nlookup = 0, removed
        assert!(map.forget(ino, 1));
        assert!(map.real_path(ino).is_none());
        assert_eq!(map.len(), 1, "only ROOT should remain");
    }

    #[test]
    fn test_inode_map_forget_keeps_when_nonzero() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);
        let ino = map.get_or_create(PathBuf::from("/nix/store/abc"));
        map.increment_lookup(ino);
        map.increment_lookup(ino);
        map.increment_lookup(ino);
        // nlookup = 3; forget(2) -> nlookup = 1, not removed
        assert!(!map.forget(ino, 2));
        assert!(map.real_path(ino).is_some());
        assert_eq!(map.len(), 2, "ROOT + abc");
    }

    #[test]
    fn test_inode_map_forget_never_removes_root() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root.clone());
        map.increment_lookup(INodeNo::ROOT.0);
        // Even if nlookup hits zero, ROOT is never removed.
        assert!(!map.forget(INodeNo::ROOT.0, 100));
        assert_eq!(map.real_path(INodeNo::ROOT.0), Some(root));
    }

    #[test]
    fn test_inode_map_forget_untracked_inode() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root);
        // Forget for an inode we never tracked (e.g., an ephemeral readdir
        // inode, or a stale kernel call) must be a no-op, not a panic.
        let ephemeral = ephemeral_inode(Path::new("/nix/store/never-looked-up"));
        assert!(!map.forget(ephemeral, 1));
        assert!(map.real_path(ephemeral).is_none()); // never in the map
    }

    #[test]
    fn test_ephemeral_inode_high_bit_set() {
        let paths = [
            "/nix/store/abc-hello",
            "/nix/store/def-world",
            "/tmp/anything",
        ];
        for p in paths {
            let ino = ephemeral_inode(Path::new(p));
            assert!(
                ino & EPHEMERAL_INODE_BIT != 0,
                "ephemeral inode for {p} must have high bit set: {ino:#x}"
            );
            // And must not be 0 or ROOT (1).
            assert!(ino > 1);
        }
    }

    #[test]
    fn test_ephemeral_inode_deterministic() {
        let p = Path::new("/nix/store/same-path");
        assert_eq!(ephemeral_inode(p), ephemeral_inode(p));
        // Different paths -> (almost certainly) different inodes.
        let other = Path::new("/nix/store/other-path");
        assert_ne!(ephemeral_inode(p), ephemeral_inode(other));
    }

    #[test]
    fn test_inode_map_get_existing() {
        let root = PathBuf::from("/nix/store");
        let mut map = InodeMap::new(root.clone());
        // ROOT exists.
        assert_eq!(map.get_existing(&root), Some(INodeNo::ROOT.0));
        // Unknown path does not.
        assert_eq!(map.get_existing(Path::new("/nix/store/nope")), None);
        // After creating, it does.
        let ino = map.get_or_create(PathBuf::from("/nix/store/yes"));
        assert_eq!(map.get_existing(Path::new("/nix/store/yes")), Some(ino));
    }
}
