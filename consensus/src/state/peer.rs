use std::collections::HashMap;

use anyhow::Result;
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
        let original_len = peers.len();
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
        assert_eq!(sorted_peer_ids.len(), original_len);
        Self {
            id_to_public_key,
            sorted_peer_ids,
        }
    }

    /// Gets a public key from a peer ID.
    ///
    /// # Errors
    ///
    /// Returns an error if the peer ID is not found in the peer set.
    pub fn get_public_key(&self, peer_id: &PeerId) -> Result<&BlsPublicKey> {
        if let Some(public_key) = self.id_to_public_key.get(peer_id) {
            Ok(public_key)
        } else {
            Err(anyhow::anyhow!("Peer ID {peer_id} not found in peer set"))
        }
    }

    /// Checks if a peer ID is in the peer set.
    pub fn contains(&self, peer_id: &PeerId) -> bool {
        self.id_to_public_key.contains_key(peer_id)
    }
}

// ... existing code ...

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crypto::aggregated::BlsSecretKey;
    use rand::thread_rng;

    #[test]
    fn test_peer_new() {
        let mut rng = thread_rng();
        let secret_key = BlsSecretKey::generate(&mut rng);
        let public_key = secret_key.public_key();
        let expected_peer_id = public_key.to_peer_id();

        let peer = Peer::new(public_key.clone(), true);

        assert_eq!(peer.peer_id, expected_peer_id);
        assert_eq!(peer.public_key, public_key);
        assert!(peer.is_current_leader);
    }

    #[test]
    fn test_peer_equality() {
        let mut rng = thread_rng();
        let secret_key1 = BlsSecretKey::generate(&mut rng);
        let public_key1 = secret_key1.public_key();

        let secret_key2 = BlsSecretKey::generate(&mut rng);
        let public_key2 = secret_key2.public_key();

        // Same public key should create equal peers
        let peer1 = Peer::new(public_key1.clone(), true);
        let peer2 = Peer::new(public_key1.clone(), false);
        assert_eq!(peer1, peer2);

        // Different public keys should create unequal peers
        let peer3 = Peer::new(public_key2, true);
        assert_ne!(peer1, peer3);
    }

    #[test]
    fn test_peer_set_new_empty() {
        let peer_set = PeerSet::new(vec![]);
        assert!(peer_set.id_to_public_key.is_empty());
        assert!(peer_set.sorted_peer_ids.is_empty());
    }

    #[test]
    fn test_peer_set_new_single_peer() {
        let mut rng = thread_rng();
        let secret_key = BlsSecretKey::generate(&mut rng);
        let public_key = secret_key.public_key();
        let expected_peer_id = public_key.to_peer_id();

        let peer_set = PeerSet::new(vec![public_key.clone()]);

        assert_eq!(peer_set.id_to_public_key.len(), 1);
        assert_eq!(peer_set.sorted_peer_ids.len(), 1);
        assert_eq!(peer_set.sorted_peer_ids[0], expected_peer_id);
        assert_eq!(peer_set.id_to_public_key[&expected_peer_id], public_key);
    }

    #[test]
    fn test_peer_set_new_multiple_peers() {
        let mut rng = thread_rng();
        let mut public_keys = vec![];
        let mut expected_peer_ids = vec![];

        // Generate multiple distinct public keys
        for _ in 0..5 {
            let secret_key = BlsSecretKey::generate(&mut rng);
            let public_key = secret_key.public_key();
            let peer_id = public_key.to_peer_id();
            public_keys.push(public_key);
            expected_peer_ids.push(peer_id);
        }

        let peer_set = PeerSet::new(public_keys.clone());

        assert_eq!(peer_set.id_to_public_key.len(), 5);
        assert_eq!(peer_set.sorted_peer_ids.len(), 5);

        // Check that sorted_peer_ids is actually sorted
        let mut sorted_expected = expected_peer_ids.clone();
        sorted_expected.sort();
        assert_eq!(peer_set.sorted_peer_ids, sorted_expected);

        // Check that all peer IDs are correctly mapped
        for (i, peer_id) in expected_peer_ids.iter().enumerate() {
            assert_eq!(peer_set.id_to_public_key[peer_id], public_keys[i]);
        }
    }

    #[test]
    #[should_panic]
    fn test_peer_set_new_duplicate_public_keys() {
        let mut rng = thread_rng();
        let secret_key = BlsSecretKey::generate(&mut rng);
        let public_key = secret_key.public_key();

        // Try to create PeerSet with duplicate public keys
        let _peer_set = PeerSet::new(vec![public_key.clone(), public_key]);
    }

    #[test]
    fn test_peer_set_get_public_key_existing() {
        let mut rng = thread_rng();
        let secret_key = BlsSecretKey::generate(&mut rng);
        let public_key = secret_key.public_key();
        let peer_id = public_key.to_peer_id();

        let peer_set = PeerSet::new(vec![public_key.clone()]);

        let result = peer_set.get_public_key(&peer_id);
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), &public_key);
    }

    #[test]
    fn test_peer_set_get_public_key_non_existing() {
        let mut rng = thread_rng();
        let secret_key = BlsSecretKey::generate(&mut rng);
        let public_key = secret_key.public_key();

        let peer_set = PeerSet::new(vec![public_key]);

        // Use a peer ID that doesn't exist
        let non_existing_peer_id = 999999999999;
        let result = peer_set.get_public_key(&non_existing_peer_id);
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("not found in peer set")
        );
    }

    #[test]
    fn test_peer_set_get_public_key_multiple_peers() {
        let mut rng = thread_rng();
        let mut public_keys = vec![];
        let mut peer_ids = vec![];

        // Generate multiple distinct public keys
        for _ in 0..3 {
            let secret_key = BlsSecretKey::generate(&mut rng);
            let public_key = secret_key.public_key();
            let peer_id = public_key.to_peer_id();
            public_keys.push(public_key);
            peer_ids.push(peer_id);
        }

        let peer_set = PeerSet::new(public_keys.clone());

        // Test getting each public key by its peer ID
        for (i, peer_id) in peer_ids.iter().enumerate() {
            let result = peer_set.get_public_key(peer_id);
            assert!(result.is_ok());
            assert_eq!(result.unwrap(), &public_keys[i]);
        }
    }
}
