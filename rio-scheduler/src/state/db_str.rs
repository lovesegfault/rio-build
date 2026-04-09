//! `db_str_enum!` — generates `as_str`/`Display`/`FromStr`/`ALL`
//! boilerplate for enums that round-trip through PG TEXT columns.
//!
//! Three scheduler enums (`DerivationStatus`, `PriorityClass`,
//! `AssignmentStatus`) carried hand-rolled
//! `match self { Variant => "variant", ... }` ×2 for encode/decode.
//! Adding a variant meant updating four places (as_str, FromStr, ALL,
//! the golden snapshot); missing one was a runtime `UnknownStatus`
//! instead of a compile error. The macro keeps the variant↔string
//! list in one place per enum.
//!
//! `BuildState` is a re-exported proto type so orphan rules prevent
//! `FromStr`/inherent `as_str`; it keeps the `BuildStateExt` trait
//! pattern.
//!
//! Not `strum`: the encode/decode is ~10 lines of macro vs a new
//! workspace dep + derive-macro compile time + Cargo.json regen. The
//! enums are stable enough that the macro pays for itself.

/// Generate `as_str`, `Display`, `ALL`, and (optionally) `FromStr` for
/// a local enum whose variants map to PG TEXT values.
///
/// Two forms:
///   - `parse_err = |s| Expr` — also generates `FromStr` with the given
///     error constructor (closure: `String -> E`).
///   - no `parse_err` — `as_str`/`Display`/`ALL` only (for enums that
///     are write-only to PG, e.g. `AssignmentStatus`).
macro_rules! db_str_enum {
    // With FromStr.
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $str:literal ),+ $(,)?
        }
        parse_err($bad:ident) = $err_ty:ty : $err:expr;
    ) => {
        db_str_enum! {
            $(#[$meta])*
            $vis enum $name { $( $(#[$vmeta])* $variant = $str ),+ }
        }
        impl ::std::str::FromStr for $name {
            type Err = $err_ty;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $( $str => Ok(Self::$variant), )+
                    $bad => Err($err),
                }
            }
        }
    };
    // Base form: as_str + Display + ALL only.
    (
        $(#[$meta:meta])*
        $vis:vis enum $name:ident {
            $( $(#[$vmeta:meta])* $variant:ident = $str:literal ),+ $(,)?
        }
    ) => {
        $(#[$meta])*
        $vis enum $name {
            $( $(#[$vmeta])* $variant ),+
        }
        impl $name {
            /// PG TEXT repr (lowercase / snake_case).
            pub fn as_str(self) -> &'static str {
                match self { $( Self::$variant => $str ),+ }
            }
            /// All variants in declaration order. Used by exhaustive
            /// roundtrip tests and golden-snapshot checks.
            #[allow(dead_code)]
            pub const ALL: &[Self] = &[ $( Self::$variant ),+ ];
        }
        impl ::std::fmt::Display for $name {
            fn fmt(&self, f: &mut ::std::fmt::Formatter<'_>) -> ::std::fmt::Result {
                f.write_str(self.as_str())
            }
        }
    };
}

pub(crate) use db_str_enum;
