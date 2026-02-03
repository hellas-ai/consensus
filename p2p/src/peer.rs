//! Peer identity and registry management.
//!
//! Uses an **anonymous RPC model**: validators are explicitly registered,
//! while any non-validator peer is implicitly treated as an RPC node.

use std::collections::HashMap;
use std::sync::Arc;

use arc_swap::ArcSwap;
use consensus::crypto::aggregated::PeerId;

/// Registry mapping network identities to consensus peer IDs.
///
/// Uses raw 32-byte public keys since commonware's ed25519::PublicKey
/// has limited construction options from bytes.
///
/// # Anonymous RPC Model
/// - Validators are explicitly registered with their BLS identities
/// - Any connected peer not in the validator set is treated as an RPC node
/// - Connection limits (max_rpc_connections) are enforced at the network layer
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

    /// Check if a peer is an RPC node (i.e., not a consensus validator node).
    pub fn is_rpc_node(&self, ed25519_key_bytes: &[u8; 32]) -> bool {
        !self.is_validator(ed25519_key_bytes)
    }
}

impl Default for PeerRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Information about a connected peer.
#[derive(Debug, Clone)]
pub struct PeerInfo {
    /// ED25519 public key bytes
    pub ed25519_key: [u8; 32],
    /// Whether this peer is a validator
    pub is_validator: bool,
}

/// Statistics about P2P connections.
#[derive(Debug, Clone, Default)]
pub struct PeerStats {
    /// Number of connected peers
    pub connected_count: u32,
    /// Total validators expected
    pub total_validators: u32,
    /// List of connected peer information
    pub peers: Vec<PeerInfo>,
}

/// Lock-free reader for peer statistics.
///
/// Uses ArcSwap for wait-free reads from the gRPC layer while the
/// P2P thread periodically updates the stats.
#[derive(Clone)]
pub struct PeerStatsReader {
    inner: Arc<ArcSwap<PeerStats>>,
}

impl PeerStatsReader {
    /// Create a new peer stats reader/writer pair.
    pub fn new(total_validators: u32) -> (Self, PeerStatsWriter) {
        let stats = PeerStats {
            connected_count: 0,
            total_validators,
            peers: Vec::new(),
        };
        let shared = Arc::new(ArcSwap::from_pointee(stats));
        (
            Self {
                inner: Arc::clone(&shared),
            },
            PeerStatsWriter { inner: shared },
        )
    }

    /// Load the current stats snapshot.
    pub fn load(&self) -> arc_swap::Guard<Arc<PeerStats>> {
        self.inner.load()
    }
}

/// Writer for peer statistics (used by P2P thread).
pub struct PeerStatsWriter {
    inner: Arc<ArcSwap<PeerStats>>,
}

impl PeerStatsWriter {
    /// Update the peer stats.
    pub fn update(&self, connected_count: u32, peers: Vec<PeerInfo>) {
        let current = self.inner.load();
        let new_stats = PeerStats {
            connected_count,
            total_validators: current.total_validators,
            peers,
        };
        self.inner.store(Arc::new(new_stats));
    }
}
