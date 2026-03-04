//! Macros for defining string-like newtypes with ergonomic trait impls.
//!
//! Two variants:
//! - [`string_newtype!`](crate::string_newtype) — `String` backing. Clone = allocation.
//! - [`arc_string_newtype!`](crate::arc_string_newtype) — `Arc<str>` backing. Clone = atomic refcount
//!   bump. Use for identifiers that are cloned frequently in hot paths
//!   (DAG edge storage, queue entries, cascade walks).
//!
//! Both expose the same trait surface, so swapping a type between them
//! is a one-line change at the definition site.

/// Define a String-backed newtype with all the ergonomic traits for
/// HashMap keys, tracing fields, and test assertions.
///
/// Generates: `new`/`as_str`/`into_inner`, `From<String>`, `From<&str>`,
/// `Display`, `Deref<Target=str>`, `Borrow<str>`, `Borrow<String>`,
/// `AsRef<str>`, `PartialEq<str>`, `PartialEq<&str>`, `PartialEq<String>`.
#[macro_export]
macro_rules! string_newtype {
    ($(#[$meta:meta])* $vis:vis struct $name:ident) => {
        $(#[$meta])*
        // Ord/PartialOrd: needed for BinaryHeap keys (phase2c D5).
        // Lexicographic on the inner String — fine, only used as a
        // tiebreak in the heap (priority then sequence then hash).
        #[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
        $vis struct $name(String);

        impl $name {
            pub fn new(s: impl Into<String>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
            pub fn into_inner(self) -> String { self.0 }
        }
        impl From<String> for $name {
            fn from(s: String) -> Self { Self(s) }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(s.into()) }
        }
        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl ::std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str { &self.0 }
        }
        impl ::std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str { &self.0 }
        }
        impl ::std::borrow::Borrow<String> for $name {
            fn borrow(&self) -> &String { &self.0 }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }
        impl PartialEq<str> for $name {
            fn eq(&self, o: &str) -> bool { self.0 == o }
        }
        impl PartialEq<&str> for $name {
            fn eq(&self, o: &&str) -> bool { self.0 == *o }
        }
        impl PartialEq<String> for $name {
            fn eq(&self, o: &String) -> bool { &self.0 == o }
        }
    };
}

/// Define an `Arc<str>`-backed newtype with the same trait surface as
/// [`string_newtype!`](crate::string_newtype).
///
/// The key difference is `Clone`: `Arc<str>` clones via atomic refcount
/// increment — no allocation, no memcpy. For identifiers cloned ~40×/merge
/// in hot paths (DAG edge storage, priority queue entries), this replaces
/// heap churn with a single atomic op.
///
/// `Hash` and `Eq` are structural (compare str contents, not Arc pointer).
/// Two `X::from("same")` built independently are `==` and hash-equal even
/// though they're distinct allocations — HashMap/HashSet semantics are
/// identical to the `String` variant.
///
/// `into_inner()` returns `Arc<str>` (not `String`) — if you need owned
/// `String`, use `.to_string()` (allocates).
#[macro_export]
macro_rules! arc_string_newtype {
    ($(#[$meta:meta])* $vis:vis struct $name:ident) => {
        $(#[$meta])*
        // NOT deriving Hash/PartialEq/Eq/Ord: those would compare Arc
        // POINTERS, not contents. Manual impls below hash/compare the str.
        #[derive(Debug, Clone)]
        $vis struct $name(::std::sync::Arc<str>);

        impl $name {
            pub fn new(s: impl Into<::std::sync::Arc<str>>) -> Self { Self(s.into()) }
            pub fn as_str(&self) -> &str { &self.0 }
            /// Returns the inner `Arc<str>`. For owned `String`, use `.to_string()`.
            pub fn into_inner(self) -> ::std::sync::Arc<str> { self.0 }
            /// Pointer-equality check on the underlying Arc. Two clones of
            /// the same `$name` are ptr_eq; two independent `$name::from("x")`
            /// are NOT. Useful for verifying interning invariants in tests.
            /// Not `#[cfg(test)]` because downstream crates' tests need it
            /// (deps are built with `cfg(not(test))`).
            pub fn ptr_eq(a: &Self, b: &Self) -> bool {
                ::std::sync::Arc::ptr_eq(&a.0, &b.0)
            }
        }
        impl From<String> for $name {
            // Arc::from(String) goes String -> Box<str> -> Arc<str>.
            // The Box<str> -> Arc<str> step copies into a new alloc (to
            // prepend the refcount header), but it's still one copy,
            // not two.
            fn from(s: String) -> Self { Self(::std::sync::Arc::from(s)) }
        }
        impl From<&str> for $name {
            fn from(s: &str) -> Self { Self(::std::sync::Arc::from(s)) }
        }
        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(&self.0)
            }
        }
        impl ::std::ops::Deref for $name {
            type Target = str;
            fn deref(&self) -> &str { &self.0 }
        }
        impl ::std::borrow::Borrow<str> for $name {
            fn borrow(&self) -> &str { &self.0 }
        }
        impl AsRef<str> for $name {
            fn as_ref(&self) -> &str { &self.0 }
        }
        // Hash/Eq/Ord on the str CONTENTS — same semantics as String-backed.
        // Critical: HashMap<X, _>::get(&str) via Borrow<str> requires that
        // X::hash("foo") == "foo".hash() — which str::hash guarantees.
        impl ::std::hash::Hash for $name {
            fn hash<H: ::std::hash::Hasher>(&self, state: &mut H) {
                (*self.0).hash(state)
            }
        }
        impl PartialEq for $name {
            fn eq(&self, o: &Self) -> bool { *self.0 == *o.0 }
        }
        impl Eq for $name {}
        impl PartialOrd for $name {
            fn partial_cmp(&self, o: &Self) -> Option<::std::cmp::Ordering> {
                Some(self.cmp(o))
            }
        }
        impl Ord for $name {
            fn cmp(&self, o: &Self) -> ::std::cmp::Ordering {
                (*self.0).cmp(&*o.0)
            }
        }
        impl PartialEq<str> for $name {
            fn eq(&self, o: &str) -> bool { *self.0 == *o }
        }
        impl PartialEq<&str> for $name {
            fn eq(&self, o: &&str) -> bool { *self.0 == **o }
        }
        impl PartialEq<String> for $name {
            fn eq(&self, o: &String) -> bool { *self.0 == **o }
        }
    };
}

