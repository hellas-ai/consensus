use std::hash::{Hash, Hasher};

use rkyv::{Archive, Deserialize, Serialize};

use crate::{
    crypto::{
        aggregated::{BlsPublicKey, BlsSignature, PeerId},
        conversions::ArkSerdeWrapper,
    },
    state::peer::PeerSet,
};

/// [`Nullify`] represents a nullify message in the consensus protocol.
///
/// A nullify message is a message that is sent by a peer to the network to indicate that the view
/// is nullified, in his local state machine.
/// It contains the view number, the leader's Peer ID, the signature of the nullify message, and the
/// peer's Peer ID.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Nullify {
    /// The view number for which the nullify is being cast
    pub view: u64,
    /// The Peer ID of the leader for the current view
    pub leader_id: PeerId,
    /// The signature of the nullify message
    #[rkyv(with = ArkSerdeWrapper)]
    pub signature: BlsSignature,
    /// The Peer ID of the peer that is sending the nullify message
    pub peer_id: PeerId,
}

impl Nullify {
    pub fn new(view: u64, leader_id: PeerId, signature: BlsSignature, peer_id: PeerId) -> Self {
        Self {
            view,
            leader_id,
            signature,
            peer_id,
        }
    }

    /// Verifies if the nullify message has been successfully signed by the peer with the given
    /// public key. Note: this does not verify that the [`PeerId`] matches the public key
    /// of the peer that signed the nullify message. This should be verified by the caller,
    /// beforehand.
    pub fn verify(&self, public_key: &BlsPublicKey) -> bool {
        let message =
            blake3::hash(&[self.view.to_le_bytes(), self.leader_id.to_le_bytes()].concat());
        public_key.verify(message.as_bytes(), &self.signature)
    }
}

impl PartialEq for Nullify {
    fn eq(&self, other: &Self) -> bool {
        self.view == other.view && self.peer_id == other.peer_id
    }
}

impl Eq for Nullify {}

impl Hash for Nullify {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.view.hash(state);
        self.peer_id.hash(state);
    }
}

/// [`Nullification`] represents a nullification message in the consensus protocol.
///
/// A nullification contains a (2f + 1) number of nullify messages, and is used to finalize a view,
/// as being nullfied, meaning no block can be finalized for that view.
#[derive(Archive, Deserialize, Serialize, Clone, Debug)]
pub struct Nullification<const N: usize, const F: usize, const M_SIZE: usize> {
    /// The view number for which the nullification is being cast
    pub view: u64,
    /// The Peer ID of the leader for the current view
    pub leader_id: PeerId,
    /// The aggregated signature of the nullification by the peers that have nullified the view.
    /// The signature signs the blake3 hash of the view number and the leader's Peer ID.
    #[rkyv(with = ArkSerdeWrapper)]
    pub aggregated_signature: BlsSignature,
    /// The peer IDs of the replicas that have nullified the view
    pub peer_ids: [PeerId; M_SIZE],
}

impl<const N: usize, const F: usize, const M_SIZE: usize> Nullification<N, F, M_SIZE> {
    pub fn new(
        view: u64,
        leader_id: PeerId,
        aggregated_signature: BlsSignature,
        peer_ids: [PeerId; M_SIZE],
    ) -> Self {
        Self {
            view,
            leader_id,
            aggregated_signature,
            peer_ids,
        }
    }

    pub fn verify(&self, peer_set: &PeerSet) -> bool {
        let public_keys = self
            .peer_ids
            .iter()
            .map(|peer_id| {
                peer_set
                    .get_public_key(peer_id)
                    .expect("Peer ID not found in peer set")
                    .clone()
            })
            .collect::<Vec<BlsPublicKey>>();
        let hash = blake3::hash(&[self.view.to_le_bytes(), self.leader_id.to_le_bytes()].concat());
        BlsPublicKey::aggregate(&public_keys).verify(hash.as_bytes(), &self.aggregated_signature)
    }
}
