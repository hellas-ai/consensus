//! Peer identity and registry management.

use consensus::crypto::aggregated::PeerId;
use std::collections::HashMap;

/// Registry mapping network identities to consensus peer IDs.
///
/// Uses raw 32-byte public keys since commonware's ed25519::PublicKey
/// has limited construction options from bytes.
pub struct PeerRegistry {
    /// Map from ED25519 public key bytes to BLS peer ID.
    ed25519_to_bls: HashMap<[u8; 32], PeerId>,

    /// Map from BLS peer ID to ED25519 public key bytes.
    bls_to_ed25519: HashMap<PeerId, [u8; 32]>,
}

impl PeerRegistry {
    /// Create a new empty peer registry.
    pub fn new() -> Self {
        Self {
            ed25519_to_bls: HashMap::new(),
            bls_to_ed25519: HashMap::new(),
        }
    }

    /// Register a validator with their ED25519 key bytes and BLS identity.
    pub fn register_validator(&mut self, ed25519_key_bytes: [u8; 32], bls_peer_id: PeerId) {
        self.ed25519_to_bls.insert(ed25519_key_bytes, bls_peer_id);
        self.bls_to_ed25519.insert(bls_peer_id, ed25519_key_bytes);
    }

    /// Look up BLS peer ID from ED25519 public key bytes.
    pub fn get_bls_peer_id(&self, ed25519_key_bytes: &[u8; 32]) -> Option<PeerId> {
        self.ed25519_to_bls.get(ed25519_key_bytes).copied()
    }

    /// Look up ED25519 public key bytes from BLS peer ID.
    pub fn get_ed25519_key(&self, bls_peer_id: PeerId) -> Option<&[u8; 32]> {
        self.bls_to_ed25519.get(&bls_peer_id)
    }

    /// Get all known validator ED25519 key bytes.
    pub fn known_validators(&self) -> impl Iterator<Item = &[u8; 32]> {
        self.ed25519_to_bls.keys()
    }

    /// Get the number of registered validators.
    pub fn validator_count(&self) -> usize {
        self.ed25519_to_bls.len()
    }

    /// Check if a peer is a known validator.
    pub fn is_validator(&self, ed25519_key_bytes: &[u8; 32]) -> bool {
        self.ed25519_to_bls.contains_key(ed25519_key_bytes)
    }
}

impl Default for PeerRegistry {
    fn default() -> Self {
        Self::new()
    }
}