// ---------------------------------------------------------------------------
// Shared newtype instances
// ---------------------------------------------------------------------------
//
// These identifiers cross crate boundaries (scheduler <-> worker <-> proto),
// so they live here rather than in a single consumer crate.
//
// Both use Arc<str> backing: the scheduler's DAG clones DrvHash ~40×/merge
// for edge storage, priority queue entries, and cascade walks. WorkerId is
// cloned per-heartbeat into reconciled sets. Arc makes those clones free.

arc_string_newtype! {
    /// Derivation hash newtype. The 32-char nixbase32 hash part of a .drv
    /// store path, plus the name component (`{hash}-{name}.drv`).
    ///
    /// Distinct from `drv_path` (full `/nix/store/HASH-name.drv` string).
    /// Prevents accidental swaps — see the post-2a `drv_key` rename where
    /// `handle_completion` took a `drv_hash: String` that was sometimes
    /// actually a path.
    ///
    /// Implements `Borrow<str>` so `HashMap<DrvHash, _>::get(&str)` works.
    /// `Arc<str>` backing — clone is an atomic refcount bump, not alloc.
    pub struct DrvHash
}

arc_string_newtype! {
    /// Worker identifier newtype (e.g., `"worker-0"` or a UUID).
    /// `Arc<str>` backing — clone is an atomic refcount bump, not alloc.
    pub struct WorkerId
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn arc_newtype_eq_across_independent_arcs() {
        // Two DrvHash built from the same &str are distinct Arcs but
        // structurally equal. Critical for HashMap/HashSet semantics.
        let a = DrvHash::from("abc123-foo.drv");
        let b = DrvHash::from("abc123-foo.drv");
        assert_eq!(a, b);
        assert!(!DrvHash::ptr_eq(&a, &b), "distinct allocations");
        // Clone IS ptr-equal — that's the point of Arc backing.
        let c = a.clone();
        assert!(DrvHash::ptr_eq(&a, &c));
        assert_eq!(a, c);
    }

    #[test]
    fn arc_newtype_hashmap_lookup_by_str() {
        // Borrow<str> + consistent Hash impl → lookup by &str works.
        let mut m: HashMap<DrvHash, i32> = HashMap::new();
        m.insert(DrvHash::from("key-one"), 1);
        m.insert(DrvHash::from("key-two"), 2);
        assert_eq!(m.get("key-one"), Some(&1));
        assert_eq!(m.get("key-two"), Some(&2));
        assert_eq!(m.get("missing"), None);
        // Also works with an independent DrvHash (structural eq, not ptr).
        assert_eq!(m.get(&DrvHash::from("key-one")), Some(&1));
    }

    #[test]
    fn arc_newtype_trait_surface() {
        let h = DrvHash::from("surface-test");
        // Display
        assert_eq!(h.to_string(), "surface-test");
        // Deref<Target=str>
        let s: &str = &h;
        assert_eq!(s, "surface-test");
        // as_str
        assert_eq!(h.as_str(), "surface-test");
        // AsRef<str>
        assert_eq!(<DrvHash as AsRef<str>>::as_ref(&h), "surface-test");
        // PartialEq<&str>, PartialEq<str>, PartialEq<String>
        assert_eq!(h, "surface-test");
        assert_eq!(h, *"surface-test");
        assert_eq!(h, String::from("surface-test"));
        // From<String>
        let h2 = DrvHash::from(String::from("surface-test"));
        assert_eq!(h, h2);
        // Ord (lexicographic). Bind to locals so clippy::cmp_owned doesn't
        // suggest replacing with a str comparison — we're exercising
        // Ord<DrvHash>, not PartialEq<str>.
        let (lo, hi) = (DrvHash::from("aaa"), DrvHash::from("bbb"));
        assert!(lo < hi);
    }
}
