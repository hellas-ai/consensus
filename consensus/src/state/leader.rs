use rkyv::{Archive, Deserialize, Serialize};

use crate::crypto::{aggregated::BlsPublicKey, conversions::ArkSerdeWrapper};

/// [`Leader`] represents a leader in the consensus protocol, for a given view.
///
/// A leader is responsible for proposing a block for a given view. The leader
/// is randomly (but ideally deterministically) selected for each view.
#[derive(Archive, Deserialize, Serialize)]
pub struct Leader {
    /// The leader's BlsPublicKey
    #[rkyv(with = ArkSerdeWrapper)]
    pub leader: BlsPublicKey,
    /// Whether the leader is currently active
    pub is_current: bool,
    /// The view number
    pub view: u64,
}

impl Leader {
    pub fn new(leader: BlsPublicKey, is_current: bool, view: u64) -> Self {
        Self {
            leader,
            is_current,
            view,
        }
    }

    /// Returns the leader's BlsPublicKey
    #[inline]
    pub fn leader(&self) -> &BlsPublicKey {
        &self.leader
    }

    /// Returns whether the leader is currently active
    #[inline]
    pub fn is_current(&self) -> bool {
        self.is_current
    }

    /// Returns the view number
    #[inline]
    pub fn view(&self) -> u64 {
        self.view
    }

    /// Returns whether the leader is the current leader
    #[inline]
    pub fn is_leader_for_view(&self, view: u64) -> bool {
        self.view == view
    }
}
