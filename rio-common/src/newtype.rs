//! Macro for defining String-backed newtypes with ergonomic trait impls.

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
