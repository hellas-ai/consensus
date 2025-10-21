use std::collections::HashMap;

use rkyv::{Archive, Deserialize, Serialize};

use crate::crypto::{
    aggregated::{BlsPublicKey, PeerId},
    conversions::ArkSerdeWrapper,
};

/// [`Peer`] represents a peer in the consensus protocol.
///
/// It is identified by its public key.
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub struct Peer {
    /// The peer's ID
    pub peer_id: PeerId,
    /// The peer's public key
    #[rkyv(with = ArkSerdeWrapper)]
    pub public_key: BlsPublicKey,
    /// Whether the peer is the current leader
    pub is_current_leader: bool,
}

impl Peer {
    pub fn new(public_key: BlsPublicKey, is_current_leader: bool) -> Self {
        Self {
            peer_id: public_key.to_peer_id(),
            public_key,
            is_current_leader,
        }
    }
}

impl PartialEq for Peer {
    fn eq(&self, other: &Self) -> bool {
        self.peer_id == other.peer_id
    }
}

impl Eq for Peer {}

/// [`PeerSet`] represents a set of peers in the consensus protocol.
///
/// It contains the peers' IDs and public keys.
#[derive(Clone, Debug)]
pub struct PeerSet {
    /// The map of peer IDs to public keys
    pub id_to_public_key: HashMap<PeerId, BlsPublicKey>,
    /// Sorted vector of peer ids
    pub sorted_peer_ids: Vec<PeerId>,
}

impl PeerSet {
    /// Creates a new peer set from a vector of public keys.
    ///
    /// The vector of public keys is expected to be sorted by peer ID.
    ///
    /// The sorting of the peer IDs is done in order to make sure that the order of the
    /// peers in the vector is the same as the order of the peer IDs in the sorted vector.
    /// This is useful for the round-robin leader selection strategy, where the leader is
    /// selected by the index of the replica in the vector of replicas.
    ///
    /// # Panics
    ///
    /// Panics if the input vector of public keys contains either:
    /// - Duplicate public keys
    /// - Distinct public keys with the same peer ID
    pub fn new(peers: Vec<BlsPublicKey>) -> Self {
        let mut id_to_public_key = HashMap::with_capacity(peers.len());
        let mut sorted_peer_ids = Vec::with_capacity(peers.len());
        for peer in peers {
            let peer_id = peer.to_peer_id();
            id_to_public_key.insert(peer_id, peer);
            sorted_peer_ids.push(peer_id);
        }
        sorted_peer_ids.sort();
        sorted_peer_ids.dedup();
        // Make sure there were no duplicates in the input.
        assert_eq!(sorted_peer_ids.len(), id_to_public_key.len());
        Self {
            id_to_public_key,
            sorted_peer_ids,
        }
    }
}
