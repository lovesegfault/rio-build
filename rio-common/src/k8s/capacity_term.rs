//! The ONE typed decoder for the `(hw_class_names, node_affinity)`
//! wire grammar's capacity requirement — shared by every consumer of
//! the selector-term alphabet (bug_063, R25: one wire grammar, ONE
//! decode law).
//!
//! The producer (`rio-scheduler::sla::solve::cells_to_selector_terms`)
//! emits exactly ONE `karpenter.sh/capacity-type` requirement per
//! term, operator `In`, single-valued, value from the shared
//! [`WireCapacity`] alphabet. This decoder is a TOTAL match over the
//! multiplicity × operator × arity product of that shape —
//! merged_bug_039 hardened the scheduler's consumer
//! (`decode_capacity_requirement`) against the
//! `find().and_then(values.first())` peek that read one cell out of
//! `In[spot,on-demand]`, decoded `NotIn[spot]` to its inverse, and
//! resolved duplicate requirements order-sensitively; one day later
//! the controller's `cells_of_checked` was built FROM the condemned
//! template cross-crate (the merged_bug_006 close) — single-site
//! hardening cannot survive template reuse, so the decode law now has
//! one home and both planes delegate.
//!
//! Refusals are TYPED, never truncations ([`CapacityTermDefect`], a
//! closed alphabet — R14): consumers fold the partition into their
//! own refusal lanes ([`CapacityTermDefect::is_structural`] splits
//! the scheduler's `ArmEchoSkewed` pairing lanes from its
//! `PlaneEntryUndecodable` shape lanes;
//! [`CapacityTermDefect::render`] reproduces the scheduler's refusal
//! strings byte-for-byte so the delegation is reader-invisible).

use crate::cell_wire::WireCapacity;

/// The `karpenter.sh/capacity-type` requirement key — the label this
/// decoder filters on. Re-exported by the consuming planes so the
/// grammar's key has one owner beside its decode law (the scheduler's
/// producer-side `LABEL_CAPACITY_TYPE` mirrors it; the producer-shape
/// round-trip tests in both crates pin the agreement end-to-end).
pub const CAPACITY_TYPE_LABEL: &str = "karpenter.sh/capacity-type";

/// One selector-term requirement, as a borrowed view — rio-common
/// sits BELOW rio-proto in the dependency graph, so the decoder takes
/// the shape, not the generated type; each consumer maps its
/// `NodeSelectorRequirement`s in.
pub struct TermRequirement<'a> {
    /// The requirement's label key (e.g. `karpenter.sh/capacity-type`).
    pub key: &'a str,
    /// The selector operator (`In`, `NotIn`, ...).
    pub operator: &'a str,
    /// The requirement's value list.
    pub values: &'a [String],
}

/// Why one aligned term's capacity requirement failed the typed
/// parse — the refusal partition, CLOSED (zero wildcard consumers).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CapacityTermDefect {
    /// No capacity requirement at all: structural pairing skew (the
    /// pair cannot name a cell).
    MissingRequirement,
    /// Empty values list: the existing empty-values structural law.
    EmptyValues,
    /// The requirement appears more than once in one term (the
    /// producer emits exactly one); `total` counts all appearances.
    DuplicateRequirement {
        /// How many times the requirement appears (≥ 2).
        total: usize,
    },
    /// Present but the operator is not the producer's `In`.
    NonInOperator {
        /// The offending operator, verbatim.
        operator: String,
        /// The requirement's values, comma-joined for the render.
        values_joined: String,
    },
    /// `In` with multiple values: names `count` cells, not one.
    MultiValue {
        /// The requirement's values, comma-joined for the render.
        values_joined: String,
        /// How many values the term carries (≥ 2).
        count: usize,
    },
    /// Single `In` value outside the [`WireCapacity`] alphabet.
    UnknownValue {
        /// The out-of-alphabet value, verbatim.
        value: String,
    },
}

