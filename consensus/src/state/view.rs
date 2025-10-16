use rkyv::{Archive, Deserialize, Serialize, with::Skip};

use crate::crypto::{aggregated::BlsPublicKey, conversions::ArkSerdeWrapper};

/// [`View`] represents a view in the consensus protocol.
///
/// A view is a sequence number that advances monotonically.
/// Each view is associated with a leader and a boolean indicating whether the view
/// is currently active or not. Moreover, views can be 'nullified', meaning that
/// the consensus decided to make progress without finalizing any block for the given view.
#[derive(Archive, Deserialize, Serialize)]
pub struct View {
    /// The view number
    #[rkyv(with = Skip)]
    pub view: u64,
    /// The leader's BlsPublicKey of the view
    #[rkyv(with = ArkSerdeWrapper)]
    pub leader: BlsPublicKey,
    /// Whether the view is currently active
    pub is_current: bool,
    /// Whether the view has been nullified.
    /// At the start of a new view, this value is set to false.
    pub nullified: bool,
}

impl View {
    pub fn new(view: u64, leader: BlsPublicKey, is_current: bool, nullified: bool) -> Self {
        Self {
            view,
            leader,
            is_current,
            nullified,
        }
    }

    /// Returns the leader's BlsPublicKey of the view
    #[inline]
    pub fn leader(&self) -> &BlsPublicKey {
        &self.leader
    }

    /// Returns whether the view is currently active
    #[inline]
    pub fn is_current_view(&self) -> bool {
        self.is_current
    }

    /// Returns whether the view has been nullified
    #[inline]
    pub fn is_nullified(&self) -> bool {
        self.nullified
    }

    /// Returns the view number
    #[inline]
    pub fn view(&self) -> u64 {
        self.view
    }
}
