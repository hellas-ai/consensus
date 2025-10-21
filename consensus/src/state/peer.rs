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
pub struct PeerSet(pub HashMap<PeerId, BlsPublicKey>);

impl PeerSet {
    pub fn new(peers: Vec<BlsPublicKey>) -> Self {
        Self(peers.into_iter().map(|p| (p.to_peer_id(), p)).collect())
    }
}
