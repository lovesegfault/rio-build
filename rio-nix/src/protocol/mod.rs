/// Define a `#[repr(u64)]` wire enum with a generated `TryFrom<u64>` impl
/// (and optional `name()` method) from a single discriminant list.
///
/// Two forms: `Variant = N` (just enum + `TryFrom`) and `Variant = N => "name"`
/// (also generates `fn name(&self) -> &'static str`). Adding a variant means
/// touching one line instead of three.
macro_rules! wire_enum {
    ($(#[$m:meta])* $vis:vis enum $T:ident { $($V:ident = $n:literal => $name:literal),* $(,)? }) => {
        wire_enum!(@def $(#[$m])* $vis $T; $($V = $n),*);
        impl $T {
            #[allow(dead_code)]
            pub fn name(&self) -> &'static str { match self { $($T::$V => $name,)* } }
        }
    };
    ($(#[$m:meta])* $vis:vis enum $T:ident { $($V:ident = $n:literal),* $(,)? }) => {
        wire_enum!(@def $(#[$m])* $vis $T; $($V = $n),*);
    };
    (@def $(#[$m:meta])* $vis:vis $T:ident; $($V:ident = $n:literal),*) => {
        $(#[$m])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        #[repr(u64)]
        $vis enum $T { $($V = $n,)* }
        impl ::std::convert::TryFrom<u64> for $T {
            type Error = u64;
            fn try_from(v: u64) -> ::std::result::Result<Self, u64> {
                match v { $($n => Ok($T::$V),)* other => Err(other) }
            }
        }
    };
}

pub mod build;
pub mod client;
pub mod derived_path;
pub mod handshake;
pub mod opcodes;
pub mod pathinfo;
pub mod stderr;
pub mod wire;