impl CapacityTermDefect {
    /// True for the absence/pairing-structure half of the partition
    /// (the scheduler's `ArmEchoSkewed` lanes); false for the
    /// present-but-not-producer-shaped half (`PlaneEntryUndecodable`).
    pub fn is_structural(&self) -> bool {
        matches!(self, Self::MissingRequirement | Self::EmptyValues)
    }

    /// The rendered offending requirement — byte-identical to the
    /// strings the scheduler's pre-extraction decoder minted, so the
    /// delegation is invisible to every refusal reader (logs, error
    /// rows, tests).
    pub fn render(&self) -> String {
        match self {
            Self::MissingRequirement => format!("{CAPACITY_TYPE_LABEL} absent from term"),
            Self::EmptyValues => format!("{CAPACITY_TYPE_LABEL} In [] (empty values)"),
            Self::DuplicateRequirement { total } => format!(
                "{CAPACITY_TYPE_LABEL} appears {total} times in one term (the producer emits exactly one)"
            ),
            Self::NonInOperator {
                operator,
                values_joined,
            } => format!("{CAPACITY_TYPE_LABEL} {operator} [{values_joined}]"),
            Self::MultiValue {
                values_joined,
                count,
            } => format!(
                "{CAPACITY_TYPE_LABEL} In [{values_joined}] (multi-valued: names {count} cells, not one)"
            ),
            Self::UnknownValue { value } => value.clone(),
        }
    }
}

