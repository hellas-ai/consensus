use rkyv::{Archive, Deserialize, Serialize};

use crate::crypto::aggregated::PeerId;

/// [`Leader`] represents a leader in the consensus protocol, for a given view.
///
/// A leader is responsible for proposing a block for a given view. The leader
/// is randomly (but ideally deterministically) selected for each view.
#[derive(Archive, Deserialize, Serialize)]
pub struct Leader {
    /// The leader's BlsPublicKey
    pub peer_id: PeerId,
    /// The view number
    pub view: u64,
}

impl Leader {
    pub fn new(peer_id: PeerId, view: u64) -> Self {
        Self { peer_id, view }
    }

    /// Returns the leader's BlsPublicKey
    #[inline]
    pub fn peer_id(&self) -> PeerId {
        self.peer_id
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
