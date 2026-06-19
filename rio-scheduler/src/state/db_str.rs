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
//! Not `strum`: strum's `Display`/`EnumString` defaults are
//! variant-case (`"Queued"`), and the PG TEXT values are snake-case
//! plus carry per-variant `parse_err` types. The encode/decode is ~10
//! lines of macro vs `#[strum(serialize = "...")]` on every variant of
//! every enum; the macro keeps the variant↔string list in one place.

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

/// "TEXT column → `db_str_enum`, log+default on alphabet drift" idiom
/// (sh-044 r1): every recovered-row mirror field that decodes a
/// `db_str_enum!` from a PG TEXT column hits the same fork — a CHECK
/// drift (deploy bug) lands in the unknown arm and the in-memory
/// mirror falls back to a conservative default. Hand-rolled copies
/// drifted log levels and message tokens (`warn!` vs `error!`,
/// "unknown … defaulting" vs "078 CHECK drift") so a Splunk search
/// for CHECK-drift events couldn't anchor on a common token. One
/// helper, one `db_str_enum_drift` token. The conservative default is
/// caller-supplied (not `Default::default()`): each site documents
/// which arm is conservative for its read (e.g. `JobOrigin::
/// CacheOpportunity` for the age-out label, `PriorityClass::default()`
/// for the recovery rebuild).
pub(crate) fn parse_or_warn_default<T>(col: &'static str, raw: &str, default: T) -> T
where
    T: std::str::FromStr,
{
    raw.parse().unwrap_or_else(|_| {
        tracing::error!(
            column = col,
            raw,
            "db_str_enum_drift: TEXT column not in the known alphabet \
             (CHECK constraint drift); defaulting the in-memory mirror"
        );
        default
    })
}
