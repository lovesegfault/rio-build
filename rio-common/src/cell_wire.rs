//! Single-sourced `"h:cap[@epoch]"` cell-event wire grammar.
//!
//! The controller (producer: `AckSpawnedIntents.{unfulfillable_cells,
//! registered_cells}`, `ObservedInstanceType.cell`) and the scheduler
//! (consumer: the ack apply plan; reverse direction:
//! `GetSpawnIntentsResponse.ice_masked_cells`) used to open-code the
//! capacity vocabulary and the `':'` separator independently — four
//! mirrored literal sites whose agreement was checked only by
//! same-crate round-trip tests (bug_094). This module is the one
//! place the alphabet lives; both crates' `CapacityType` codecs pin
//! to it, so a vocabulary change is a one-site const edit or a
//! compile error, never a silent cross-crate divergence.
//!
//! Grammar (total over the decode side — R9 read-side-first):
//!
//! ```text
//! cell-event  = hw-class ':' capacity [ '@' epoch ]
//! hw-class    = any string (MAY contain ':'; the LAST ':' separates)
//! capacity    = "spot" | "od" | "on-demand"
//! epoch       = u64 decimal (producer evidence epoch, millis)
//! ```
//!
//! `"od"` is the canonical wire/PG form (migration 059/060 `CHECK
//! (capacity_type IN ('spot','od'))`, controller `Cell::Display`);
//! `"on-demand"` is the Karpenter `karpenter.sh/capacity-type` label
//! form (scheduler `cell_label`, `ice_masked_cells`). Decode accepts
//! all three tokens; each encode direction keeps its existing
//! canonical form byte-identical.
//!
//! The optional `@<epoch>` suffix carries the producer's evidence
//! epoch (merged_bug_008): a per-cell monotonic ordering token minted
//! once per buffered evidence event, stable across redeliveries of
//! that event, letting the consumer no-op redelivery and reorder.
//! Epoch-less entries take the legacy semantics — the lane stays as
//! decode totality over the grammar, not version-skew tolerance.

/// Canonical wire/PG capacity token for spot capacity.
pub const CAPACITY_SPOT: &str = "spot";
/// Canonical wire/PG capacity token for on-demand capacity
/// (migration 059/060 `CHECK` form, controller `Cell::Display` form).
pub const CAPACITY_OD: &str = "od";
/// Karpenter `karpenter.sh/capacity-type` label value for on-demand
/// capacity (scheduler `cell_label` form; never written to PG).
pub const CAPACITY_ON_DEMAND: &str = "on-demand";
/// Separates `hw-class` from `capacity` (the LAST occurrence wins —
/// hw-class names containing `':'` are tolerated).
pub const CELL_SEP: char = ':';
/// Separates the capacity token from the optional evidence epoch.
pub const EPOCH_SEP: char = '@';

/// The closed capacity alphabet in wire form. Each daemon crate keeps
/// its own local `CapacityType` (serde/derive surfaces differ) but
/// pins its codec through this enum, so the alphabet itself has one
/// owner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum WireCapacity {
    /// Interruptible spot capacity (`"spot"` on the wire).
    Spot,
    /// On-demand capacity (`"od"` on the wire).
    OnDemand,
}

impl WireCapacity {
    /// Canonical wire/PG token (`"spot"` / `"od"`).
    pub const fn wire_str(self) -> &'static str {
        match self {
            Self::Spot => CAPACITY_SPOT,
            Self::OnDemand => CAPACITY_OD,
        }
    }

    /// Karpenter `karpenter.sh/capacity-type` label value
    /// (`"spot"` / `"on-demand"`).
    pub const fn karpenter_label(self) -> &'static str {
        match self {
            Self::Spot => CAPACITY_SPOT,
            Self::OnDemand => CAPACITY_ON_DEMAND,
        }
    }

    /// Total decode over the capacity alphabet: accepts the wire/PG
    /// form AND the Karpenter label form. Anything else is `None` —
    /// callers on refusal lanes turn that into a typed error.
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            CAPACITY_SPOT => Some(Self::Spot),
            CAPACITY_OD | CAPACITY_ON_DEMAND => Some(Self::OnDemand),
            _ => None,
        }
    }
}

