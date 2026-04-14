//! Declarative `newtype!` macro for unit-safe scalar wrappers.
//!
//! Emits a transparent tuple struct with `Debug`/`Clone`/`Copy`/`PartialEq`/
//! `Serialize`/`Deserialize` plus bidirectional `From`, then opt-in arithmetic
//! and ordering traits. Used by ADR-023 sizing types (`RawCores`, `RefSeconds`,
//! `MemBytes`, …) so unit confusion is a compile error rather than a wrong
//! number.

/// Define a `Copy` newtype around a scalar with opt-in trait impls.
///
/// ```
/// rio_common::newtype!(pub Meters(f64): Display, Add, Sub, Mul<f64>, Div<f64>, Ord);
/// let d = Meters(3.0) + Meters(2.0);
/// assert_eq!(d.0, 5.0);
/// ```
///
/// Always derived: `Debug`, `Clone`, `Copy`, `PartialEq`, `serde::Serialize`,
/// `serde::Deserialize` (`#[serde(transparent)]`), and `From` both ways.
///
/// Opt-in traits (after `:`): `Display`, `Add`, `Sub`, `Mul<S>`, `Div<S>`,
/// `Ord`. `Ord` also supplies `Eq`/`PartialOrd` and **panics on NaN** — the
/// sizing model never produces NaN, and a loud panic beats a silently wrong
/// sort.
#[macro_export]
macro_rules! newtype {
    ($vis:vis $name:ident($inner:ty) $(: $($trait:tt $(<$g:ty>)?),+)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, ::serde::Serialize, ::serde::Deserialize)]
        #[serde(transparent)]
        $vis struct $name(pub $inner);
        impl From<$inner> for $name { fn from(v: $inner) -> Self { Self(v) } }
        impl From<$name> for $inner { fn from(v: $name) -> Self { v.0 } }
        $($($crate::__newtype_trait!($name, $inner, $trait $(<$g>)?);)+)?
    };
}

/// Implementation detail of [`newtype!`]. Not part of the public API.
#[doc(hidden)]
#[macro_export]
macro_rules! __newtype_trait {
    ($n:ident, $i:ty, Display) => {
        impl ::core::fmt::Display for $n {
            fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                self.0.fmt(f)
            }
        }
    };
    ($n:ident, $i:ty, Add) => {
        impl ::core::ops::Add for $n {
            type Output = Self;
            fn add(self, r: Self) -> Self {
                Self(self.0 + r.0)
            }
        }
    };
    ($n:ident, $i:ty, Sub) => {
        impl ::core::ops::Sub for $n {
            type Output = Self;
            fn sub(self, r: Self) -> Self {
                Self(self.0 - r.0)
            }
        }
    };
    ($n:ident, $i:ty, Mul<$g:ty>) => {
        impl ::core::ops::Mul<$g> for $n {
            type Output = Self;
            fn mul(self, r: $g) -> Self {
                Self(self.0 * r)
            }
        }
    };
    ($n:ident, $i:ty, Div<$g:ty>) => {
        impl ::core::ops::Div<$g> for $n {
            type Output = Self;
            fn div(self, r: $g) -> Self {
                Self(self.0 / r)
            }
        }
    };
    ($n:ident, $i:ty, Ord) => {
        impl Eq for $n {}
        // Delegates to the inner `partial_cmp` (not `Some(self.cmp)`) so float
        // wrappers compile; `Ord` then unwraps with a NaN panic. Intentional.
        #[allow(clippy::non_canonical_partial_ord_impl)]
        impl PartialOrd for $n {
            fn partial_cmp(&self, o: &Self) -> Option<::core::cmp::Ordering> {
                self.0.partial_cmp(&o.0)
            }
        }
        impl Ord for $n {
            fn cmp(&self, o: &Self) -> ::core::cmp::Ordering {
                self.partial_cmp(o).expect("newtype Ord on NaN")
            }
        }
    };
}

#[cfg(test)]
mod tests {
    newtype!(pub Meters(f64): Display, Add, Sub, Mul<f64>, Div<f64>, Ord);
    newtype!(pub Count(u32): Display, Add, Ord);

    #[test]
    fn arithmetic_preserves_type() {
        let a = Meters(3.0) + Meters(2.0);
        assert_eq!(a.0, 5.0);
    }

    #[test]
    fn scalar_mul() {
        assert_eq!((Meters(3.0) * 2.0).0, 6.0);
    }

    #[test]
    fn scalar_div_and_sub() {
        assert_eq!((Meters(9.0) / 3.0 - Meters(1.0)).0, 2.0);
    }

    #[test]
    fn ord_and_display() {
        let mut v = [Meters(2.0), Meters(1.0)];
        v.sort();
        assert_eq!(v, [Meters(1.0), Meters(2.0)]);
        assert_eq!(Count(3).to_string(), "3");
        assert!(Count(1) + Count(2) > Count(2));
    }

    #[test]
    fn from_roundtrip() {
        let m: Meters = 4.5.into();
        let f: f64 = m.into();
        assert_eq!(f, 4.5);
    }

    #[test]
    fn serde_transparent() {
        assert_eq!(serde_json::to_string(&Count(7)).unwrap(), "7");
        assert_eq!(serde_json::from_str::<Count>("7").unwrap(), Count(7));
    }
}
