use rkyv::{Archive, Deserialize, Serialize};

use crate::crypto::{aggregated::BlsPublicKey, conversions::ArkSerdeWrapper};

/// [`Peer`] represents a peer in the consensus protocol.
///
/// It is identified by its public key.
#[derive(Clone, Debug, Archive, Deserialize, Serialize)]
pub struct Peer {
    /// The peer's public key
    #[rkyv(with = ArkSerdeWrapper)]
    pub public_key: BlsPublicKey,
    /// Whether the peer is the current leader
    pub is_current_leader: bool,
}

impl Peer {
    pub fn new(public_key: BlsPublicKey, is_current_leader: bool) -> Self {
        Self {
            public_key,
            is_current_leader,
        }
    }
}