/// Producer evidence epoch (merged_bug_008): a per-cell monotonic
/// ordering token in a SINGLE mint lineage (the controller's
/// reconciler-owned evidence buffer). Minted once per buffered
/// evidence event and stable across redeliveries of that event;
/// the consumer applies a cell-event iff `epoch > last_applied[cell]`
/// — token vs token from the same lineage, never compared against
/// any other clock. Wire form: decimal millis after [`EPOCH_SEP`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EvidenceEpoch(pub u64);

impl std::fmt::Display for EvidenceEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// Why a cell-event string failed to decode. Carried into typed
/// refusals (the scheduler's ack apply plan) so an undecodable entry
/// is a loud producer-skew signal, never a silent drop (bug_094).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellDecodeError {
    /// No [`CELL_SEP`] in the entry.
    MissingSeparator {
        /// The undecodable entry, verbatim.
        entry: String,
    },
    /// The capacity token is outside the closed alphabet.
    UnknownCapacity {
        /// The undecodable entry, verbatim.
        entry: String,
    },
    /// An [`EPOCH_SEP`] suffix is present but not a `u64`.
    BadEpoch {
        /// The undecodable entry, verbatim.
        entry: String,
    },
}

impl std::fmt::Display for CellDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingSeparator { entry } => {
                write!(f, "cell event {entry:?}: missing {CELL_SEP:?} separator")
            }
            Self::UnknownCapacity { entry } => write!(
                f,
                "cell event {entry:?}: capacity not in \
                 [{CAPACITY_SPOT:?}, {CAPACITY_OD:?}, {CAPACITY_ON_DEMAND:?}]"
            ),
            Self::BadEpoch { entry } => {
                write!(
                    f,
                    "cell event {entry:?}: {EPOCH_SEP:?} suffix is not a u64 epoch"
                )
            }
        }
    }
}

impl std::error::Error for CellDecodeError {}

/// Decoded parts of a `"h:cap[@epoch]"` cell event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellEventParts {
    /// The hardware-class half of the cell key.
    pub hw_class: String,
    /// The capacity half of the cell key.
    pub capacity: WireCapacity,
    /// `None` = legacy epoch-less entry (the consumer applies it with
    /// the pre-epoch semantics and leaves its epoch gate untouched).
    pub epoch: Option<EvidenceEpoch>,
}

/// Strict decode of one cell-event string. Total over the grammar:
/// every input is either parts or a typed [`CellDecodeError`] naming
/// the entry — there is no drop lane.
pub fn decode_cell_event(s: &str) -> Result<CellEventParts, CellDecodeError> {
    let (hw_class, rest) =
        s.rsplit_once(CELL_SEP)
            .ok_or_else(|| CellDecodeError::MissingSeparator {
                entry: s.to_string(),
            })?;
    let (capacity, epoch) = match rest.split_once(EPOCH_SEP) {
        None => (rest, None),
        Some((cap, raw)) => {
            let epoch = raw.parse::<u64>().map_err(|_| CellDecodeError::BadEpoch {
                entry: s.to_string(),
            })?;
            (cap, Some(EvidenceEpoch(epoch)))
        }
    };
    let capacity =
        WireCapacity::parse(capacity).ok_or_else(|| CellDecodeError::UnknownCapacity {
            entry: s.to_string(),
        })?;
    Ok(CellEventParts {
        hw_class: hw_class.to_string(),
        capacity,
        epoch,
    })
}