/// Decode one term's capacity requirement against the PRODUCER'S
/// shape: exactly one [`CAPACITY_TYPE_LABEL`] requirement, operator
/// `In`, single value from the shared [`WireCapacity`] alphabet
/// (`"spot"` / `"od"` / `"on-demand"`). Total over the
/// multiplicity × operator × arity product — every off-shape face is
/// a typed refusal, never a peek.
pub fn decode_capacity_term<'a, I>(reqs: I) -> Result<WireCapacity, CapacityTermDefect>
where
    I: IntoIterator<Item = TermRequirement<'a>>,
{
    let mut matches = reqs.into_iter().filter(|r| r.key == CAPACITY_TYPE_LABEL);
    let Some(req) = matches.next() else {
        return Err(CapacityTermDefect::MissingRequirement);
    };
    let dupes = matches.count();
    if dupes > 0 {
        return Err(CapacityTermDefect::DuplicateRequirement { total: dupes + 1 });
    }
    if req.operator != "In" {
        return Err(CapacityTermDefect::NonInOperator {
            operator: req.operator.to_string(),
            values_joined: req.values.join(", "),
        });
    }
    match req.values {
        [] => Err(CapacityTermDefect::EmptyValues),
        [value] => WireCapacity::parse(value).ok_or_else(|| CapacityTermDefect::UnknownValue {
            value: value.clone(),
        }),
        more => Err(CapacityTermDefect::MultiValue {
            values_joined: req.values.join(", "),
            count: more.len(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn term<'a>(reqs: &'a [(&'a str, &'a str, &'a [String])]) -> Vec<TermRequirement<'a>> {
        reqs.iter()
            .map(|(k, o, v)| TermRequirement {
                key: k,
                operator: o,
                values: v,
            })
            .collect()
    }

    fn vals(xs: &[&str]) -> Vec<String> {
        xs.iter().map(|s| (*s).into()).collect()
    }

    /// The full term alphabet, total: {In-single (each alphabet
    /// value), NotIn, multi-value, duplicate, malformed value,
    /// missing requirement, empty values} — every off-shape face is
    /// a TYPED refusal (W13-AJ's population, the shared half).
    #[test]
    fn decode_total_over_the_term_alphabet() {
        // The producer's shape: each alphabet value decodes.
        for (v, want) in [
            ("spot", WireCapacity::Spot),
            ("on-demand", WireCapacity::OnDemand),
            ("od", WireCapacity::OnDemand),
        ] {
            let vs = vals(&[v]);
            let got = decode_capacity_term(term(&[(CAPACITY_TYPE_LABEL, "In", &vs)]));
            assert_eq!(got, Ok(want), "{v}");
        }
        // NotIn: refused, NEVER inverted (the merged_bug_039 peek
        // decoded NotIn[spot] to Spot).
        let vs = vals(&["spot"]);
        let got = decode_capacity_term(term(&[(CAPACITY_TYPE_LABEL, "NotIn", &vs)]));
        assert_eq!(
            got,
            Err(CapacityTermDefect::NonInOperator {
                operator: "NotIn".into(),
                values_joined: "spot".into(),
            })
        );
        // Multi-value: refused, never first-only.
        let vs = vals(&["spot", "on-demand"]);
        let got = decode_capacity_term(term(&[(CAPACITY_TYPE_LABEL, "In", &vs)]));
        assert_eq!(
            got,
            Err(CapacityTermDefect::MultiValue {
                values_joined: "spot, on-demand".into(),
                count: 2,
            })
        );
        // Duplicate: refused, never order-sensitive.
        let a = vals(&["spot"]);
        let b = vals(&["on-demand"]);
        let got = decode_capacity_term(term(&[
            (CAPACITY_TYPE_LABEL, "In", &a),
            (CAPACITY_TYPE_LABEL, "In", &b),
        ]));
        assert_eq!(
            got,
            Err(CapacityTermDefect::DuplicateRequirement { total: 2 })
        );
        // Malformed value: typed refusal carrying the value.
        let vs = vals(&["spto"]);
        let got = decode_capacity_term(term(&[(CAPACITY_TYPE_LABEL, "In", &vs)]));
        assert_eq!(
            got,
            Err(CapacityTermDefect::UnknownValue {
                value: "spto".into()
            })
        );
        // Missing requirement / empty values: the structural half.
        let vs = vals(&["x"]);
        let got = decode_capacity_term(term(&[("other.io/key", "In", &vs)]));
        assert_eq!(got, Err(CapacityTermDefect::MissingRequirement));
        let vs = vals(&[]);
        let got = decode_capacity_term(term(&[(CAPACITY_TYPE_LABEL, "In", &vs)]));
        assert_eq!(got, Err(CapacityTermDefect::EmptyValues));
        // Foreign requirements beside the capacity one are ignored
        // (hw-class label requirements share the term).
        let hw = vals(&["mid-ebs-x86"]);
        let cap = vals(&["spot"]);
        let got = decode_capacity_term(term(&[
            ("rio.build/hw-class", "In", &hw),
            (CAPACITY_TYPE_LABEL, "In", &cap),
        ]));
        assert_eq!(got, Ok(WireCapacity::Spot));
    }

    /// The structural/shape partition is exactly the scheduler's
    /// ArmEchoSkewed vs PlaneEntryUndecodable split, and the rendered
    /// strings are byte-identical to the pre-extraction decoder's
    /// (W13-AJ2's shared half — the delegation is reader-invisible).
    #[test]
    fn refusal_partition_and_renders_byte_stable() {
        assert!(CapacityTermDefect::MissingRequirement.is_structural());
        assert!(CapacityTermDefect::EmptyValues.is_structural());
        assert!(!CapacityTermDefect::DuplicateRequirement { total: 2 }.is_structural());
        assert!(!CapacityTermDefect::UnknownValue { value: "x".into() }.is_structural());
        // The exact strings the scheduler minted before extraction.
        assert_eq!(
            CapacityTermDefect::DuplicateRequirement { total: 3 }.render(),
            "karpenter.sh/capacity-type appears 3 times in one term (the producer emits exactly one)"
        );
        assert_eq!(
            CapacityTermDefect::NonInOperator {
                operator: "NotIn".into(),
                values_joined: "spot".into(),
            }
            .render(),
            "karpenter.sh/capacity-type NotIn [spot]"
        );
        assert_eq!(
            CapacityTermDefect::MultiValue {
                values_joined: "spot, od".into(),
                count: 2,
            }
            .render(),
            "karpenter.sh/capacity-type In [spot, od] (multi-valued: names 2 cells, not one)"
        );
        assert_eq!(
            CapacityTermDefect::UnknownValue {
                value: "spto".into()
            }
            .render(),
            "spto"
        );
    }
}