/// Encode one cell event in the canonical wire form
/// (`"h:od"` / `"h:od@123"`). The epoch-less form is byte-identical
/// to the pre-epoch wire (controller `Cell::Display`), so consumers
/// pinned to that form are unaffected by the encoder adoption.
pub fn encode_cell_event(
    hw_class: &str,
    capacity: WireCapacity,
    epoch: Option<EvidenceEpoch>,
) -> String {
    match epoch {
        None => format!("{hw_class}{CELL_SEP}{}", capacity.wire_str()),
        Some(e) => format!("{hw_class}{CELL_SEP}{}{EPOCH_SEP}{e}", capacity.wire_str()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_legacy_forms() {
        let p = decode_cell_event("mid-ebs-x86:spot").unwrap();
        assert_eq!(p.hw_class, "mid-ebs-x86");
        assert_eq!(p.capacity, WireCapacity::Spot);
        assert_eq!(p.epoch, None);
        // Both on-demand tokens decode to the same capacity.
        assert_eq!(
            decode_cell_event("h:od").unwrap().capacity,
            WireCapacity::OnDemand
        );
        assert_eq!(
            decode_cell_event("h:on-demand").unwrap().capacity,
            WireCapacity::OnDemand
        );
        // hw-class names containing ':' are tolerated (the LAST ':'
        // separates) — same law as the crates' rsplit_once codecs.
        let weird = decode_cell_event("weird:name:od").unwrap();
        assert_eq!(weird.hw_class, "weird:name");
        // '@' inside the hw-class segment is NOT an epoch separator.
        let at = decode_cell_event("h@x:spot").unwrap();
        assert_eq!(at.hw_class, "h@x");
        assert_eq!(at.epoch, None);
    }

    #[test]
    fn decode_epoch_suffix() {
        let p = decode_cell_event("mid-ebs-x86:spot@1765432100123").unwrap();
        assert_eq!(p.hw_class, "mid-ebs-x86");
        assert_eq!(p.capacity, WireCapacity::Spot);
        assert_eq!(p.epoch, Some(EvidenceEpoch(1765432100123)));
    }

    #[test]
    fn decode_refusals_are_typed() {
        assert_eq!(
            decode_cell_event("no-colon"),
            Err(CellDecodeError::MissingSeparator {
                entry: "no-colon".into()
            })
        );
        assert_eq!(
            decode_cell_event("h:bogus"),
            Err(CellDecodeError::UnknownCapacity {
                entry: "h:bogus".into()
            })
        );
        assert_eq!(
            decode_cell_event("h:spot@bad"),
            Err(CellDecodeError::BadEpoch {
                entry: "h:spot@bad".into()
            })
        );
        assert_eq!(
            decode_cell_event("h:spot@"),
            Err(CellDecodeError::BadEpoch {
                entry: "h:spot@".into()
            })
        );
        // Epoch BEFORE capacity is not the grammar (suffix only).
        assert_eq!(
            decode_cell_event("h@5:bogus"),
            Err(CellDecodeError::UnknownCapacity {
                entry: "h@5:bogus".into()
            })
        );
    }

    #[test]
    fn encode_decode_round_trip() {
        for (h, cap, e) in [
            ("mid-ebs-x86", WireCapacity::Spot, None),
            ("mid-ebs-x86", WireCapacity::OnDemand, None),
            ("h", WireCapacity::Spot, Some(EvidenceEpoch(0))),
            (
                "weird:name",
                WireCapacity::OnDemand,
                Some(EvidenceEpoch(u64::MAX)),
            ),
        ] {
            let s = encode_cell_event(h, cap, e);
            let p = decode_cell_event(&s).unwrap();
            assert_eq!(p.hw_class, h);
            assert_eq!(p.capacity, cap);
            assert_eq!(p.epoch, e);
        }
        // The epoch-less encode is byte-identical to the pre-epoch
        // wire form (controller `Cell::Display`): "h:od", not
        // "h:on-demand".
        assert_eq!(encode_cell_event("h", WireCapacity::OnDemand, None), "h:od");
    }

    /// Drift tripwire for the surfaces that CANNOT reference these
    /// consts: migration 059/060's `CHECK (capacity_type IN
    /// ('spot','od'))` is checksum-frozen (its side can never move)
    /// and serde attribute literals (`#[serde(alias = "on-demand")]`,
    /// `rename_all = "lowercase"`) are attr-position strings. Each
    /// asserts agreement THROUGH the shared consts: if the canonical
    /// alphabet drifts from the frozen/attr literals, this goes red
    /// naming the law.
    #[test]
    fn frozen_surface_literals_agree_with_alphabet() {
        // Migration 059/060 CHECK vocabulary (frozen .sql).
        assert_eq!(CAPACITY_SPOT, "spot");
        assert_eq!(CAPACITY_OD, "od");
        // Karpenter label vocabulary (serde alias in the scheduler's
        // CapacityType, `karpenter.sh/capacity-type` node label
        // values).
        assert_eq!(CAPACITY_ON_DEMAND, "on-demand");
        // The enum methods expose exactly the consts (no third copy).
        assert_eq!(WireCapacity::Spot.wire_str(), CAPACITY_SPOT);
        assert_eq!(WireCapacity::Spot.karpenter_label(), CAPACITY_SPOT);
        assert_eq!(WireCapacity::OnDemand.wire_str(), CAPACITY_OD);
        assert_eq!(WireCapacity::OnDemand.karpenter_label(), CAPACITY_ON_DEMAND);
    }
}
